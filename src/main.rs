//! `dvb` — Docker volume backup tool.
//!
//! Parses the command line, initialises tracing, dispatches the subcommand and
//! maps errors to process exit codes:
//!
//! * `0` — success
//! * `1` — failure
//! * `2` — partial: the archive was uploaded but prune or a post hook failed

mod archive;
mod cli;
mod config;
mod error;
mod job;
mod lock;
mod storage;

use std::io::IsTerminal as _;
use std::process::ExitCode;

use anyhow::Context as _;
use chrono::Utc;
use clap::Parser as _;

use crate::cli::{Cli, Command, GlobalArgs, LogFormat};
use crate::config::Config;
use crate::error::{EXIT_FAILURE, Error};

/// Exit code for a completed command.
const EXIT_SUCCESS: u8 = 0;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli.global);

    // The runtime is built by hand so `spawn_blocking` (used by the archiver)
    // gets a full multi-threaded blocking pool.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!("cannot start the tokio runtime: {err}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };

    match runtime.block_on(dispatch(&cli)) {
        Ok(exit) => ExitCode::from(exit),
        Err(err) => {
            // `{:#}` renders the whole anyhow context chain on one line.
            tracing::error!("{err:#}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

/// Run the requested subcommand. Each arm returns the process exit code.
async fn dispatch(cli: &Cli) -> anyhow::Result<u8> {
    let outcome: error::Result<u8> = match &cli.command {
        Command::Run => Err(Error::NotImplemented("run")),
        Command::Backup { job } => backup(&cli.global, job).await,
        Command::Prune { .. } => Err(Error::NotImplemented("prune")),
        Command::List { .. } => Err(Error::NotImplemented("list")),
        Command::Check => Err(Error::NotImplemented("check")),
    };

    outcome
        .map_err(anyhow::Error::from)
        .with_context(|| describe(&cli.command))
}

/// `dvb backup <job>`: one archive, streamed to storage, right now.
async fn backup(global: &GlobalArgs, job_name: &str) -> error::Result<u8> {
    let config = Config::load(&global.config)?;
    let job = config.job(job_name)?;

    let op = crate::storage::operator(&job.storage)?;
    crate::job::run_backup(&op, job, Utc::now()).await?;

    Ok(EXIT_SUCCESS)
}

/// Short description of what was attempted, used as error context.
fn describe(command: &Command) -> String {
    match command {
        Command::Run => "daemon run failed".to_owned(),
        Command::Backup { job } | Command::Prune { job, .. } | Command::List { job } => {
            format!("job `{job}` failed")
        }
        Command::Check => "configuration check failed".to_owned(),
    }
}

/// Install the global tracing subscriber (text or JSON).
///
/// The filter comes from `DVB_LOG` when set (e.g. `dvb=debug,opendal=trace`),
/// otherwise from the `-v` verbosity count. ANSI escapes are only emitted when
/// stderr is a terminal, so `docker logs` stays readable.
fn init_tracing(global: &GlobalArgs) {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_env_var(config::ENV_LOG_FILTER)
        .with_default_directive(default_filter_directive(global.verbose).into())
        .from_env_lossy();

    let ansi = std::io::stderr().is_terminal();

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(global.verbose > 0)
        .with_ansi(ansi)
        .with_writer(std::io::stderr);

    match global.log_format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().flatten_event(true).init(),
    }
}

/// `0` warnings, `-v` info, `-vv` debug, `-vvv` and above trace.
fn default_filter_directive(verbose: u8) -> tracing::level_filters::LevelFilter {
    use tracing::level_filters::LevelFilter;
    match verbose {
        0 => LevelFilter::WARN,
        1 => LevelFilter::INFO,
        2 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_maps_to_levels() {
        assert_eq!(default_filter_directive(0).to_string(), "warn");
        assert_eq!(default_filter_directive(1).to_string(), "info");
        assert_eq!(default_filter_directive(2).to_string(), "debug");
        assert_eq!(default_filter_directive(9).to_string(), "trace");
    }

    #[test]
    fn exit_codes_match_the_specification() {
        assert_eq!(EXIT_SUCCESS, 0);
        assert_eq!(error::EXIT_FAILURE, 1);
    }

    #[test]
    fn not_yet_implemented_commands_fail() {
        let cli = Cli::parse_from(["dvb", "check"]);
        let err = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(dispatch(&cli))
            .unwrap_err();
        assert!(format!("{err:#}").contains("not implemented yet"));
    }

    #[test]
    fn describe_names_the_job() {
        assert_eq!(describe(&Command::Run), "daemon run failed");
        assert_eq!(
            describe(&Command::List {
                job: "pg".to_owned()
            }),
            "job `pg` failed"
        );
        assert_eq!(describe(&Command::Check), "configuration check failed");
    }
}
