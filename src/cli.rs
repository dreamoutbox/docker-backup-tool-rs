//! Command line surface.
//!
//! Global flags (`--config`, `--log-format`, `-v`) are marked `global` so they
//! may appear before or after the subcommand name.

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};

use dvb::config::DEFAULT_CONFIG_PATH;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum JobsFormat {
    /// Human-readable table layout (default).
    #[default]
    Table,
    /// JSON view model output.
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

    /// Restore a backup to a directory.
    Restore {
        /// Name of the job as defined in the config file.
        job: String,

        /// Exact backup object name (under the job's storage prefix).
        #[arg(long, conflicts_with = "at")]
        name: Option<String>,

        /// Newest backup at or before this time (RFC3339 or YYYY-MM-DD).
        #[arg(long, conflicts_with = "name")]
        at: Option<String>,

        /// Extraction target directory.
        #[arg(long)]
        to: Option<PathBuf>,

        /// Script to execute after extraction.
        #[arg(long)]
        script: Option<PathBuf>,

        /// Timeout in seconds for the restore script.
        #[arg(long)]
        script_timeout: Option<u64>,

        /// Allow extracting into an existing non-empty directory.
        #[arg(long)]
        force: bool,

        /// Delete extracted directory after the script succeeds (default: true).
        #[arg(long, default_value = "true", num_args = 0..=1, default_missing_value = "true")]
        cleanup: bool,

        /// Stop containers while the restore script runs.
        #[arg(long)]
        stop_containers: bool,

        /// Skip checksum verification.
        #[arg(long)]
        no_verify: bool,

        /// Resolve the backup and print the plan without downloading.
        #[arg(long)]
        dry_run: bool,

        /// Restore file ownership when running as root.
        #[arg(long)]
        preserve_owner: bool,

        /// Maximum allowed extracted bytes (guard against decompression bombs).
        #[arg(long)]
        max_extracted_bytes: Option<u64>,

        /// Extra arguments passed to the restore script.
        #[arg(last = true)]
        extra_args: Vec<String>,
    },

    /// Parse and evaluate a human-friendly schedule expression.
    Crontext {
        /// Schedule expression (e.g. "every friday at 18:00").
        expression: String,

        /// Timezone to evaluate the schedule in (defaults to TZ or UTC).
        #[arg(long, short)]
        timezone: Option<String>,
    },

    /// Write a full reference configuration file.
    Init {
        /// Output path for the reference config (or "-" for stdout).
        #[arg(short, long, default_value = "./dvb.toml", value_name = "PATH")]
        output: String,

        /// Overwrite the target file if it already exists.
        #[arg(long)]
        force: bool,
    },

    /// List all configured backup jobs.
    Jobs {
        /// Output format (table or json).
        #[arg(long, value_enum, default_value_t = JobsFormat::Table)]
        format: JobsFormat,

        /// Query storage backends for backup statistics (last backup, count, size).
        #[arg(long)]
        remote: bool,

        /// Timeout in seconds per job when querying remote storage.
        #[arg(long, default_value_t = 15, value_name = "SECS")]
        remote_timeout: u64,
    },
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
        assert_eq!(
            names,
            [
                "run", "backup", "prune", "list", "check", "restore", "crontext", "init", "jobs"
            ]
        );
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
