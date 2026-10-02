//! Command line surface.
//!
//! Global flags (`--config`, `--log-format`, `-v`) are marked `global` so they
//! may appear before or after the subcommand name.

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};

use crate::config::DEFAULT_CONFIG_PATH;

#[derive(Debug, Parser)]
#[command(
    name = "dvb",
    version,
    about = "Docker volume backup tool",
    long_about = "Tar and compress mounted paths, then stream the archive to a storage backend.",
    propagate_version = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, clap::Args)]
pub struct GlobalArgs {
    /// Path to the TOML configuration file.
    #[arg(
        long,
        global = true,
        env = "DVB_CONFIG",
        default_value = DEFAULT_CONFIG_PATH,
        value_name = "PATH"
    )]
    pub config: PathBuf,

    /// Tracing output format.
    #[arg(
        long,
        global = true,
        env = "DVB_LOG_FORMAT",
        value_enum,
        value_name = "FORMAT",
        default_value_t = LogFormat::Text
    )]
    pub log_format: LogFormat,

    /// Increase log verbosity (repeat for trace level).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    /// Human readable, one line per event.
    Text,
    /// One JSON object per event.
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Run the scheduler and execute every configured job on its cron schedule.
    Run,

    /// Run a single job once, right now.
    Backup {
        /// Name of the job as defined in the config file.
        job: String,
    },

    /// Apply the retention policy of a job without creating a new backup.
    Prune {
        /// Name of the job as defined in the config file.
        job: String,
        /// Only report what would be deleted.
        #[arg(long)]
        dry_run: bool,
    },

    /// List the backups stored for a job.
    List {
        /// Name of the job as defined in the config file.
        job: String,
    },

    /// Validate the configuration and test storage/docker connectivity.
    Check,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn verify_cli() {
        Cli::command().debug_assert();
    }

    #[test]
    fn subcommands_are_registered() {
        let command = Cli::command();
        let names: Vec<&str> = command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert_eq!(names, ["run", "backup", "prune", "list", "check"]);
    }

    #[test]
    fn config_defaults_to_etc() {
        let cli = Cli::try_parse_from(["dvb", "backup", "db"]).expect("valid cli");
        // `try_parse_from` fails only on unknown args; the default is applied.
        assert_eq!(cli.global.config, PathBuf::from("/etc/dvb/config.toml"));
        assert_eq!(
            cli.command,
            Command::Backup {
                job: "db".to_owned()
            }
        );
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::try_parse_from(["dvb", "backup", "db", "--config", "/tmp/dvb.toml", "-vv"])
            .expect("valid cli");
        assert_eq!(cli.global.config, PathBuf::from("/tmp/dvb.toml"));
        assert_eq!(cli.global.verbose, 2);
    }

    #[test]
    fn prune_accepts_dry_run() {
        let cli = Cli::try_parse_from(["dvb", "prune", "db", "--dry-run"]).expect("valid cli");
        assert!(matches!(cli.command, Command::Prune { dry_run: true, .. }));
    }
}
