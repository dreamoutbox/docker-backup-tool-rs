//! Secret-free summary models for configured jobs.

use std::io::IsTerminal;

use chrono::{DateTime, FixedOffset, Utc};
use serde::Serialize;

use crate::config::{
    self, Compression, Config, HookConfig, KnownHostsStrategy, RunOn, ScheduleSource, StorageConfig,
};
use crate::scheduler;

/// Secret-free view model representing a single configured job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobSummary {
    pub name: String,
    /// Effective 5-field cron expression (resolved from crontext when used).
    pub cron: String,
    /// Original crontext text; `None` when the job uses `cron`.
    pub crontext: Option<String>,
    /// Effective timezone (job `timezone`, else process TZ, else UTC).
    pub timezone: String,
    pub source: Vec<String>,
    pub filename: String,
    pub compression: String,
    pub retention_days: u32,
    pub min_keep: u32,
    pub stop_containers: Vec<String>,
    /// Backend type and non-secret configuration parameters.
    pub storage: StorageSummary,
    pub pre: Vec<HookSummary>,
    pub post: Vec<HookSummary>,
    /// Next scheduled execution time in the job's timezone.
    pub next_run: Option<DateTime<FixedOffset>>,
    /// Remote storage statistics (populated only when `--remote` is requested).
    pub remote: Option<RemoteSummary>,
}

/// Secret-free remote storage status for a configured job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteSummary {
    pub last_backup: Option<String>,
    pub count: u64,
    pub total_bytes: u64,
    pub error: Option<String>,
}

/// JSON view model emitted by `dvb jobs --format json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobsOutput {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub jobs: Vec<JobSummary>,
}

/// Secret-free storage configuration summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StorageSummary {
    Fs {
        root: String,
        #[serde(skip_serializing_if = "String::is_empty")]
        prefix: String,
    },
    S3 {
        bucket: String,
        region: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
        #[serde(skip_serializing_if = "String::is_empty")]
        prefix: String,
    },
    Sftp {
        endpoint: String,
        user: String,
        root: String,
        known_hosts_strategy: KnownHostsStrategy,
    },
    Dropbox {
        root: String,
    },
}

impl StorageSummary {
    /// Build a summary view from a storage configuration, omitting all credentials and secret paths.
    #[must_use]
    pub fn from_config(config: &StorageConfig) -> Self {
        match config {
            StorageConfig::Fs(cfg) => Self::Fs {
                root: cfg.root.display().to_string(),
                prefix: cfg.prefix.clone(),
            },
            StorageConfig::S3(cfg) => Self::S3 {
                bucket: cfg.bucket.clone(),
                region: cfg.region.clone(),
                endpoint: cfg.endpoint.clone(),
                prefix: cfg.prefix.clone(),
            },
            StorageConfig::Sftp(cfg) => Self::Sftp {
                endpoint: cfg.endpoint.clone(),
                user: cfg.user.clone(),
                root: cfg.root.clone(),
                known_hosts_strategy: cfg.known_hosts_strategy,
            },
            StorageConfig::Dropbox(cfg) => Self::Dropbox {
                root: cfg.root.clone(),
            },
        }
    }

    /// Render the human-readable storage location string (e.g. for table output).
    #[must_use]
    pub fn location(&self) -> String {
        match self {
            Self::Fs { root, .. } => {
                let clean = root.trim_start_matches('/');
                format!("file:///{clean}")
            }
            Self::S3 { bucket, prefix, .. } => {
                let clean = prefix.trim_start_matches('/');
                if clean.is_empty() {
                    format!("s3://{bucket}")
                } else {
                    format!("s3://{bucket}/{clean}")
                }
            }
            Self::Sftp {
                endpoint,
                user,
                root,
                ..
            } => {
                let clean = root.trim_start_matches('/');
                format!("sftp://{user}@{endpoint}/{clean}")
            }
            Self::Dropbox { root } => {
                let clean = root.trim_start_matches('/');
                format!("dropbox:/{clean}")
            }
        }
    }
}

/// Secret-free hook configuration summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookSummary {
    /// Only the executable name is preserved; arguments are discarded to avoid leaking secrets.
    pub cmd: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    pub timeout_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_on: Option<RunOn>,
}

impl HookSummary {
    /// Build a pre-hook summary. Pre-hooks do not specify `run_on`.
    #[must_use]
    pub fn from_pre_hook(hook: &HookConfig) -> Self {
        Self {
            cmd: hook.cmd.first().cloned().into_iter().collect(),
            container: hook.container.clone(),
            timeout_secs: hook.timeout_secs,
            run_on: None,
        }
    }

