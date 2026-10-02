//! Job orchestration.
//!
//! The pipeline, in order:
//!
//! ```text
//! acquire lock
//!  -> pre hooks            (failure aborts; post hooks still run with status=failure)
//!  -> stop containers      (only those that were running)
//!  -> archive + upload     (streamed; abortable by SIGINT/SIGTERM)
//!  -> restore containers   (ALWAYS, before post hooks and prune)
//!  -> post hooks           (filtered by run_on)
//!  -> prune                (only if the upload succeeded)
//! release lock
//! ```

use chrono::{DateTime, Utc};
use opendal::Operator;
use tracing::Instrument as _;

use crate::archive::{self, ArchiveOptions, ArchiveStats, Source};
use crate::config::JobConfig;
use crate::docker::{self, StopGuard};
use crate::error::{Error, Result};
use crate::hooks::{self, Context, Phase, Status};
use crate::lock::JobLock;
use crate::signal::Shutdown;
use crate::storage::{self, WriterSink};

/// Outcome of one backup run that got as far as storing its archive.
///
/// Three steps can fail *after* the archive is safe, and each is reported
/// separately: a failed restart is fatal, a failed post hook or prune only means
/// the run was partial. See [`BackupOutcome::exit_code`].
#[derive(Debug)]
pub struct BackupOutcome {
    /// Job name.
    pub job: String,
    /// Remote path the archive was written to.
    pub object: String,
    /// Compressed size in bytes.
    pub size: u64,
    /// Tar statistics.
    pub stats: ArchiveStats,
    /// How long the whole run took.
    pub duration: std::time::Duration,
    /// Whether every stopped container came back up.
    ///
    /// An error here means containers are still down, which is worse than a
    /// missed rotation and takes priority over the other two.
    pub restore: Result<()>,
    /// What the post hooks did, or why the first one failed.
    pub post: Result<()>,
    /// What retention did afterwards, or why it failed.
    pub prune: Result<crate::retention::PrunePlan>,
}

impl BackupOutcome {
    /// Exit code for a run that stored its archive.
    ///
    /// * `0` — everything succeeded
    /// * `1` — a container was left stopped
    /// * `2` — the archive is stored, but a post hook or pruning failed
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        if self.restore.is_err() {
            crate::error::EXIT_FAILURE
        } else if self.post.is_err() || self.prune.is_err() {
            crate::error::EXIT_PARTIAL
        } else {
            crate::error::EXIT_SUCCESS
        }
    }
}

/// Per-run inputs that are neither the job nor its storage.
///
/// Built by the caller so the library never reaches for process configuration
/// itself: the CLI resolves the Docker client from `[docker].socket`, tests
/// leave it empty.
#[derive(Debug, Default)]
pub struct RunContext {
    /// Fires on SIGINT/SIGTERM so the run can unwind through its cleanup.
    pub shutdown: Shutdown,
    /// Daemon handle. `None` for a job that needs no container control.
    pub client: Option<docker::Client>,
}

impl RunContext {
    /// Build the context for one job, connecting only if it needs Docker.
    ///
    /// # Errors
    ///
    /// When the job needs the daemon and `[docker].socket` is missing or cannot
    /// be opened.
    pub fn for_job(socket: Option<&std::path::Path>, job: &JobConfig) -> Result<Self> {
        Ok(Self {
            shutdown: Shutdown::new(),
            client: docker::client_for(socket, job)?,
        })
    }
}

/// Build the [`Source`] list for a job.
pub fn sources(job: &JobConfig) -> Vec<Source> {
    job.source.iter().cloned().map(Source::from_path).collect()
}

/// Build the [`ArchiveOptions`] for a job.
#[must_use]
pub fn archive_options(job: &JobConfig) -> ArchiveOptions {
    ArchiveOptions {
        compression: job.compression,
        follow_symlinks: job.follow_symlinks,
    }
}

/// Run one backup for `job` at `now`.
///
/// The lock is taken first so a concurrent `docker exec dvb backup` or a
/// scheduler fire cannot overlap.
///
/// # Errors
///
/// [`Error::Locked`] when another process holds the job lock, [`Error::Archive`]
/// when a source cannot be read, [`Error::Storage`] when the backend refuses an
/// operation, [`Error::HookFailed`] when a pre hook fails, and
/// [`Error::Cancelled`] when a shutdown signal arrives. Containers stopped for
/// the run are restarted on every one of those paths before the error returns,
/// and any failure after the upload started removes the partial object.
pub async fn run_backup(
    op: &Operator,
    job: &JobConfig,
    now: DateTime<Utc>,
    run: &RunContext,
) -> Result<BackupOutcome> {
    try_run_backup(op, job, now, &crate::lock::lock_dir(), run).await
}

