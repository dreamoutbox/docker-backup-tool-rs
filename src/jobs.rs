//! Implementation of `dvb jobs`.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::ValueEnum;
use futures_util::stream::StreamExt;

use crate::config::{Config, JobConfig, ValidationMode};
use crate::error::{ConfigError, EXIT_PARTIAL, EXIT_SUCCESS, Error, Result};
use crate::retention;
use crate::storage;
use crate::summary::{self, JobSummary, JobsOutput, RemoteSummary};

/// Output format for `dvb jobs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum JobsFormat {
    /// Human-readable table layout (default).
    #[default]
    Table,
    /// JSON view model output.
    Json,
}

/// Execute the `dvb jobs` command.
///
/// # Errors
///
/// Returns [`Error::Config`] if the configuration is missing or invalid,
/// or [`Error::Storage`] on storage initialization failure.
pub async fn run_jobs(
    config_path: &Path,
    format: JobsFormat,
    remote: bool,
    remote_timeout: Duration,
    now: Option<DateTime<Utc>>,
) -> Result<u8> {
    if !config_path.exists() {
        return Err(Error::Config(ConfigError::Invalid(format!(
            "config not found at {}; run \"dvb init\" to create one",
            config_path.display()
        ))));
    }

    let config = Config::load(config_path, ValidationMode::Static)?;
    let now = now.unwrap_or_else(Utc::now);

    if config.jobs.is_empty() {
        if format == JobsFormat::Table {
            println!("no jobs configured");
        } else {
            let output = JobsOutput {
                schema_version: 1,
                generated_at: now,
                jobs: Vec::new(),
            };
            let json_str = render_json(&output, std::io::stdout().is_terminal())?;
            println!("{json_str}");
        }
        return Ok(EXIT_SUCCESS);
    }

    let mut job_summaries = summary::summarize(&config, now);
    let mut any_remote_error = false;

    if remote {
        storage::install_transport()?;
        let remote_map = query_all_remotes(&config.jobs, remote_timeout).await;

        for job_summary in &mut job_summaries {
            if let Some(remote_summary) = remote_map.get(&job_summary.name) {
                if remote_summary.error.is_some() {
                    any_remote_error = true;
                }
                job_summary.remote = Some(remote_summary.clone());
            }
        }
    }

    match format {
        JobsFormat::Table => {
            let table = render_table(&job_summaries, remote);
            print!("{table}");
        }
        JobsFormat::Json => {
            let output = JobsOutput {
                schema_version: 1,
                generated_at: now,
                jobs: job_summaries,
            };
            let is_tty = std::io::stdout().is_terminal();
            let json_str = render_json(&output, is_tty)?;
            println!("{json_str}");
        }
    }

    if remote && any_remote_error {
        Ok(EXIT_PARTIAL)
    } else {
        Ok(EXIT_SUCCESS)
    }
}

async fn query_all_remotes(
    jobs: &[JobConfig],
    remote_timeout: Duration,
) -> HashMap<String, RemoteSummary> {
    let tasks = jobs.iter().map(|job| {
        let job = job.clone();
        async move { query_remote_job(&job, remote_timeout).await }
    });

    let mut stream = futures_util::stream::iter(tasks).buffer_unordered(4);
    let mut map = HashMap::new();
    while let Some((name, summary)) = stream.next().await {
        map.insert(name, summary);
    }
    map
}

async fn query_remote_job(job: &JobConfig, remote_timeout: Duration) -> (String, RemoteSummary) {
    let backend = job.storage.kind();
    let query_fut = async {
        let op = storage::operator(&job.storage)?;
        let (backups, _ignored) = retention::list_backups(&op, job).await?;
        let count = backups.len() as u64;
        let total_bytes = backups.iter().map(|b| b.size).sum::<u64>();
        let last_backup = backups.first().map(|b| b.timestamp.to_rfc3339());
        Ok::<_, Error>((count, total_bytes, last_backup))
    };

    match tokio::time::timeout(remote_timeout, query_fut).await {
        Ok(Ok((count, total_bytes, last_backup))) => (
            job.name.clone(),
            RemoteSummary {
                last_backup,
                count,
                total_bytes,
                error: None,
            },
        ),
        Ok(Err(err)) => {
            let sanitized = sanitize_storage_error(&err, backend);
            (
                job.name.clone(),
                RemoteSummary {
                    last_backup: None,
                    count: 0,
                    total_bytes: 0,
                    error: Some(sanitized),
                },
            )
        }
        Err(_) => (
            job.name.clone(),
            RemoteSummary {
                last_backup: None,
                count: 0,
                total_bytes: 0,
                error: Some(format!("timed out querying {backend} storage")),
            },
        ),
    }
}

