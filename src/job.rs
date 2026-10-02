//! Job orchestration.
//!
//! Phase 1 scope: acquire the lock, stream the archive to storage, release the
//! lock. Container stop/start, hooks and pruning are added in later phases.

use chrono::{DateTime, Utc};
use opendal::Operator;
use tracing::Instrument as _;

use crate::archive::{self, ArchiveOptions, ArchiveStats, Source};
use crate::config::JobConfig;
use crate::error::Result;
use crate::lock::JobLock;
use crate::storage::{self, WriterSink};

/// Outcome of one successful backup run.
#[derive(Debug, Clone, PartialEq, Eq)]
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
/// [`Error::Locked`] when another process holds the job lock,
/// [`Error::Archive`] when a source cannot be read, and [`Error::Storage`] when
/// the backend refuses an operation. Any failure after the upload started
/// removes the partial object, so storage never holds a truncated archive that
/// looks like a valid backup.
pub async fn run_backup(
    op: &Operator,
    job: &JobConfig,
    now: DateTime<Utc>,
) -> Result<BackupOutcome> {
    try_run_backup(op, job, now, &crate::lock::lock_dir()).await
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
) -> Result<BackupOutcome> {
    let started = std::time::Instant::now();
    let _lock = JobLock::try_acquire_in(lock_dir, &job.name)?;
    let span = tracing::info_span!("job", job = %job.name);
    run_backup_locked(op, job, now, started)
        .instrument(span)
        .await
}

/// The pipeline, with the lock already held.
async fn run_backup_locked(
    op: &Operator,
    job: &JobConfig,
    now: DateTime<Utc>,
    started: std::time::Instant,
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

    let sources = sources(job);
    let options = archive_options(job);
    let result = upload(op, backend, &remote, sources, options).await;

    match result {
        Ok((meta, stats)) => {
            tracing::info!(
                object = %remote,
                bytes = meta.content_length(),
                entries = stats.entries,
                elapsed_secs = started.elapsed().as_secs_f64(),
                "backup uploaded"
            );
            Ok(BackupOutcome {
                job: job.name.clone(),
                object: remote,
                size: meta.content_length(),
                stats,
                duration: started.elapsed(),
            })
        }
        Err(err) => {
            // `upload` already cleaned up, but a failure between opening the
            // writer and the first byte needs the same treatment.
            tracing::error!(object = %remote, error = %err, "backup failed");
            Err(err)
        }
    }
}

/// Stream the archive into `remote`, cleaning up on failure.
async fn upload(
    op: &Operator,
    backend: &'static str,
    remote: &str,
    sources: Vec<Source>,
    options: ArchiveOptions,
) -> Result<(opendal::Metadata, ArchiveStats)> {
    let writer = storage::new_writer(op, backend, remote).await?;
    let mut sink = WriterSink::new(writer, backend);

    let stats = match archive::stream_to(sources, &mut sink, options).await {
        Ok(stats) => stats,
        Err(err) => {
            sink.abort().await;
            storage::cleanup_partial(op, backend, remote).await;
            return Err(err);
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
        let outcome = run_backup_locked(&op, &job, now, std::time::Instant::now())
            .await
            .expect("backup");
        assert_eq!(outcome.job, "db");
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

        let err = try_run_backup(&op, &job, Utc::now(), &locks)
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
        let err = try_run_backup(&op, &job, Utc::now(), &locks)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Locked { .. }), "got {err:?}");

        // Once released, the same run succeeds.
        drop(held);
        try_run_backup(&op, &job, Utc::now(), &locks)
            .await
            .expect("backup after release");
    }
}