/// [`run_backup`] taking the lock from an explicit directory.
///
/// The scheduler needs to distinguish "another process holds the lock, skip this
/// fire" from a real failure, which this reports as [`Error::Locked`].
///
/// # Errors
///
/// As [`run_backup`], plus [`Error::Io`] when the lock directory or file cannot
/// be opened.
pub async fn try_run_backup(
    op: &Operator,
    job: &JobConfig,
    now: DateTime<Utc>,
    lock_dir: &std::path::Path,
    run: &RunContext,
) -> Result<BackupOutcome> {
    let started = std::time::Instant::now();
    let _lock = JobLock::try_acquire_in(lock_dir, &job.name)?;
    let span = tracing::info_span!("job", job = %job.name);
    run_backup_locked(op, job, now, started, run)
        .instrument(span)
        .await
}

/// The pipeline, with the lock already held.
async fn run_backup_locked(
    op: &Operator,
    job: &JobConfig,
    now: DateTime<Utc>,
    started: std::time::Instant,
    run: &RunContext,
) -> Result<BackupOutcome> {
    let backend = job.storage.kind();
    let object = job.object_name(now);
    let remote = job.storage.remote_path(&object);

    tracing::info!(
        object = %remote,
        compression = ?job.compression,
        sources = job.source.len(),
        "starting backup"
    );

    let client = run.client.as_ref();
    // Nothing is stopped yet, so a failure before step 2 has an empty guard.
    let mut guard: Option<StopGuard> = None;

    let hooks_ctx = |status, error| Context {
        job: &job.name,
        status,
        archive: &remote,
        error,
    };

    // 1. Pre hooks. A failure here means nothing has been stopped, and the post
    //    hooks still get their turn with status=failure.
    let pending = hooks_ctx(Status::Pending, "");
    if let Err(err) = hooks::run(Phase::Pre, &job.pre, &pending, client).await {
        tracing::error!(error = %err, "pre hook failed; the backup was not taken");
        let failed_at = err.to_string();
        abort(job, &remote, &mut guard, client, &failed_at).await;
        return Err(err);
    }

    // 2. Stop the containers this job selects, recording which were running.
    if let Some(handle) = client.cloned() {
        match StopGuard::stop(handle, job).await {
            Ok(stopped) => guard = Some(stopped),
            Err(err) => {
                tracing::error!(error = %err, "could not stop the job's containers");
                let failed_at = err.to_string();
                abort(job, &remote, &mut guard, client, &failed_at).await;
                return Err(err);
            }
        }
    }

    // 3. Archive and upload.
    let uploaded = upload(
        op,
        backend,
        &remote,
        sources(job),
        archive_options(job),
        &run.shutdown,
    )
    .await;

    // 4. Restart everything. ALWAYS, and before the post hooks and pruning:
    //    a post hook may target the very containers that were stopped.
    let restore = restore(&mut guard).await;

    // 5. Post hooks, filtered by run_on.
    let (hook_status, error) = match &uploaded {
        Ok(_) => (Status::Success, String::new()),
        Err(err) => (Status::Failure, err.to_string()),
    };
    let post = hooks::run(
        Phase::Post,
        &job.post,
        &hooks_ctx(hook_status, &error),
        client,
    )
    .await;
    if let Err(err) = &post {
        tracing::error!(error = %err, "a post hook failed after the backup");
    }

    let (meta, archive_stats) = match uploaded {
        Ok(written) => written,
        Err(err) => {
            tracing::error!(object = %remote, error = %err, "backup failed");
            return Err(err);
        }
    };

    tracing::info!(
        object = %remote,
        bytes = meta.content_length(),
        entries = archive_stats.entries,
        elapsed_secs = started.elapsed().as_secs_f64(),
        "backup uploaded"
    );

    // 6. Prune only now that the new backup is safely stored, so a failed
    //    upload can never delete the backup it would have replaced.
    let prune = prune_after_upload(op, job, now).await;

    Ok(BackupOutcome {
        job: job.name.clone(),
        object: remote,
        size: meta.content_length(),
        stats: archive_stats,
        duration: started.elapsed(),
        restore,
        post,
        prune,
    })
}