fn sanitize_storage_error(err: &Error, backend: &'static str) -> String {
    match err {
        Error::Storage { source, .. } => {
            format!("{backend} storage error: {}", source.kind())
        }
        Error::StorageConfig(_) => {
            format!("invalid {backend} storage configuration")
        }
        _ => {
            format!("{backend} query failed")
        }
    }
}

/// Format the sources column: truncated to first path +N when multiple paths exist.
#[must_use]
pub fn format_sources(sources: &[String]) -> String {
    match sources.len() {
        0 => "-".to_string(),
        1 => sources[0].clone(),
        n => format!("{}, +{}", sources[0], n - 1),
    }
}

/// Render a human-readable table of jobs.
#[must_use]
pub fn render_table(jobs: &[JobSummary], remote: bool) -> String {
    render_table_styled(jobs, remote, summary::stdout_is_colored_terminal())
}

/// Render a table of jobs, choosing between UTF8 borders and plain style.
#[must_use]
pub fn render_table_styled(jobs: &[JobSummary], remote: bool, colored_terminal: bool) -> String {
    let mut table = comfy_table::Table::new();
    if colored_terminal {
        table.load_style(comfy_table::presets::UTF8_FULL);
    } else {
        table.load_style(comfy_table::presets::NOTHING);
    }
    table.set_content_arrangement(comfy_table::ContentArrangement::Disabled);

    if remote {
        table.set_header([
            "NAME",
            "SCHEDULE",
            "NEXT RUN",
            "STORAGE",
            "RETENTION",
            "SOURCES",
            "LAST BACKUP",
            "COUNT",
            "SIZE",
        ]);
    } else {
        table.set_header([
            "NAME",
            "SCHEDULE",
            "NEXT RUN",
            "STORAGE",
            "RETENTION",
            "SOURCES",
        ]);
    }

    for job in jobs {
        let schedule = job.crontext.as_deref().unwrap_or(&job.cron);
        let next_run = job.next_run.map_or_else(
            || "-".to_string(),
            |dt| dt.format("%Y-%m-%d %H:%M %:z").to_string(),
        );
        let storage_loc = job.storage.location();
        let retention_str = format!("{}d, keep>={}", job.retention_days, job.min_keep);
        let sources_str = format_sources(&job.source);

        if remote {
            let (last_backup, count, size) = match &job.remote {
                Some(r) if r.error.is_some() => (
                    "error".to_string(),
                    "-".to_string(),
                    r.error.clone().unwrap_or_default(),
                ),
                Some(r) => (
                    r.last_backup.clone().unwrap_or_else(|| "-".to_string()),
                    r.count.to_string(),
                    summary::format_bytes(r.total_bytes),
                ),
                None => ("-".to_string(), "-".to_string(), "-".to_string()),
            };

            table.add_row([
                &job.name,
                schedule,
                &next_run,
                &storage_loc,
                &retention_str,
                &sources_str,
                &last_backup,
                &count,
                &size,
            ]);
        } else {
            table.add_row([
                &job.name,
                schedule,
                &next_run,
                &storage_loc,
                &retention_str,
                &sources_str,
            ]);
        }
    }

    format!("{table}\n")
}

