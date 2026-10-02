//! Error types shared by the library modules.
//!
//! `main.rs` converts these into `anyhow::Error` for reporting and maps them to
//! process exit codes. Variants are added as the modules that produce them land
//! (config/storage in phase 1, retention in phase 2, docker/hooks in phase 3).

use std::fmt;
use std::path::{Path, PathBuf};

/// Convenience alias for results produced inside the library modules.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Exit code returned for a plain failure.
pub const EXIT_FAILURE: u8 = 1;

/// Exit code returned when everything the command set out to do succeeded.
pub const EXIT_SUCCESS: u8 = 0;

/// Exit code returned when the work succeeded but a follow-up step did not.
///
/// For a backup that means: the archive is stored, but pruning or a post hook
/// failed. The caller can treat the backup as valid.
pub const EXIT_PARTIAL: u8 = 2;

/// Every error this crate can produce.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A subcommand parsed fine but is not wired up yet.
    #[error("`dvb {0}` is not implemented yet")]
    NotImplemented(&'static str),

    /// The configuration could not be loaded or is invalid.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// Another `dvb` process holds the job lock.
    #[error("job `{job}` is already locked by another dvb process (lock file {path})")]
    Locked { job: String, path: PathBuf },

    /// A filesystem operation failed outside of archiving.
    #[error("io error on {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The requested job name is not in the configuration.
    #[error("unknown job `{name}` (configured jobs: {known})")]
    UnknownJob { name: String, known: String },

    /// Creating or streaming the archive failed.
    #[error("archive error while processing {}{source}", ArchiveContext(context.as_deref()))]
    Archive {
        /// Source path being archived, when the failure can be attributed.
        context: Option<PathBuf>,
        #[source]
        source: std::io::Error,
    },

    /// A storage backend rejected an operation.
    #[error("storage backend `{backend}` failed: {source}")]
    Storage {
        backend: &'static str,
        #[source]
        source: opendal::Error,
    },

    /// A storage backend could not be built from the configuration.
    #[error("invalid storage configuration: {0}")]
    StorageConfig(String),

    /// `dvb check` found at least one broken job.
    #[error("{failures} check(s) failed")]
    CheckFailed { failures: usize },

    /// The Docker API could not be reached, or refused an operation.
    #[error("docker: {0}")]
    Docker(String),

    /// A pre or post hook did not succeed.
    ///
    /// `reason` is free-form because it spans three different failures: a
    /// non-zero exit, a timeout, and a spawn error.
    #[error("hook `{hook}` failed: {reason}")]
    HookFailed { hook: String, reason: String },

    /// A shutdown signal arrived and the run was abandoned.
    ///
    /// Cleanup has already happened by the time this is returned: containers
    /// are restarted and the partial object is removed.
    #[error("interrupted by {signal}")]
    Cancelled { signal: &'static str },
}

/// Errors raised while loading or validating the configuration file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file could not be read.
    #[allow(dead_code)] // figment reports IO problems through Parse
    #[error("cannot read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The config file was not valid TOML, or had an unexpected shape.
    #[error("cannot parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: figment::Error,
    },

    /// The config parsed but does not make sense.
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

impl Error {
    /// Build an archive error, attaching the path that was being archived.
    pub fn archive(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Archive {
            context: Some(path.into()),
            source,
        }
    }

    /// Build an archive error without a specific path.
    #[must_use]
    pub fn archive_other(source: std::io::Error) -> Self {
        Self::Archive {
            context: None,
            source,
        }
    }

    /// Recover the underlying IO error, so a sink failure can be re-wrapped with
    /// the caller's own context.
    #[must_use]
    pub fn into_io(self) -> std::io::Error {
        match self {
            Self::Archive { source, .. } | Self::Io { source, .. } => source,
            other => std::io::Error::other(other.to_string()),
        }
    }
}

/// Renders the `while processing <path>` fragment of [`Error::Archive`].
pub(crate) struct ArchiveContext<'a>(pub Option<&'a Path>);

impl fmt::Display for ArchiveContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self(Some(path)) => write!(f, "{}: ", path.display()),
            Self(None) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_implemented_displays_the_command() {
        let err = Error::NotImplemented("restore");
        assert_eq!(err.to_string(), "`dvb restore` is not implemented yet");
    }

    #[test]
    fn archive_errors_mention_the_path() {
        let err = Error::archive(
            "/data/unreadable",
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope"),
        );
        assert_eq!(
            err.to_string(),
            "archive error while processing /data/unreadable: nope"
        );
    }

    #[test]
    fn archive_errors_without_a_path_still_read_well() {
        let err = Error::archive_other(std::io::Error::other("broken pipe"));
        assert!(
            err.to_string()
                .starts_with("archive error while processing")
        );
        assert_eq!(
            err.to_string(),
            "archive error while processing broken pipe"
        );
    }

    #[test]
    fn unknown_job_lists_the_known_ones() {
        let err = Error::UnknownJob {
            name: "nope".to_owned(),
            known: "db, files".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "unknown job `nope` (configured jobs: db, files)"
        );
    }
}
