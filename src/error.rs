//! Error types shared by the library modules.
//!
//! `main.rs` converts these into `anyhow::Error` for reporting and maps them to
//! process exit codes. Variants are added as the modules that produce them land
//! (config in phase 1, storage in phases 1-2, docker/hooks in phase 3, ...).

/// Convenience alias for results produced inside the library modules.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Exit code returned for a plain failure.
pub const EXIT_FAILURE: u8 = 1;

/// Every error this crate can produce.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A subcommand parsed fine but is not wired up yet.
    #[error("`dvb {0}` is not implemented yet")]
    NotImplemented(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_implemented_displays_the_command() {
        let err = Error::NotImplemented("restore");
        assert_eq!(err.to_string(), "`dvb restore` is not implemented yet");
    }
}
