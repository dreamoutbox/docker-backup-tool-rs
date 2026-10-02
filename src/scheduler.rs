//! Scheduler daemon for scheduled backup jobs.
//!
//! Evaluates cron schedules in the process's `TZ` timezone, sleeps until each
//! fire time, and executes backups. If a job's lock is already held (by a
//! previous run or an external `dvb backup`), the scheduled run is skipped with
//! a warning (no queueing).
//!
//! On SIGINT/SIGTERM, the scheduler stops scheduling new runs, waits up to
//! `shutdown_grace_secs` for running jobs to finish, and only cancels remaining
//! jobs if the grace period expires.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use opendal::Operator;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::config::{self, Config, JobConfig};
use crate::error::{ConfigError, EXIT_SUCCESS, Error, Result};
use crate::job::{self, RunContext};
use crate::signal::{Shutdown, Signal};
use crate::storage;

/// Resolve the timezone for cron scheduling from `TZ` environment variable.
///
/// Defaults to UTC when `TZ` is unset or empty.
///
/// # Errors
///
/// Returns [`Error::Config`] if `TZ` is set but cannot be parsed as an IANA
/// timezone name.
pub fn resolve_timezone() -> Result<Tz> {
    resolve_timezone_from(std::env::var("TZ").ok().as_deref())
}

/// Parse an optional timezone string, defaulting to UTC when absent or empty.
///
/// # Errors
///
/// Returns [`Error::Config`] if `tz_str` cannot be parsed as an IANA timezone name.
pub fn resolve_timezone_from(tz_env: Option<&str>) -> Result<Tz> {
    match tz_env {
        Some(tz_str) => crate::config::parse_timezone(tz_str),
        None => Ok(chrono_tz::UTC),
    }
}

/// Calculate the next fire time in UTC for a cron expression evaluated in `tz`.
///
/// Ensures the returned fire time is strictly after `after_slot` (if provided)
/// and strictly after `now`.
///
/// # Errors
///
/// Returns [`Error::Config`] if `croner` fails to calculate the next occurrence.
pub fn next_fire_time(
    cron: &croner::Cron,
    tz: &Tz,
    now: DateTime<Utc>,
    after_slot: Option<DateTime<Utc>>,
) -> Result<DateTime<Utc>> {
    let now_tz = now.with_timezone(tz);
    let start_tz = match after_slot {
        Some(after) => {
            let after_tz = after.with_timezone(tz);
            if after_tz > now_tz { after_tz } else { now_tz }
        }
        None => now_tz,
    };

    let next_tz = cron.find_next_occurrence(&start_tz, false).map_err(|err| {
        Error::Config(ConfigError::Invalid(format!(
            "failed to calculate next run for cron: {err}"
        )))
    })?;

    let mut next_utc = next_tz.with_timezone(&Utc);

    // Prevent re-firing the same slot if clock jumps backward or DST repeats.
    if let Some(after) = after_slot.filter(|&after| next_utc <= after) {
        let after_tz = after.with_timezone(tz);
        let next_after_tz = cron.find_next_occurrence(&after_tz, false).map_err(|err| {
            Error::Config(ConfigError::Invalid(format!(
                "failed to calculate next run for cron: {err}"
            )))
        })?;
        next_utc = next_after_tz.with_timezone(&Utc);
    }

    Ok(next_utc)
}

/// Sleep until `target_utc` or until `stop_token` is cancelled.
///
/// Returns `true` if target time was reached, `false` if cancelled early.
pub async fn sleep_until_utc(target_utc: DateTime<Utc>, stop_token: &CancellationToken) -> bool {
    let now = Utc::now();
    let delay = if target_utc > now {
        (target_utc - now).to_std().unwrap_or(Duration::ZERO)
    } else {
        Duration::ZERO
    };

    let deadline = tokio::time::Instant::now() + delay;
    tokio::select! {
        () = stop_token.cancelled() => false,
        () = tokio::time::sleep_until(deadline) => true,
    }
}

