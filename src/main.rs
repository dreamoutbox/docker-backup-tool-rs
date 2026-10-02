//! `dvb` — Docker volume backup tool.
//!
//! Parses the command line, initialises tracing, dispatches the subcommand and
//! maps errors to process exit codes:
//!
//! * `0` — success
//! * `1` — failure
//! * `2` — partial: the archive was uploaded but prune or a post hook failed

mod cli;
mod error;

use std::io::IsTerminal as _;
use std::process::ExitCode;

use anyhow::Context as _;
use clap::Parser as _;

use crate::cli::{Cli, Command, GlobalArgs, LogFormat};
use crate::error::{EXIT_FAILURE, Error};

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli.global);

    match dispatch(&cli) {
        Ok(exit) => ExitCode::from(exit),
        Err(err) => {
            // `{:#}` renders the whole anyhow context chain on one line.
            tracing::error!("{err:#}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

/// Run the requested subcommand. Each arm returns the process exit code.
fn dispatch(cli: &Cli) -> anyhow::Result<u8> {
    let outcome: error::Result<u8> = match &cli.command {
        Command::Run => Err(Error::NotImplemented("run")),
        Command::Backup { .. } => Err(Error::NotImplemented("backup")),
        Command::Prune { .. } => Err(Error::NotImplemented("prune")),
        Command::List { .. } => Err(Error::NotImplemented("list")),
        Command::Check => Err(Error::NotImplemented("check")),
    };

    outcome
        .map_err(anyhow::Error::from)
        .with_context(|| describe(&cli.command))
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
/// The filter comes from `DVB_LOG` when set (e.g. `dvb=debug,s3=trace`),
/// otherwise from the `-v` verbosity count. ANSI escapes are only emitted when
/// stderr is a terminal, so `docker logs` stays readable.
fn init_tracing(global: &GlobalArgs) {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_env_var("DVB_LOG")
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
        assert_eq!(error::EXIT_FAILURE, 1);
    }

    #[test]
    fn unstubbed_commands_fail_with_exit_code_one() {
        let cli = Cli::parse_from(["dvb", "check"]);
        let err = dispatch(&cli).unwrap_err();
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