    /// Build a post-hook summary.
    #[must_use]
    pub fn from_post_hook(hook: &HookConfig) -> Self {
        Self {
            cmd: hook.cmd.first().cloned().into_iter().collect(),
            container: hook.container.clone(),
            timeout_secs: hook.timeout_secs,
            run_on: Some(hook.run_on),
        }
    }
}

/// Generate secret-free summaries for all configured jobs relative to `now`.
#[must_use]
pub fn summarize(cfg: &Config, now: DateTime<Utc>) -> Vec<JobSummary> {
    let default_tz = scheduler::resolve_timezone().unwrap_or(chrono_tz::UTC);
    cfg.jobs
        .iter()
        .map(|job| {
            let tz = job.effective_timezone(default_tz);
            let next_run = job.cron.as_deref().and_then(|cron_str| {
                let parsed = config::parse_cron(cron_str).ok()?;
                let next_utc = scheduler::next_fire_time(&parsed, &tz, now, None).ok()?;
                Some(next_utc.with_timezone(&tz).fixed_offset())
            });

            JobSummary {
                name: job.name.clone(),
                cron: job.cron.clone().unwrap_or_default(),
                crontext: match &job.schedule_source {
                    Some(ScheduleSource::Crontext(text)) => Some(text.clone()),
                    _ => None,
                },
                timezone: tz.name().to_owned(),
                source: job.source.iter().map(|p| p.display().to_string()).collect(),
                filename: job.filename.clone(),
                compression: match job.compression {
                    Compression::Zstd => "zstd",
                    Compression::Gzip => "gzip",
                    Compression::None => "none",
                }
                .to_owned(),
                retention_days: job.retention_days,
                min_keep: job.min_keep,
                stop_containers: job.stop_containers.clone(),
                storage: StorageSummary::from_config(&job.storage),
                pre: job.pre.iter().map(HookSummary::from_pre_hook).collect(),
                post: job.post.iter().map(HookSummary::from_post_hook).collect(),
                next_run,
                remote: None,
            }
        })
        .collect()
}

/// Return whether stdout is an interactive colored terminal with `NO_COLOR` unset/empty.
#[must_use]
pub fn stdout_is_colored_terminal() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|val| val.is_empty())
}