/// Track concurrently executing jobs for graceful shutdown.
#[derive(Clone, Default)]
pub struct ActiveJobs {
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl ActiveJobs {
    /// Create a new tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a job starting execution.
    #[must_use]
    pub fn enter(&self) -> JobGuard {
        self.count.fetch_add(1, Ordering::SeqCst);
        JobGuard {
            tracker: self.clone(),
        }
    }

    /// Number of jobs currently running.
    #[must_use]
    pub fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// Wait until all tracked jobs have exited.
    pub async fn wait_for_empty(&self) {
        loop {
            let notified = self.notify.notified();
            if self.count.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// RAII guard that decrements active job count on drop.
pub struct JobGuard {
    tracker: ActiveJobs,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        self.tracker.count.fetch_sub(1, Ordering::SeqCst);
        self.tracker.notify.notify_waiters();
    }
}

/// Execute a single backup iteration for a job.
pub async fn run_job_task(
    op: &Operator,
    job: &JobConfig,
    fire_time: DateTime<Utc>,
    lock_dir: &Path,
    docker_socket: Option<&Path>,
    job_shutdown: &Shutdown,
) {
    let run_ctx = match RunContext::for_job(docker_socket, job) {
        Ok(mut ctx) => {
            ctx.shutdown = job_shutdown.clone();
            ctx
        }
        Err(err) => {
            tracing::error!(job = %job.name, error = %err, "cannot build run context for job");
            return;
        }
    };

    match job::try_run_backup(op, job, fire_time, lock_dir, &run_ctx).await {
        Ok(outcome) => {
            if outcome.exit_code() == EXIT_SUCCESS {
                tracing::info!(
                    job = %job.name,
                    object = %outcome.object,
                    size = outcome.size,
                    duration_secs = outcome.duration.as_secs_f64(),
                    "scheduled job succeeded"
                );
            } else {
                tracing::warn!(
                    job = %job.name,
                    exit_code = outcome.exit_code(),
                    "scheduled job completed with warnings or partial failures"
                );
            }
        }
        Err(Error::Locked { path, .. }) => {
            tracing::warn!(
                job = %job.name,
                lock_file = %path.display(),
                "job lock is held; skipping this scheduled run"
            );
        }
        Err(Error::Cancelled { signal }) => {
            tracing::warn!(
                job = %job.name,
                signal,
                "scheduled job was cancelled by shutdown"
            );
        }
        Err(err) => {
            tracing::error!(
                job = %job.name,
                error = %err,
                "scheduled job failed"
            );
        }
    }
}

/// Main entry point for `dvb run`.
///
/// Installs signal handlers and starts the scheduler with the default lock directory.
///
/// # Errors
///
/// Returns an error if the timezone cannot be resolved, an operator cannot be
/// configured, or an unrecoverable failure occurs during startup.
pub async fn run(config: Config) -> Result<u8> {
    let daemon_shutdown = Shutdown::new();
    daemon_shutdown.install();
    run_scheduler(config, crate::lock::lock_dir(), daemon_shutdown).await
}

fn log_scheduled_jobs(jobs: &[JobConfig], tz: Tz) -> Result<()> {
    tracing::info!(
        jobs = jobs.len(),
        tz = %tz.name(),
        "starting scheduler daemon"
    );

    for job in jobs {
        let job_tz = job.effective_timezone(tz);
        if let Some(cron_str) = &job.cron {
            let cron = config::parse_cron(cron_str).map_err(|err| {
                Error::Config(ConfigError::Invalid(format!(
                    "job `{}`: invalid cron `{cron_str}`: {err}",
                    job.name
                )))
            })?;
            let next = next_fire_time(&cron, &job_tz, Utc::now(), None)?;
            if let Some(crate::config::ScheduleSource::Crontext(orig)) = &job.schedule_source {
                tracing::info!(
                    job = %job.name,
                    cron = %cron_str,
                    crontext = %orig,
                    tz = %job_tz.name(),
                    next_run = %next.to_rfc3339(),
                    run_on_start = job.run_on_start,
                    "scheduled job"
                );
            } else {
                tracing::info!(
                    job = %job.name,
                    cron = %cron_str,
                    tz = %job_tz.name(),
                    next_run = %next.to_rfc3339(),
                    run_on_start = job.run_on_start,
                    "scheduled job"
                );
            }
        } else {
            tracing::info!(
                job = %job.name,
                run_on_start = job.run_on_start,
                "job has no cron schedule; manual trigger only"
            );
        }
    }
    Ok(())
}

async fn wait_for_graceful_shutdown(
    active_jobs: &ActiveJobs,
    job_shutdown: &Shutdown,
    grace_secs: u64,
    signal: Signal,
) {
    let running = active_jobs.count();
    if running == 0 {
        tracing::info!("no jobs running; shutdown complete");
        return;
    }

    tracing::info!(
        running,
        grace_secs,
        "waiting for running jobs to finish gracefully"
    );

    let grace_duration = Duration::from_secs(grace_secs);
    tokio::select! {
        () = active_jobs.wait_for_empty() => {
            tracing::info!("all jobs finished gracefully");
        }
        () = tokio::time::sleep(grace_duration) => {
            tracing::warn!(
                grace_secs,
                "grace period expired; cancelling remaining running jobs"
            );
            job_shutdown.cancel(signal);
            // Allow cancelled jobs to finish their unwinding cleanup.
            active_jobs.wait_for_empty().await;
            tracing::info!("all jobs cleaned up and stopped");
        }
    }
}

/// Run the scheduler with an explicit lock directory and shutdown trigger.
///
/// Used directly by integration tests to inject custom paths and simulate signals.
///
/// # Errors
///
/// Returns an error if the timezone cannot be resolved or an operator cannot be
/// configured.
pub async fn run_scheduler(
    config: Config,
    lock_dir: PathBuf,
    daemon_shutdown: Shutdown,
) -> Result<u8> {
    let tz = resolve_timezone()?;

    // 1. Build and verify operators for each job at startup.
    let mut operators = Vec::with_capacity(config.jobs.len());
    for job in &config.jobs {
        let op = storage::operator(&job.storage)?;
        operators.push(op);
    }

    // 2. Startup log: list all jobs with their next run time (Phase 4 Task 3).
    log_scheduled_jobs(&config.jobs, tz)?;

    let stop_scheduling = CancellationToken::new();
    let job_shutdown = Shutdown::new();
    let active_jobs = ActiveJobs::new();

    // 3. Spawn scheduler tasks for each configured job.
    let docker_socket = config.docker.socket.clone();

    for (job, op) in config.jobs.into_iter().zip(operators) {
        let job_tz = job.effective_timezone(tz);
        let ctx = JobSchedulerContext {
            job: Arc::new(job),
            op: Arc::new(op),
            tz: job_tz,
            lock_dir: lock_dir.clone(),
            docker_socket: docker_socket.clone(),
            stop_token: stop_scheduling.clone(),
            job_shutdown: job_shutdown.clone(),
            active_jobs: active_jobs.clone(),
        };

        tokio::spawn(async move {
            ctx.run().await;
        });
    }

    // 4. Wait for shutdown signal (SIGTERM/SIGINT).
    daemon_shutdown.cancelled().await;
    let signal = daemon_shutdown.signal().unwrap_or(Signal::Terminate);
    tracing::info!(
        signal = signal.as_str(),
        "shutdown signal received; stopping scheduler"
    );

    // Stop scheduling any new runs.
    stop_scheduling.cancel();

    // 5. Graceful shutdown: wait for running jobs up to shutdown_grace_secs.
    wait_for_graceful_shutdown(
        &active_jobs,
        &job_shutdown,
        config.shutdown_grace_secs,
        signal,
    )
    .await;

    Ok(EXIT_SUCCESS)
}

/// Context for running a single job's schedule.
struct JobSchedulerContext {
    job: Arc<JobConfig>,
    op: Arc<Operator>,
    tz: Tz,
    lock_dir: PathBuf,
    docker_socket: Option<PathBuf>,
    stop_token: CancellationToken,
    job_shutdown: Shutdown,
    active_jobs: ActiveJobs,
}

impl JobSchedulerContext {
    /// Loop managing the schedule and execution of one job.
    async fn run(self) {
        // Run once at start if configured.
        if self.job.run_on_start {
            tracing::info!(job = %self.job.name, "executing run_on_start");
            let guard = self.active_jobs.enter();
            let op = Arc::clone(&self.op);
            let job = Arc::clone(&self.job);
            let lock_dir = self.lock_dir.clone();
            let socket = self.docker_socket.clone();
            let shutdown = self.job_shutdown.clone();

            tokio::spawn(async move {
                let _guard = guard;
                run_job_task(
                    &op,
                    &job,
                    Utc::now(),
                    &lock_dir,
                    socket.as_deref(),
                    &shutdown,
                )
                .await;
            });
        }

        let Some(cron_str) = &self.job.cron else {
            return;
        };

        let cron = match config::parse_cron(cron_str) {
            Ok(c) => c,
            Err(err) => {
                tracing::error!(job = %self.job.name, error = %err, "invalid cron pattern in scheduler loop");
                return;
            }
        };

        let mut last_fire_slot: Option<DateTime<Utc>> = None;

        loop {
            if self.stop_token.is_cancelled() {
                break;
            }

            let next_fire = match next_fire_time(&cron, &self.tz, Utc::now(), last_fire_slot) {
                Ok(time) => time,
                Err(err) => {
                    tracing::error!(job = %self.job.name, error = %err, "cannot compute next fire time");
                    break;
                }
            };

            if !sleep_until_utc(next_fire, &self.stop_token).await {
                // Cancelled during sleep.
                break;
            }

            last_fire_slot = Some(next_fire);

            // Spawn run_job per fire.
            let guard = self.active_jobs.enter();
            let op = Arc::clone(&self.op);
            let job = Arc::clone(&self.job);
            let lock_dir = self.lock_dir.clone();
            let socket = self.docker_socket.clone();
            let shutdown = self.job_shutdown.clone();

            tokio::spawn(async move {
                let _guard = guard;
                run_job_task(
                    &op,
                    &job,
                    next_fire,
                    &lock_dir,
                    socket.as_deref(),
                    &shutdown,
                )
                .await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Compression, FsConfig, StorageConfig};
    use chrono::Duration as ChronoDuration;

    fn test_job(name: &str, cron: Option<&str>, run_on_start: bool) -> JobConfig {
        JobConfig {
            name: name.to_owned(),
            cron: cron.map(str::to_owned),
            crontext: None,
            timezone: None,
            schedule_source: cron.map(|_| crate::config::ScheduleSource::Cron),
            source: vec![PathBuf::from("/tmp")],
            filename: "backup-%Y%m%dT%H%M%SZ".to_owned(),
            compression: Compression::None,
            retention_days: 14,
            min_keep: 3,
            stop_containers: vec![],
            stop_label: None,
            stop_timeout_secs: 30,
            follow_symlinks: false,
            storage: StorageConfig::Fs(FsConfig {
                root: PathBuf::from("/tmp"),
                prefix: "test".to_owned(),
            }),
            pre: vec![],
            post: vec![],
            run_on_start,
            restore: None,
        }
    }

    #[test]
    fn timezone_defaults_to_utc() {
        assert_eq!(resolve_timezone_from(None).expect("tz").name(), "UTC");
        assert_eq!(resolve_timezone_from(Some("")).expect("tz").name(), "UTC");
        assert_eq!(
            resolve_timezone_from(Some("America/New_York"))
                .expect("tz")
                .name(),
            "America/New_York"
        );
        assert_eq!(
            resolve_timezone_from(Some(":UTC")).expect("tz").name(),
            "UTC"
        );
    }

    #[test]
    fn next_fire_time_finds_future_slot() {
        let cron = config::parse_cron("0 * * * *").expect("parse cron");
        let tz = chrono_tz::UTC;
        let now = Utc::now();
        let next = next_fire_time(&cron, &tz, now, None).expect("next fire");
        assert!(next > now);
    }

    #[test]
    fn next_fire_time_never_repeats_the_same_slot() {
        let cron = config::parse_cron("* * * * *").expect("parse cron");
        let tz = chrono_tz::UTC;
        let now = Utc::now();
        let first = next_fire_time(&cron, &tz, now, None).expect("first fire");
        // Even if now is identical or earlier (clock jump backward), after_slot prevents repeating
        let fake_past = first - ChronoDuration::seconds(30);
        let second = next_fire_time(&cron, &tz, fake_past, Some(first)).expect("second fire");
        assert!(second > first);
    }

    #[tokio::test]
    async fn active_jobs_tracker_waits_until_empty() {
        let tracker = ActiveJobs::new();
        assert_eq!(tracker.count(), 0);

        let guard1 = tracker.enter();
        let guard2 = tracker.enter();
        assert_eq!(tracker.count(), 2);

        let tracker_clone = tracker.clone();
        let handle = tokio::spawn(async move {
            tracker_clone.wait_for_empty().await;
        });

        drop(guard1);
        assert_eq!(tracker.count(), 1);
        drop(guard2);
        assert_eq!(tracker.count(), 0);

        tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("timeout")
            .expect("join handle");
    }

    #[tokio::test]
    async fn idle_daemon_shuts_down_quickly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let job = test_job("test_idle", Some("0 0 1 1 *"), false);
        let config = Config {
            docker: crate::config::DockerConfig::default(),
            shutdown_grace_secs: 60,
            jobs: vec![job],
        };

        let shutdown = Shutdown::new();
        let shutdown_clone = shutdown.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            shutdown_clone.cancel(Signal::Terminate);
        });

        let start = std::time::Instant::now();
        let result = run_scheduler(config, tmp.path().to_path_buf(), shutdown).await;
        assert_eq!(result.expect("run scheduler"), EXIT_SUCCESS);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn running_job_is_given_grace_period() {
        let _tmp = tempfile::tempdir().expect("tempdir");
        let tracker = ActiveJobs::new();
        let guard = tracker.enter();

        let shutdown = Shutdown::new();
        let shutdown_clone = shutdown.clone();

        // Release the job during the grace period (at 100ms, grace period is 2s)
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(guard);
        });

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            shutdown_clone.cancel(Signal::Terminate);
        });

        let job_shutdown = Shutdown::new();
        shutdown.cancelled().await;

        let grace_duration = Duration::from_secs(2);
        let completed = tokio::select! {
            () = tracker.wait_for_empty() => true,
            () = tokio::time::sleep(grace_duration) => {
                job_shutdown.cancel(Signal::Terminate);
                false
            }
        };

        assert!(completed, "job should finish within grace period");
        assert!(
            job_shutdown.signal().is_none(),
            "job was not forcibly cancelled"
        );
    }