/// Serialize [`JobsOutput`] to JSON.
///
/// # Errors
///
/// Returns [`Error::Config`] on serialization failure.
pub fn render_json(output: &JobsOutput, pretty: bool) -> Result<String> {
    if pretty {
        serde_json::to_string_pretty(output)
            .map_err(|err| Error::Config(ConfigError::Invalid(err.to_string())))
    } else {
        serde_json::to_string(output)
            .map_err(|err| Error::Config(ConfigError::Invalid(err.to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    #[test]
    fn format_sources_tests() {
        assert_eq!(format_sources(&[]), "-");
        assert_eq!(format_sources(&["/data".to_string()]), "/data");
        assert_eq!(
            format_sources(&["/data1".to_string(), "/data2".to_string()]),
            "/data1, +1"
        );
        assert_eq!(
            format_sources(&[
                "/data1".to_string(),
                "/data2".to_string(),
                "/data3".to_string()
            ]),
            "/data1, +2"
        );
    }

    #[test]
    fn format_bytes_tests() {
        assert_eq!(summary::format_bytes(0), "0 B");
        assert_eq!(summary::format_bytes(1023), "1023 B");
        assert_eq!(summary::format_bytes(1024), "1.0 KiB");
        assert_eq!(summary::format_bytes(1536), "1.5 KiB");
        assert_eq!(summary::format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(summary::format_bytes(1024 * 1024 * 1024), "1.0 GiB");
    }

    #[test]
    fn golden_json_output() {
        let fixture_path = Path::new("tests/fixtures/jobs_fixture.toml");
        let config = Config::load(fixture_path, ValidationMode::Static).expect("load fixture");
        let now = Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).unwrap();
        let job_summaries = summary::summarize(&config, now);

        let output = JobsOutput {
            schema_version: 1,
            generated_at: now,
            jobs: job_summaries,
        };
        let json_str = render_json(&output, true).expect("render json");
        let golden_str = include_str!("../tests/golden/jobs_json.golden");
        assert_eq!(json_str, golden_str.trim_end());
    }

    #[test]
    fn golden_table_output() {
        let fixture_path = Path::new("tests/fixtures/jobs_fixture.toml");
        let config = Config::load(fixture_path, ValidationMode::Static).expect("load fixture");
        let now = Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).unwrap();
        let job_summaries = summary::summarize(&config, now);
        let table_str = render_table_styled(&job_summaries, false, false);
        let golden_str = include_str!("../tests/golden/jobs_table.golden");
        assert_eq!(table_str, golden_str);
    }

    #[test]
    fn json_schema_key_and_nesting_check() {
        let fixture_path = Path::new("tests/fixtures/jobs_fixture.toml");
        let config = Config::load(fixture_path, ValidationMode::Static).expect("load fixture");
        let now = Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).unwrap();
        let job_summaries = summary::summarize(&config, now);
        let output = JobsOutput {
            schema_version: 1,
            generated_at: now,
            jobs: job_summaries,
        };
        let json_val: serde_json::Value =
            serde_json::to_value(&output).expect("convert output to value");

        assert_eq!(json_val["schema_version"], 1);
        assert_eq!(json_val["generated_at"], "2026-10-02T12:00:00Z");

        let jobs = json_val["jobs"].as_array().expect("jobs array");
        assert_eq!(jobs.len(), 3);

        // Job 0 (cron used): crontext is null, cron is set, remote is null
        let j0 = &jobs[0];
        assert_eq!(j0["name"], "db");
        assert_eq!(j0["cron"], "0 18 * * 5");
        assert!(j0["crontext"].is_null());
        assert!(j0["remote"].is_null());
        assert_eq!(j0["storage"]["type"], "s3");
        assert_eq!(j0["storage"]["bucket"], "bk");
        assert_eq!(j0["storage"]["region"], "eu-west-1");
        assert_eq!(j0["storage"]["prefix"], "db/");
        assert_eq!(j0["pre_backup"]["cmd"], serde_json::json!(["pg_dump"]));
        assert_eq!(j0["pre_backup"]["container"], "postgres");
        assert_eq!(
            j0["post_backup"]["cmd"],
            serde_json::json!(["/bin/notify.sh"])
        );
        assert_eq!(j0["post_backup"]["run_on"], "always");
        assert_eq!(j0["post_restore"]["cmd"], serde_json::json!(["pg_restore"]));
        assert!(j0["pre_restore"].is_null());

        // Job 1 (sftp): crontext is null, sftp keys matched
        let j1 = &jobs[1];
        assert_eq!(j1["name"], "logs");
        assert_eq!(j1["cron"], "0 0 * * *");
        assert!(j1["crontext"].is_null());
        assert_eq!(j1["storage"]["type"], "sftp");
        assert_eq!(j1["storage"]["endpoint"], "sftp.example.com:22");
        assert_eq!(j1["storage"]["user"], "backupuser");
        assert_eq!(j1["storage"]["root"], "/remote/logs");
        assert_eq!(j1["storage"]["known_hosts_strategy"], "strict");

        // Job 2 (crontext used): both crontext and resolved cron set, remote is null
        let j2 = &jobs[2];
        assert_eq!(j2["name"], "web");
        assert_eq!(j2["cron"], "0 18 * * 5");
        assert_eq!(j2["crontext"], "every friday at 18:00");
        assert!(j2["remote"].is_null());
        assert_eq!(j2["storage"]["type"], "fs");
        assert_eq!(j2["storage"]["root"], "/backup/storage");
        assert_eq!(j2["storage"]["prefix"], "web");
        assert_eq!(
            j2["source"],
            serde_json::json!(["/var/www/html", "/var/www/uploads"])
        );
    }
}