/// Format byte count into human-readable representation.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        let whole = bytes / KIB;
        let frac = (bytes % KIB) * 10 / KIB;
        format!("{whole}.{frac} KiB")
    } else if bytes < GIB {
        let whole = bytes / MIB;
        let frac = (bytes % MIB) * 10 / MIB;
        format!("{whole}.{frac} MiB")
    } else if bytes < TIB {
        let whole = bytes / GIB;
        let frac = (bytes % GIB) * 10 / GIB;
        format!("{whole}.{frac} GiB")
    } else {
        let whole = bytes / TIB;
        let frac = (bytes % TIB) * 10 / TIB;
        format!("{whole}.{frac} TiB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        DockerConfig, DropboxConfig, FsConfig, HookConfig, JobConfig, KnownHostsStrategy, S3Config,
        SecretString, SftpConfig,
    };
    use std::path::PathBuf;

    fn s3_sentinel_job(key: &str, secret: &str, hook_arg: &str) -> JobConfig {
        JobConfig {
            name: "job-s3".to_owned(),
            cron: Some("0 3 * * *".to_owned()),
            crontext: None,
            timezone: Some("UTC".to_owned()),
            schedule_source: Some(ScheduleSource::Cron),
            source: vec![PathBuf::from("/data")],
            filename: "backup-%Y%m%dT%H%M%SZ".to_owned(),
            compression: Compression::Zstd,
            retention_days: 14,
            min_keep: 3,
            stop_containers: vec![],
            stop_label: None,
            stop_timeout_secs: 30,
            follow_symlinks: false,
            storage: StorageConfig::S3(S3Config {
                bucket: "my-bucket".to_owned(),
                region: "us-east-1".to_owned(),
                endpoint: Some("https://s3.example.com".to_owned()),
                prefix: "backups/".to_owned(),
                access_key_id: Some(SecretString::new(key)),
                secret_access_key: Some(SecretString::new(secret)),
                force_path_style: true,
            }),
            pre: vec![HookConfig {
                cmd: vec!["dump.sh".to_owned(), hook_arg.to_owned()],
                run_on: RunOn::Success,
                container: None,
                timeout_secs: 30,
            }],
            post: vec![],
            run_on_start: false,
            restore: None,
        }
    }

    fn sftp_sentinel_job(key: &str) -> JobConfig {
        JobConfig {
            name: "job-sftp".to_owned(),
            cron: Some("0 4 * * *".to_owned()),
            crontext: None,
            timezone: Some("UTC".to_owned()),
            schedule_source: Some(ScheduleSource::Cron),
            source: vec![PathBuf::from("/data")],
            filename: "backup-%Y%m%dT%H%M%SZ".to_owned(),
            compression: Compression::Zstd,
            retention_days: 14,
            min_keep: 3,
            stop_containers: vec![],
            stop_label: None,
            stop_timeout_secs: 30,
            follow_symlinks: false,
            storage: StorageConfig::Sftp(SftpConfig {
                endpoint: "sftp.example.com:22".to_owned(),
                user: "backupuser".to_owned(),
                root: "/remote/backups".to_owned(),
                key_path: Some(PathBuf::from(format!("/secrets/{key}"))),
                known_hosts_strategy: KnownHostsStrategy::Strict,
            }),
            pre: vec![],
            post: vec![],
            run_on_start: false,
            restore: None,
        }
    }

    fn dropbox_sentinel_job(id: &str, secret: &str, refresh: &str) -> JobConfig {
        JobConfig {
            name: "job-dropbox".to_owned(),
            cron: Some("0 5 * * *".to_owned()),
            crontext: None,
            timezone: Some("UTC".to_owned()),
            schedule_source: Some(ScheduleSource::Cron),
            source: vec![PathBuf::from("/data")],
            filename: "backup-%Y%m%dT%H%M%SZ".to_owned(),
            compression: Compression::Zstd,
            retention_days: 14,
            min_keep: 3,
            stop_containers: vec![],
            stop_label: None,
            stop_timeout_secs: 30,
            follow_symlinks: false,
            storage: StorageConfig::Dropbox(DropboxConfig {
                root: "/dbx/backups".to_owned(),
                client_id: SecretString::new(id),
                client_secret: SecretString::new(secret),
                refresh_token: SecretString::new(refresh),
            }),
            pre: vec![],
            post: vec![],
            run_on_start: false,
            restore: None,
        }
    }

    fn build_sentinel_config(sentinels: &[&str; 7]) -> Config {
        let [
            s3_key,
            s3_secret,
            sftp_key,
            db_id,
            db_secret,
            db_refresh,
            hook_arg,
        ] = *sentinels;
        Config {
            docker: DockerConfig::default(),
            shutdown_grace_secs: 60,
            jobs: vec![
                s3_sentinel_job(s3_key, s3_secret, hook_arg),
                sftp_sentinel_job(sftp_key),
                dropbox_sentinel_job(db_id, db_secret, db_refresh),
            ],
        }
    }

    #[test]
    fn secret_leak_prevention() {
        let sentinels = [
            "SENTINEL_SECRET_S3_KEY_12345",
            "SENTINEL_SECRET_S3_SECRET_67890",
            "SENTINEL_SECRET_SFTP_KEY_PATH",
            "SENTINEL_SECRET_DROPBOX_CLIENT_ID",
            "SENTINEL_SECRET_DROPBOX_CLIENT_SECRET",
            "SENTINEL_SECRET_DROPBOX_REFRESH_TOKEN",
            "SENTINEL_SECRET_HOOK_ARGUMENT",
        ];

        let config = build_sentinel_config(&sentinels);
        let now = DateTime::from_timestamp(1_790_942_400, 0).expect("valid timestamp");
        let summaries = summarize(&config, now);

        let json = serde_json::to_string(&summaries).expect("serialize to json");
        let debug = format!("{summaries:?}");

        for sentinel in sentinels {
            assert!(
                !json.contains(sentinel),
                "secret leak in JSON output: found {sentinel}"
            );
            assert!(
                !debug.contains(sentinel),
                "secret leak in Debug output: found {sentinel}"
            );
        }
    }

    #[test]
    fn next_run_utc_and_berlin() {
        // 2026-10-02 12:00:00 UTC (Friday)
        let now = DateTime::from_timestamp(1_790_942_400, 0).expect("valid timestamp");

        let config = Config {
            docker: DockerConfig::default(),
            shutdown_grace_secs: 60,
            jobs: vec![
                JobConfig {
                    name: "utc-job".to_owned(),
                    cron: Some("0 18 * * 5".to_owned()),
                    crontext: None,
                    timezone: Some("UTC".to_owned()),
                    schedule_source: Some(ScheduleSource::Cron),
                    source: vec![PathBuf::from("/data")],
                    filename: "backup-%Y%m%dT%H%M%SZ".to_owned(),
                    compression: Compression::Zstd,
                    retention_days: 14,
                    min_keep: 3,
                    stop_containers: vec![],
                    stop_label: None,
                    stop_timeout_secs: 30,
                    follow_symlinks: false,
                    storage: StorageConfig::Fs(FsConfig {
                        root: PathBuf::from("/backup"),
                        prefix: String::new(),
                    }),
                    pre: vec![],
                    post: vec![],
                    run_on_start: false,
                    restore: None,
                },
                JobConfig {
                    name: "berlin-job".to_owned(),
                    cron: Some("0 18 * * 5".to_owned()),
                    crontext: Some("every friday at 18:00".to_owned()),
                    timezone: Some("Europe/Berlin".to_owned()),
                    schedule_source: Some(ScheduleSource::Crontext(
                        "every friday at 18:00".to_owned(),
                    )),
                    source: vec![PathBuf::from("/data")],
                    filename: "backup-%Y%m%dT%H%M%SZ".to_owned(),
                    compression: Compression::Zstd,
                    retention_days: 14,
                    min_keep: 3,
                    stop_containers: vec![],
                    stop_label: None,
                    stop_timeout_secs: 30,
                    follow_symlinks: false,
                    storage: StorageConfig::Fs(FsConfig {
                        root: PathBuf::from("/backup"),
                        prefix: String::new(),
                    }),
                    pre: vec![],
                    post: vec![],
                    run_on_start: false,
                    restore: None,
                },
            ],
        };

        let summaries = summarize(&config, now);
        assert_eq!(summaries.len(), 2);

        // UTC job
        let utc_summary = &summaries[0];
        assert_eq!(utc_summary.name, "utc-job");
        assert_eq!(utc_summary.timezone, "UTC");
        assert_eq!(utc_summary.cron, "0 18 * * 5");
        assert_eq!(utc_summary.crontext, None);
        let utc_next = utc_summary.next_run.expect("next_run computed");
        assert_eq!(utc_next.to_rfc3339(), "2026-10-02T18:00:00+00:00");

        // Berlin job (CEST is UTC+2 in October)
        let berlin_summary = &summaries[1];
        assert_eq!(berlin_summary.name, "berlin-job");
        assert_eq!(berlin_summary.timezone, "Europe/Berlin");
        assert_eq!(berlin_summary.cron, "0 18 * * 5");
        assert_eq!(
            berlin_summary.crontext.as_deref(),
            Some("every friday at 18:00")
        );
        let berlin_next = berlin_summary.next_run.expect("next_run computed");
        assert_eq!(berlin_next.to_rfc3339(), "2026-10-02T18:00:00+02:00");
    }

    #[test]
    fn storage_location_formatting() {
        let fs = StorageSummary::Fs {
            root: "/backup/data".to_owned(),
            prefix: String::new(),
        };
        assert_eq!(fs.location(), "file:///backup/data");

        let s3_with_prefix = StorageSummary::S3 {
            bucket: "my-bucket".to_owned(),
            region: "us-east-1".to_owned(),
            endpoint: None,
            prefix: "db/".to_owned(),
        };
        assert_eq!(s3_with_prefix.location(), "s3://my-bucket/db/");

        let s3_without_prefix = StorageSummary::S3 {
            bucket: "my-bucket".to_owned(),
            region: "us-east-1".to_owned(),
            endpoint: None,
            prefix: String::new(),
        };
        assert_eq!(s3_without_prefix.location(), "s3://my-bucket");

        let sftp = StorageSummary::Sftp {
            endpoint: "sftp.internal:2222".to_owned(),
            user: "backup".to_owned(),
            root: "/var/backups".to_owned(),
            known_hosts_strategy: KnownHostsStrategy::Strict,
        };
        assert_eq!(
            sftp.location(),
            "sftp://backup@sftp.internal:2222/var/backups"
        );

        let dropbox = StorageSummary::Dropbox {
            root: "/backups/db".to_owned(),
        };
        assert_eq!(dropbox.location(), "dropbox:/backups/db");
    }
}
