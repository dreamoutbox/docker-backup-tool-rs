//! `dvb` — Docker volume backup tool.
//!
//! Parses the command line, initialises tracing, dispatches the subcommand and
//! maps errors to process exit codes:
//!
//! * `0` — success
//! * `1` — failure
//! * `2` — partial: the archive was uploaded but prune or a post hook failed

mod cli;

use dvb::config::{self, Config, JobConfig, ScheduleSource, ValidationMode};
use dvb::{docker, error, init, job, jobs, restore, retention, scheduler, signal, storage};

use std::io::IsTerminal as _;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context as _;
use chrono::Utc;
use clap::Parser as _;

use crate::cli::{Cli, Command, GlobalArgs, LogFormat};
use dvb::error::{EXIT_FAILURE, Error};

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
        Command::Run => run(&cli.global).await,
        Command::Backup { job } => backup(&cli.global, job).await,
        Command::Prune { job, dry_run } => prune(&cli.global, job, *dry_run).await,
        Command::List { job } => list(&cli.global, job).await,
        Command::Check => check(&cli.global).await,
        Command::Crontext {
            expression,
            timezone,
        } => crontext_cmd(expression, timezone.as_deref()),
        Command::Init { output, force } => init::run_init(output, *force),
        Command::Jobs {
            format,
            remote,
            remote_timeout,
            now,
        } => {
            let now_dt = if let Some(now_str) = now {
                Some(
                    chrono::DateTime::parse_from_rfc3339(now_str)
                        .map_err(|err| {
                            Error::Config(dvb::error::ConfigError::Invalid(format!(
                                "invalid --now timestamp `{now_str}`: {err}"
                            )))
                        })?
                        .with_timezone(&chrono::Utc),
                )
            } else {
                None
            };
            jobs::run_jobs(
                &cli.global.config,
                *format,
                *remote,
                std::time::Duration::from_secs(*remote_timeout),
                now_dt,
            )
            .await
        }
        Command::Restore {
            job,
            name,
            at,
            to,
            script,
            script_timeout,
            force,
            cleanup,
            stop_containers,
            no_verify,
            dry_run,
            preserve_owner,
            max_extracted_bytes,
            extra_args,
        } => {
            restore(
                &cli.global,
                job,
                restore::RestoreOptions {
                    name: name.clone(),
                    at: at.clone(),
                    to: to.clone(),
                    script: script.clone(),
                    script_timeout: *script_timeout,
                    force: *force,
                    cleanup: *cleanup,
                    stop_containers: *stop_containers,
                    no_verify: *no_verify,
                    dry_run: *dry_run,
                    preserve_owner: *preserve_owner,
                    max_extracted_bytes: *max_extracted_bytes,
                    extra_args: extra_args.clone(),
                },
            )
            .await
        }
    };

    outcome
        .map_err(anyhow::Error::from)
        .with_context(|| describe(&cli.command))
}

/// `dvb run`: start the scheduler daemon and execute jobs on their cron schedules.
async fn run(global: &GlobalArgs) -> error::Result<u8> {
    let config = Config::load(&global.config, ValidationMode::Full)?;
    scheduler::run(config).await
}

/// `dvb backup <job>`: one archive, streamed to storage, right now.
///
/// Returns exit code 2 when the archive was uploaded but a post hook or
/// retention failed, and 1 when a container could not be restarted, so a caller
/// can tell "safe but needs attention" apart from a hard failure.
async fn backup(global: &GlobalArgs, job_name: &str) -> error::Result<u8> {
    let config = Config::load(&global.config, ValidationMode::Full)?;
    let job = config.job(job_name)?;

    let op = storage::operator(&job.storage)?;
    let run = job::RunContext::for_job(config.docker.socket.as_deref(), job)?;
    // SIGINT/SIGTERM unwind through the pipeline instead of killing the
    // process: containers come back up and the partial object is removed.
    run.shutdown.install();

    Ok(job::run_backup(&op, job, Utc::now(), &run)
        .await?
        .exit_code())
}

