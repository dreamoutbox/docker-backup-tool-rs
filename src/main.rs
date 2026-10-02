//! `dvb` — Docker volume backup tool.
//!
//! Parses the command line, initialises tracing, dispatches the subcommand and
//! maps errors to process exit codes:
//!
//! * `0` — success
//! * `1` — failure
//! * `2` — partial: the archive was uploaded but prune or a post hook failed

mod cli;

use dvb::config::{self, Config};
use dvb::{error, job, retention, storage};

use std::io::IsTerminal as _;
use std::process::ExitCode;

use anyhow::Context as _;
use chrono::Utc;
use clap::Parser as _;

use crate::cli::{Cli, Command, GlobalArgs, LogFormat};
use dvb::error::{EXIT_FAILURE, EXIT_PARTIAL, Error};

/// Exit code for a completed command.
const EXIT_SUCCESS: u8 = 0;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli.global);

    // Must happen before any operator is built: without it the HTTP backends
    // (s3, dropbox) fail their first request.
    if let Err(err) = storage::install_transport() {
        tracing::error!("cannot install the HTTP transport: {err}");
        return ExitCode::from(EXIT_FAILURE);
    }

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
        Command::Prune { job, dry_run } => prune(&cli.global, job, *dry_run).await,
        Command::List { job } => list(&cli.global, job).await,
        Command::Check => check(&cli.global).await,
    };

    outcome
        .map_err(anyhow::Error::from)
        .with_context(|| describe(&cli.command))
}

/// `dvb backup <job>`: one archive, streamed to storage, right now.
///
/// Returns exit code 2 when the archive was uploaded but retention failed, so a
/// caller can tell "the backup is safe but rotation needs attention" apart from
/// a hard failure.
async fn backup(global: &GlobalArgs, job_name: &str) -> error::Result<u8> {
    let config = Config::load(&global.config)?;
    let job = config.job(job_name)?;

    let op = storage::operator(&job.storage)?;
    let outcome = job::run_backup(&op, job, Utc::now()).await?;

    match outcome.prune {
        Ok(_) => Ok(EXIT_SUCCESS),
        Err(err) => {
            tracing::error!("the backup succeeded but pruning did not: {err}");
            Ok(EXIT_PARTIAL)
        }
    }
}

/// `dvb prune <job> [--dry-run]`: apply retention without a new backup.
async fn prune(global: &GlobalArgs, job_name: &str, dry_run: bool) -> error::Result<u8> {
    let config = Config::load(&global.config)?;
    let job = config.job(job_name)?;
    let op = storage::operator(&job.storage)?;

    let plan = retention::prune(&op, job, Utc::now(), dry_run).await?;
    let verb = if dry_run { "would delete" } else { "deleted" };
    // The plan goes to stdout so it can be piped; logs stay on stderr.
    println!(
        "{}: {verb} {}, {}",
        job.name,
        plan.delete_count(),
        plan.summary()
    );
    for backup in &plan.expired {
        println!("  {}", backup.path);
    }

    Ok(EXIT_SUCCESS)
}

/// `dvb list <job>`: what is stored, with the timestamp parsed from the name.
async fn list(global: &GlobalArgs, job_name: &str) -> error::Result<u8> {
    let config = Config::load(&global.config)?;
    let job = config.job(job_name)?;
    let op = storage::operator(&job.storage)?;

    let (backups, ignored) = retention::list_backups(&op, job).await?;

    if backups.is_empty() {
        println!("no backups found for job `{}`", job.name);
    }
    for backup in &backups {
        println!(
            "{}\t{}\t{}",
            backup.timestamp.to_rfc3339(),
            backup.size,
            backup.path
        );
    }
    if !ignored.is_empty() {
        tracing::warn!(
            count = ignored.len(),
            "ignored objects under the prefix that do not match the job's filename pattern"
        );
    }

    Ok(EXIT_SUCCESS)
}

/// `dvb check`: validate the config, then prove each backend works.
///
/// Storage is checked with a real round trip (write, read, delete) rather than
/// just a list, because list succeeds on many backends where write needs
/// different permissions.
async fn check(global: &GlobalArgs) -> error::Result<u8> {
    let config = Config::load(&global.config)?;
    println!("configuration at {} is valid", global.config.display());
    println!("{} job(s) defined", config.job_count());

    let mut failures = 0_usize;
    for job in &config.jobs {
        println!("\njob `{}` ({} backend)", job.name, job.storage.kind());

        match storage::operator(&job.storage) {
            Ok(op) => match storage::probe(&op, job.storage.kind()).await {
                Ok(()) => println!("  storage: ok"),
                Err(err) => {
                    println!("  storage: FAILED: {err:#}");
                    failures += 1;
                }
            },
            Err(err) => {
                println!("  storage: FAILED to configure: {err}");
                failures += 1;
            }
        }

        // Retention input: is the prefix listable and are existing backups
        // parseable? Reported, not fatal, since an empty prefix is normal.
        match retention::list_backups(&storage::operator(&job.storage)?, job).await {
            Ok((backups, ignored)) => {
                println!("  backups: {} stored", backups.len());
                if !ignored.is_empty() {
                    println!(
                        "  warning: {} object(s) under the prefix do not match the filename \
                         pattern and will never be pruned: {:?}",
                        ignored.len(),
                        ignored
                    );
                }
            }
            Err(err) => {
                println!("  backup listing: FAILED: {err:#}");
                failures += 1;
            }
        }
    }

    if failures > 0 {
        return Err(Error::CheckFailed { failures });
    }
    println!("\nall checks passed");
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
        let cli = Cli::parse_from(["dvb", "run"]);
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