/// Apply retention once the new archive is in storage.
///
/// A failure here does not invalidate the backup, so it is logged and reported
/// through [`BackupOutcome::prune`] for the caller to turn into exit code 2.
async fn prune_after_upload(
    op: &Operator,
    job: &JobConfig,
    now: DateTime<Utc>,
) -> Result<crate::retention::PrunePlan> {
    match crate::retention::prune(op, job, now, false).await {
        Ok(plan) => {
            if plan.delete_count() > 0 {
                tracing::info!(deleted = plan.delete_count(), "pruned old backups");
            }
            Ok(plan)
        }
        Err(err) => {
            tracing::error!(error = %err, "prune failed after a successful backup");
            Err(err)
        }
    }
}

/// Undo whatever has already happened and report the failure through the post
/// hooks. Shared by the paths that fail before the archive is attempted.
async fn abort(
    job: &JobConfig,
    remote: &str,
    guard: &mut Option<StopGuard>,
    client: Option<&docker::Client>,
    reason: &str,
) {
    // `restore` logs its own failure; the caller is already returning the error
    // that caused the abort, which is the more useful one.
    let _ = restore(guard).await;
    let ctx = Context {
        job: &job.name,
        status: Status::Failure,
        archive: remote,
        error: reason,
    };
    if let Err(err) = hooks::run(Phase::Post, &job.post, &ctx, client).await {
        tracing::error!(error = %err, "a post hook failed after an aborted backup");
    }
}

/// Take the guard and restart what it holds, leaving nothing behind to restore.
///
/// A failure is logged here rather than at each call site so no path can forget
/// to mention that containers are still down.
async fn restore(guard: &mut Option<StopGuard>) -> Result<()> {
    let stopped = guard.take();
    let Some(stopped) = stopped else {
        return Ok(());
    };
    match stopped.restore().await {
        Ok(()) => Ok(()),
        Err(err) => {
            tracing::error!(
                error = %err,
                "containers may still be stopped; check them by hand"
            );
            Err(err)
        }
    }
}