    #[tokio::test]
    async fn grace_period_expires_and_cancels_job() {
        let tracker = ActiveJobs::new();
        let _guard = tracker.enter();

        let shutdown = Shutdown::new();
        shutdown.cancel(Signal::Terminate);

        let job_shutdown = Shutdown::new();
        shutdown.cancelled().await;

        let grace_duration = Duration::from_millis(50);
        let completed = tokio::select! {
            () = tracker.wait_for_empty() => true,
            () = tokio::time::sleep(grace_duration) => {
                job_shutdown.cancel(Signal::Terminate);
                false
            }
        };

        assert!(!completed, "job exceeded grace period");
        assert_eq!(
            job_shutdown.signal(),
            Some(Signal::Terminate),
            "job was cancelled after grace period"
        );
    }

    #[tokio::test]
    async fn two_jobs_fire_independently() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src1 = tmp.path().join("src1");
        let src2 = tmp.path().join("src2");
        std::fs::create_dir_all(&src1).expect("create src1");
        std::fs::create_dir_all(&src2).expect("create src2");
        std::fs::write(src1.join("file1.txt"), b"job1").expect("write file1");
        std::fs::write(src2.join("file2.txt"), b"job2").expect("write file2");

        let mut job1 = test_job("job1", None, true);
        job1.source = vec![src1];
        job1.storage = StorageConfig::Fs(FsConfig {
            root: tmp.path().join("backups"),
            prefix: "job1".to_owned(),
        });

