//! Per-job file locks.
//!
//! The daemon and `docker exec dvb backup <job>` are separate processes, so
//! mutual exclusion has to go through the filesystem. `fd-lock` gives us an
//! advisory `flock`/`LockFileEx` on a lock file: it is released automatically
//! when the process dies, so a crashed run never leaves a stale lock behind.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fd_lock::RwLock;

use crate::error::{Error, Result};

/// Default directory for lock files. Matches the runtime image layout.
pub const DEFAULT_LOCK_DIR: &str = "/run/dvb";

/// Environment variable overriding the lock directory (used by tests).
pub const ENV_LOCK_DIR: &str = "DVB_LOCK_DIR";

/// Directory holding lock files: `DVB_LOCK_DIR` or [`DEFAULT_LOCK_DIR`].
#[must_use]
pub fn lock_dir() -> PathBuf {
    lock_dir_from(std::env::var_os(ENV_LOCK_DIR).as_deref())
}

/// Pure form of [`lock_dir`], so it can be tested without touching the
/// process environment.
fn lock_dir_from(env_value: Option<&std::ffi::OsStr>) -> PathBuf {
    env_value
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_LOCK_DIR), PathBuf::from)
}

/// Path of the lock file for `job`, in the configured lock directory.
#[allow(dead_code)] // used by log lines from phase 4 on
#[must_use]
pub fn lock_path(job: &str) -> PathBuf {
    lock_path_in(&lock_dir(), job)
}

/// Pure form of [`lock_path`].
#[allow(dead_code)] // tests and the pure helpers below
fn lock_path_in(dir: &Path, job: &str) -> PathBuf {
    dir.join(format!("{job}.lock"))
}

/// An acquired job lock. The lock is released when this value is dropped.
#[derive(Debug)]
#[allow(dead_code)] // accessors are used by the scheduler's log lines in phase 4
pub struct JobLock {
    job: String,
    path: PathBuf,
    // The `RwLock` owns the file descriptor, and the `flock` lives on that
    // descriptor, so dropping this releases the lock.
    _lock: RwLock<File>,
}

impl JobLock {
    /// Take the lock for `job`, failing immediately when another process holds it.
    ///
    /// The lock directory is created if missing.
    ///
    /// # Errors
    ///
    /// [`Error::Locked`] when the lock is held elsewhere, and [`Error::Io`] when
    /// the directory or file cannot be opened.
    #[allow(dead_code)] // used by `dvb backup`; tests use the explicit-dir form
    pub fn try_acquire(job: &str) -> Result<Self> {
        Self::try_acquire_in(&lock_dir(), job)
    }

    /// [`Self::try_acquire`] against an explicit directory.
    ///
    /// # Errors
    ///
    /// As [`Self::try_acquire`].
    pub fn try_acquire_in(dir: &Path, job: &str) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|source| Error::Io {
            path: dir.to_path_buf(),
            source,
        })?;

        let path = dir.join(format!("{job}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;

        let mut lock = RwLock::new(file);
        let guard = lock.try_write().map_err(|_| Error::Locked {
            job: job.to_owned(),
            path: path.clone(),
        })?;

        // `flock` is tied to the open file description, not to the guard object:
        // it is released when the last descriptor for that description is closed.
        // Leaking the guard keeps the lock held for as long as this struct owns
        // the `File`, and closing the file in `Drop` releases it. The alternative
        // (a self-referential guard field) would need `unsafe`.
        std::mem::forget(guard);

        tracing::debug!(job, path = %path.display(), "acquired job lock");
        Ok(Self {
            job: job.to_owned(),
            path,
            _lock: lock,
        })
    }

    /// Job this lock belongs to.
    #[allow(dead_code)]
    #[must_use]
    pub fn job(&self) -> &str {
        &self.job
    }

    /// Path of the lock file, for error messages.
    #[allow(dead_code)]
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Whether the lock file for `job` is currently held by someone.
///
/// Only used for diagnostics and tests; probing means taking the lock, so this
/// momentarily contends with a real acquire.
#[allow(dead_code)]
#[must_use]
pub fn is_locked(job: &str) -> bool {
    is_locked_in(&lock_dir(), job)
}

/// [`is_locked`] against an explicit directory.
fn is_locked_in(dir: &Path, job: &str) -> bool {
    let path = dir.join(format!("{job}.lock"));
    if !path.exists() {
        return false;
    }
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
        return true;
    };
    let mut lock = RwLock::new(file);
    lock.try_write().is_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `flock` is per file descriptor, so a second descriptor in the same process
    /// also contends. That lets lock behaviour be tested without a child process.
    #[test]
    fn first_acquire_succeeds_and_second_fails() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let first = JobLock::try_acquire_in(tmp.path(), "db").expect("first acquire");
        assert_eq!(first.job(), "db");
        assert!(first.path().starts_with(tmp.path()));

        let err = JobLock::try_acquire_in(tmp.path(), "db").unwrap_err();
        assert!(matches!(err, Error::Locked { .. }), "got {err:?}");
        assert!(err.to_string().contains("already locked"), "{err}");
        assert_eq!(
            err.to_string(),
            format!(
                "job `db` is already locked by another dvb process (lock file {})",
                first.path().display()
            )
        );
    }

    #[test]
    fn different_jobs_do_not_contend() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _db = JobLock::try_acquire_in(tmp.path(), "db").expect("db lock");
        let _files = JobLock::try_acquire_in(tmp.path(), "files").expect("files lock");
    }

    #[test]
    fn releasing_allows_reacquisition() {
        let tmp = tempfile::tempdir().expect("tempdir");
        drop(JobLock::try_acquire_in(tmp.path(), "db").expect("first"));
        JobLock::try_acquire_in(tmp.path(), "db").expect("reacquire after drop");
    }

    #[test]
    fn missing_lock_directory_is_created() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let nested = tmp.path().join("a/b/c");
        let lock = JobLock::try_acquire_in(&nested, "db").expect("acquire creates dirs");
        assert!(nested.join("db.lock").exists());
        drop(lock);
    }

    #[test]
    fn is_locked_reports_state() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!is_locked_in(tmp.path(), "db"));
        let lock = JobLock::try_acquire_in(tmp.path(), "db").expect("acquire");
        assert!(is_locked_in(tmp.path(), "db"));
        drop(lock);
        assert!(!is_locked_in(tmp.path(), "db"));
    }

    #[test]
    fn lock_dir_defaults_and_overrides() {
        use std::ffi::OsStr;

        assert_eq!(lock_dir_from(None), PathBuf::from(DEFAULT_LOCK_DIR));
        assert_eq!(
            lock_dir_from(Some(OsStr::new("/tmp/dvb-locks"))),
            PathBuf::from("/tmp/dvb-locks")
        );
        // An empty value is treated as unset.
        assert_eq!(
            lock_dir_from(Some(OsStr::new(""))),
            PathBuf::from(DEFAULT_LOCK_DIR)
        );
    }

    #[test]
    fn lock_path_joins_the_directory_and_job_name() {
        assert_eq!(
            lock_path_in(Path::new("/tmp/locks"), "db"),
            PathBuf::from("/tmp/locks/db.lock")
        );
    }

    #[test]
    fn lock_directory_cannot_be_created_reports_the_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap_or_else(|e| panic!("write: {e}"));
        let nested = file.join("locks");

        let err = JobLock::try_acquire_in(&nested, "db").unwrap_err();
        assert!(matches!(err, Error::Io { .. }), "got {err:?}");
        assert!(err.to_string().contains("not-a-dir"), "{err}");
    }
}