/// Stream the archive into `remote`, cleaning up on failure.
///
/// A shutdown signal abandons the stream the same way an error does: the
/// archiver is dropped, the partial object is removed, and the caller is told
/// which signal asked for it.
async fn upload(
    op: &Operator,
    backend: &'static str,
    remote: &str,
    sources: Vec<Source>,
    options: ArchiveOptions,
    shutdown: &Shutdown,
) -> Result<(opendal::Metadata, ArchiveStats)> {
    let writer = storage::new_writer(op, backend, remote).await?;
    let mut sink = WriterSink::new(writer, backend);

    // Scoped so the stream's borrow of `sink` ends before `abort` takes it.
    // `biased` puts the shutdown first: once a signal has fired it wins over
    // any chunk the archiver happens to be ready with.
    let streamed = {
        let stream = archive::stream_to(sources, &mut sink, options);
        tokio::pin!(stream);
        tokio::select! {
            biased;
            () = shutdown.cancelled() => None,
            result = &mut stream => Some(result),
        }
    };

    let stats = match streamed {
        Some(Ok(stats)) => stats,
        Some(Err(err)) => {
            sink.abort().await;
            storage::cleanup_partial(op, backend, remote).await;
            return Err(err);
        }
        None => {
            // The archiver is gone; drop its buffers before touching storage.
            sink.abort().await;
            storage::cleanup_partial(op, backend, remote).await;
            return Err(Error::Cancelled {
                signal: shutdown
                    .signal()
                    .map_or("shutdown", crate::signal::Signal::as_str),
            });
        }
    };

    let meta = match sink.close().await {
        Ok(meta) => meta,
        Err(err) => {
            storage::cleanup_partial(op, backend, remote).await;
            return Err(err);
        }
    };

    Ok((meta, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Compression, FsConfig, StorageConfig};
    use crate::error::Error;

    fn job(root: &std::path::Path, filename: &str) -> JobConfig {
        JobConfig {
            name: "db".to_owned(),
            cron: Some("0 3 * * *".to_owned()),
            source: vec![root.join("data")],
            filename: filename.to_owned(),
            compression: Compression::Zstd,
            retention_days: 14,
            min_keep: 3,
            stop_containers: vec![],
            stop_label: None,
            stop_timeout_secs: 30,
            follow_symlinks: false,
            storage: StorageConfig::Fs(FsConfig {
                root: root.to_path_buf(),
                prefix: "backups".to_owned(),
            }),
            pre: vec![],
            post: vec![],
            run_on_start: false,
            restore: None,
        }
    }

    fn build_tree(root: &std::path::Path) {
        std::fs::create_dir_all(root.join("data")).unwrap_or_else(|e| panic!("mkdir: {e}"));
        std::fs::write(root.join("data/a.txt"), b"hello").unwrap_or_else(|e| panic!("write: {e}"));
    }

    #[test]
    fn sources_are_built_from_the_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let job = job(tmp.path(), "db-%Y%m%dT%H%M%SZ");
        let sources = sources(&job);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "data");
        assert_eq!(sources[0].path, tmp.path().join("data"));
    }

    #[test]
    fn archive_options_follow_the_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut job = job(tmp.path(), "db-%Y%m%dT%H%M%SZ");
        job.compression = Compression::Gzip;
        job.follow_symlinks = true;
        let options = archive_options(&job);
        assert_eq!(options.compression, Compression::Gzip);
        assert!(options.follow_symlinks);
    }

    #[tokio::test]
    async fn backup_writes_an_object_to_storage() {
        let tmp = tempfile::tempdir().expect("tempdir");
        build_tree(tmp.path());

        let job = job(tmp.path(), "db-%Y%m%dT%H%M%SZ");
        let op = storage::operator(&job.storage).expect("operator");
        let now = Utc::now();

        let lock = JobLock::try_acquire_in(&tmp.path().join("locks"), &job.name).expect("lock");
        let outcome = run_backup_locked(
            &op,
            &job,
            now,
            std::time::Instant::now(),
            &RunContext::default(),
        )
        .await
        .expect("backup");
        assert_eq!(outcome.job, "db");
        assert_eq!(outcome.exit_code(), crate::error::EXIT_SUCCESS);
        assert!(
            outcome.object.starts_with("backups/db-"),
            "{}",
            outcome.object
        );
        // The template has no extension, so the zstd one is appended once.
        let ext = Compression::Zstd
            .extension()
            .map_or_else(String::new, |ext| format!(".{ext}"));
        assert!(
            outcome.object.ends_with(&ext),
            "{} should end with {ext}",
            outcome.object
        );
        assert!(
            !outcome.object.ends_with(&ext.repeat(2)),
            "extension appended twice: {}",
            outcome.object
        );
        assert!(outcome.size > 0);
        assert!(outcome.stats.entries >= 2);

        let listed = storage::list_prefix(&op, "fs", "backups")
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, outcome.object);
        drop(lock);
    }

    #[tokio::test]
    async fn a_failing_source_leaves_no_object_behind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let locks = tmp.path().join("locks");

        // No source tree on disk at all, so archiving fails immediately.
        let job = job(tmp.path(), "db-%Y%m%dT%H%M%SZ");
        let op = storage::operator(&job.storage).expect("operator");

        let err = try_run_backup(&op, &job, Utc::now(), &locks, &RunContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Archive { .. }), "got {err:?}");

        let listed = storage::list_prefix(&op, "fs", "backups")
            .await
            .expect("list");
        assert!(listed.is_empty(), "partial object left behind: {listed:?}");
    }

    #[tokio::test]
    async fn a_second_concurrent_run_fails_on_the_lock() {
        let tmp = tempfile::tempdir().expect("tempdir");
        build_tree(tmp.path());
        let locks = tmp.path().join("locks");

        let job = job(tmp.path(), "db-%Y%m%dT%H%M%SZ");
        let op = storage::operator(&job.storage).expect("operator");

        // A separate `docker exec dvb backup` holds the lock.
        let held = JobLock::try_acquire_in(&locks, "db").expect("hold the lock");
        let err = try_run_backup(&op, &job, Utc::now(), &locks, &RunContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Locked { .. }), "got {err:?}");

        // Once released, the same run succeeds.
        drop(held);
        try_run_backup(&op, &job, Utc::now(), &locks, &RunContext::default())
            .await
            .expect("backup after release");
    }
}