        let mut job2 = test_job("job2", None, true);
        job2.source = vec![src2];
        job2.storage = StorageConfig::Fs(FsConfig {
            root: tmp.path().join("backups"),
            prefix: "job2".to_owned(),
        });

        let config = Config {
            docker: crate::config::DockerConfig::default(),
            shutdown_grace_secs: 5,
            jobs: vec![job1, job2],
        };

        let shutdown = Shutdown::new();
        let shutdown_clone = shutdown.clone();

        tokio::spawn(async move {
            // Let both jobs execute their run_on_start then request shutdown
            tokio::time::sleep(Duration::from_millis(200)).await;
            shutdown_clone.cancel(Signal::Terminate);
        });

        let result = run_scheduler(config, tmp.path().join("locks"), shutdown).await;
        assert_eq!(result.expect("run scheduler"), EXIT_SUCCESS);

        let backup_dir = tmp.path().join("backups");
        let job1_files = std::fs::read_dir(backup_dir.join("job1")).expect("read job1 dir");
        let job2_files = std::fs::read_dir(backup_dir.join("job2")).expect("read job2 dir");
        assert_eq!(job1_files.count(), 2);
        assert_eq!(job2_files.count(), 2);
    }

    #[tokio::test]
    async fn locked_job_skips_run_with_warning() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).expect("create src");
        std::fs::write(src.join("data.txt"), b"locked test").expect("write data");

        let mut job = test_job("locked_job", None, false);
        job.source = vec![src];
        job.storage = StorageConfig::Fs(FsConfig {
            root: tmp.path().join("backups"),
            prefix: "locked_job".to_owned(),
        });

        let op = storage::operator(&job.storage).expect("operator");
        let lock_dir = tmp.path().join("locks");

        // Hold the lock externally
        let held_lock =
            crate::lock::JobLock::try_acquire_in(&lock_dir, &job.name).expect("acquire lock");

        let job_shutdown = Shutdown::new();

        // Calling run_job_task should detect the lock and skip gracefully
        run_job_task(&op, &job, Utc::now(), &lock_dir, None, &job_shutdown).await;

        drop(held_lock);

        // Verify no object was written to storage because it was skipped
        let listed = storage::list_prefix(&op, "fs", "locked_job")
            .await
            .expect("list");
        assert!(listed.is_empty(), "locked job should not write an object");
    }

    #[test]
    fn dst_transition_every_12_hours_fires_at_local_times() {
        use chrono::Datelike as _;
        use chrono::Timelike as _;

        // Schedule parsed from `every 12 hours`
        let sched = crontext::parse("every 12 hours").expect("parse crontext");
        assert_eq!(sched.cron, "0 */12 * * *");
        let cron = config::parse_cron(&sched.cron).expect("parse cron");

        let tz = chrono_tz::America::New_York;

        // Spring forward in America/New_York occurred on Sunday, March 8, 2026:
        // Clocks jumped from 02:00:00 to 03:00:00.
        // Start before midnight on March 8:
        let start_utc = chrono::DateTime::parse_from_rfc3339("2026-03-07T23:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let mut current_slot = None;
        let mut sim_now = start_utc;

        // Collect the next 4 occurrences across the DST spring-forward boundary
        let mut local_fire_times = Vec::new();
        for _ in 0..4 {
            let next = next_fire_time(&cron, &tz, sim_now, current_slot).expect("next fire time");
            let local = next.with_timezone(&tz);
            local_fire_times.push(local);
            current_slot = Some(next);
            sim_now = next;
        }

        // Verify every fire occurrence is strictly at local minute 0 and hour 0 or 12
        for (i, local) in local_fire_times.iter().enumerate() {
            assert_eq!(local.minute(), 0, "fire #{i} minute must be 0: {local}");
            assert!(
                local.hour() == 0 || local.hour() == 12,
                "fire #{i} hour must be 00:00 or 12:00 in America/New_York: {local}"
            );
        }

        // Verify the sequence: 2026-03-08 00:00 EST -> 12:00 EDT -> 2026-03-09 00:00 EDT -> 12:00 EDT
        assert_eq!(local_fire_times[0].hour(), 0);
        assert_eq!(local_fire_times[0].day(), 8);
        assert_eq!(local_fire_times[1].hour(), 12);
        assert_eq!(local_fire_times[1].day(), 8);
        assert_eq!(local_fire_times[2].hour(), 0);
        assert_eq!(local_fire_times[2].day(), 9);
        assert_eq!(local_fire_times[3].hour(), 12);
        assert_eq!(local_fire_times[3].day(), 9);
    }
}