/// `dvb prune <job> [--dry-run]`: apply retention without a new backup.
async fn prune(global: &GlobalArgs, job_name: &str, dry_run: bool) -> error::Result<u8> {
    let config = Config::load(&global.config, ValidationMode::Full)?;
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
    let config = Config::load(&global.config, ValidationMode::Full)?;
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

/// `dvb restore <job>`: download and safely extract a backup.
async fn restore(
    global: &GlobalArgs,
    job_name: &str,
    options: restore::RestoreOptions,
) -> error::Result<u8> {
    let config = Config::load(&global.config, ValidationMode::Full)?;
    let job = config.job(job_name)?;
    let op = storage::operator(&job.storage)?;

    let shutdown = signal::Shutdown::new();
    shutdown.install();

    let outcome = restore::run_restore(
        &op,
        job,
        &options,
        &shutdown,
        config.docker.socket.as_deref(),
    )
    .await?;

    if !options.dry_run {
        println!("{}", outcome.target_dir.display());
    }

    Ok(outcome.exit_code)
}

/// `dvb check`: validate the config, then prove each backend works.
///
/// Storage is checked with a real round trip (write, read, delete) rather than
/// just a list, because list succeeds on many backends where write needs
/// different permissions.
async fn check(global: &GlobalArgs) -> error::Result<u8> {
    let config = Config::load(&global.config, ValidationMode::Full)?;
    println!("configuration at {} is valid", global.config.display());
    println!("{} job(s) defined", config.job_count());

    let (docker, mut failures) = check_socket(config.docker.socket.as_deref()).await;

    let default_tz = scheduler::resolve_timezone()?;
    for job in &config.jobs {
        let tz = job.effective_timezone(default_tz);
        println!("\njob `{}` ({} backend)", job.name, job.storage.kind());

        if let Some(cron_str) = &job.cron {
            let parsed_cron = config::parse_cron(cron_str).map_err(|err| {
                Error::Config(dvb::error::ConfigError::Invalid(format!(
                    "job `{}`: invalid cron `{cron_str}`: {err}",
                    job.name
                )))
            })?;
            let next = scheduler::next_fire_time(&parsed_cron, &tz, Utc::now(), None)?;
            let source_desc = match &job.schedule_source {
                Some(ScheduleSource::Crontext(orig)) => format!("crontext: \"{orig}\""),
                _ => "cron".to_owned(),
            };
            println!(
                "  schedule: {cron_str} (from {source_desc}) [{}]",
                tz.name()
            );
            println!("  next run: {}", next.with_timezone(&tz).to_rfc3339());
        }

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

        failures += check_containers(docker.as_ref(), job).await;

        if let Some(restore) = &job.post_restore
            && let Some(dir) = &restore.dir
        {
            check_restore_dir(dir);
        }
    }

    if failures > 0 {
        return Err(Error::CheckFailed { failures });
    }
    println!("\nall checks passed");
    Ok(EXIT_SUCCESS)
}

/// `dvb crontext`: evaluate a human-friendly schedule expression.
fn crontext_cmd(expression: &str, tz_override: Option<&str>) -> error::Result<u8> {
    let sched = crontext::parse(expression).map_err(|err| {
        Error::Config(dvb::error::ConfigError::Invalid(format!(
            "invalid crontext expression `{expression}`: {err}"
        )))
    })?;
    let tz = match tz_override {
        Some(s) => scheduler::resolve_timezone_from(Some(s))?,
        None => scheduler::resolve_timezone()?,
    };
    let cron = config::parse_cron(&sched.cron).map_err(|err| {
        Error::Config(dvb::error::ConfigError::Invalid(format!(
            "resolved cron `{}` is invalid: {err}",
            sched.cron
        )))
    })?;

    println!("expression:  {expression}");
    println!("cron:        {}", sched.cron);
    println!("description: {}", sched.description);
    println!("timezone:    {}", tz.name());
    println!("next 5 fire times:");
    let mut cursor = None;
    let now = Utc::now();
    for i in 1..=5 {
        let next = scheduler::next_fire_time(&cron, &tz, now, cursor)?;
        println!("  {i}. {}", next.with_timezone(&tz).to_rfc3339());
        cursor = Some(next);
    }

    Ok(EXIT_SUCCESS)
}

/// Connect to and ping the configured Docker socket.
///
/// Returns the client to reuse for the per-job checks plus how many failures it
/// counted. No socket configured is not a failure: container control is
/// optional, and jobs that need it are rejected when the config is loaded.
async fn check_socket(socket: Option<&Path>) -> (Option<docker::Client>, usize) {
    let Some(path) = socket else {
        println!("\ndocker socket: not configured (container control disabled)");
        return (None, 0);
    };

    let outcome = async {
        let client = docker::Client::connect(path)?;
        client.ping().await?;
        Ok::<_, Error>(client)
    }
    .await;

    match outcome {
        Ok(client) => {
            println!("\ndocker socket: ok ({})", path.display());
            (Some(client), 0)
        }
        Err(err) => {
            println!("\ndocker socket: FAILED: {err}");
            (None, 1)
        }
    }
}

/// Report whether the job's containers resolve, and dry-run each container hook.
///
/// The dry run is `true` inside the target container: it proves the socket can
/// exec there, which is the part that only fails at backup time otherwise.
///
/// Returns how many checks failed.
async fn check_containers(client: Option<&docker::Client>, job: &JobConfig) -> usize {
    let hooks = [
        ("pre", job.pre_backup.as_ref()),
        ("post", job.post_backup.as_ref()),
    ]
    .into_iter()
    .filter_map(|(phase, hook)| hook.map(|hook| (phase, hook)))
    .filter(|(_, hook)| hook.container.is_some());

    if !job.needs_docker() {
        return 0;
    }

    let Some(client) = client else {
        println!("  containers: FAILED: no reachable [docker] socket");
        return 1;
    };

    let mut failures = 0_usize;

    match client
        .resolve(&job.stop_containers, job.stop_label.as_deref())
        .await
    {
        Ok(found) => println!("  containers: ok ({} matched)", found.len()),
        Err(err) => {
            println!("  containers: FAILED: {err}");
            failures += 1;
        }
    }

    for (phase, hook) in hooks {
        let Some(container) = hook.container.as_deref() else {
            continue;
        };
        let label = format!("{phase} hook `{}`", hook.describe());
        let timeout = Duration::from_secs(hook.timeout_secs.min(30));

        match client.resolve(&[container.to_owned()], None).await {
            Err(err) => {
                println!("  {label}: FAILED: {err}");
                failures += 1;
            }
            Ok(found) if found.is_empty() || !found[0].running => {
                println!(
                    "  {label}: skipped (container `{container}` is not running), \
                     cannot dry-run it now"
                );
            }
            Ok(_) => {
                let probe = ["true"].map(str::to_owned);
                match client.exec(container, &probe, &[], timeout).await {
                    Ok(0) => println!("  {label}: exec ok"),
                    Ok(code) => {
                        println!("  {label}: FAILED: `true` exited {code}");
                        failures += 1;
                    }
                    Err(err) => {
                        println!("  {label}: FAILED: {err}");
                        failures += 1;
                    }
                }
            }
        }
    }

    failures
}

/// Report whether the restore base directory is writable.
fn check_restore_dir(dir: &Path) {
    let mut check_path = dir;
    while !check_path.exists() {
        if let Some(parent) = check_path.parent() {
            check_path = parent;
        } else {
            break;
        }
    }
    let probe = check_path.join(format!(".dvb-check-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(&probe);
            println!("  restore dir: ok ({})", dir.display());
        }
        Err(err) => {
            println!(
                "  warning: restore base dir `{}` is not writable: {err}",
                dir.display()
            );
        }
    }
}

/// Short description of what was attempted, used as error context.
fn describe(command: &Command) -> String {
    match command {
        Command::Run => "daemon run failed".to_owned(),
        Command::Backup { job }
        | Command::Prune { job, .. }
        | Command::List { job }
        | Command::Restore { job, .. } => {
            format!("job `{job}` failed")
        }
        Command::Check => "configuration check failed".to_owned(),
        Command::Crontext { .. } => "crontext evaluation failed".to_owned(),
        Command::Init { .. } => "init failed".to_owned(),
        Command::Jobs { .. } => "jobs failed".to_owned(),
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
        assert_eq!(error::EXIT_PARTIAL, 2);
    }

    #[test]
    fn run_without_config_fails() {
        let cli = Cli::parse_from(["dvb", "run"]);
        let err = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(dispatch(&cli))
            .unwrap_err();
        assert!(format!("{err:#}").contains("no [[job]] defined"));
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
        assert_eq!(
            describe(&Command::Restore {
                job: "pg".to_owned(),
                name: None,
                at: None,
                to: None,
                script: None,
                script_timeout: None,
                force: false,
                cleanup: true,
                stop_containers: false,
                no_verify: false,
                dry_run: false,
                preserve_owner: false,
                max_extracted_bytes: None,
                extra_args: vec![],
            }),
            "job `pg` failed"
        );
        assert_eq!(describe(&Command::Check), "configuration check failed");
        assert_eq!(
            describe(&Command::Crontext {
                expression: "every day".to_owned(),
                timezone: None,
            }),
            "crontext evaluation failed"
        );
        assert_eq!(
            describe(&Command::Init {
                output: String::new(),
                force: false,
            }),
            "init failed"
        );
        assert_eq!(
            describe(&Command::Jobs {
                format: cli::JobsFormat::Table,
                remote: false,
                remote_timeout: 15,
                now: None,
            }),
            "jobs failed"
        );
    }
}
