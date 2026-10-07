//! Configuration types, loading and validation.
//!
//! The config file is TOML. Values can be overridden from the environment with
//! the `DVB__` prefix and `__` as the nesting separator, e.g.
//! `DVB__JOB__0__NAME=db`. Secrets additionally accept a `*_FILE` sibling key
//! pointing at a file whose first line (trailing newline stripped) is the
//! value, which keeps them out of `docker inspect` output and process listings.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone as _, Utc};
use figment::providers::{Env, Format, Toml};
use figment::value::{Dict, Map, Value};
use figment::{Figment, Metadata, Profile, Provider};
use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Error, Result};

/// Provider that normalizes `[[job]]` array into indexed dictionary elements so
/// environment variables such as `DVB__JOB__0__CRONTEXT` merge recursively.
struct NormalizedToml<P>(P);

impl<P: AsRef<Path>> Provider for NormalizedToml<P> {
    fn metadata(&self) -> Metadata {
        Toml::file(self.0.as_ref()).metadata()
    }

    fn data(&self) -> std::result::Result<Map<Profile, Dict>, figment::Error> {
        let mut map = Toml::file(self.0.as_ref()).data()?;
        for dict in map.values_mut() {
            if let Some(Value::Array(tag, jobs)) = dict.remove("job") {
                let mut job_map = Dict::new();
                for (i, job) in jobs.into_iter().enumerate() {
                    job_map.insert(i.to_string(), job);
                }
                dict.insert("job".to_owned(), Value::Dict(tag, job_map));
            }
        }
        Ok(map)
    }
}

/// Default location of the configuration file inside the container image.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/dvb/config.toml";

/// Environment prefix for configuration overrides.
pub const ENV_PREFIX: &str = "DVB__";

/// Environment variable holding the filter directives for `tracing`.
pub const ENV_LOG_FILTER: &str = "DVB_LOG";

/// The whole configuration file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Docker settings. Read by `docker.rs` from phase 3 onwards.
    #[allow(dead_code)]
    #[serde(default)]
    pub docker: DockerConfig,

    /// Seconds to wait for running jobs to finish when shutting down before cancelling them.
    #[serde(default = "default_shutdown_grace_secs")]
    pub shutdown_grace_secs: u64,

    #[serde(rename = "job", default, deserialize_with = "deserialize_jobs")]
    pub jobs: Vec<JobConfig>,
}

fn deserialize_jobs<'de, D>(deserializer: D) -> std::result::Result<Vec<JobConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::collections::BTreeMap;

    struct JobsVisitor;

    impl<'de> Visitor<'de> for JobsVisitor {
        type Value = Vec<JobConfig>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a sequence of jobs or an indexed map of jobs")
        }

        fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut jobs = Vec::new();
            while let Some(job) = seq.next_element()? {
                jobs.push(job);
            }
            Ok(jobs)
        }

        fn visit_map<M>(self, mut map: M) -> std::result::Result<Self::Value, M::Error>
        where
            M: de::MapAccess<'de>,
        {
            let mut indexed_jobs: BTreeMap<String, JobConfig> = BTreeMap::new();
            while let Some((key, value)) = map.next_entry()? {
                indexed_jobs.insert(key, value);
            }
            let mut pairs: Vec<(usize, JobConfig)> = indexed_jobs
                .into_iter()
                .filter_map(|(k, v)| k.parse::<usize>().ok().map(|idx| (idx, v)))
                .collect();
            pairs.sort_by_key(|(idx, _)| *idx);
            Ok(pairs.into_iter().map(|(_, job)| job).collect())
        }
    }

    deserializer.deserialize_any(JobsVisitor)
}

/// Docker socket settings. Absent socket disables stop/exec features.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerConfig {
    /// Path to the Docker unix socket. `None` disables container control.
    #[serde(default)]
    pub socket: Option<PathBuf>,
}

pub use crontext::{self, CrontextError, Schedule};

/// Origin of a job's schedule (raw cron vs natural-language crontext).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleSource {
    /// Schedule was supplied directly as a 5-field cron expression.
    Cron,
    /// Schedule was supplied as a human-friendly string and resolved to cron.
    Crontext(String),
}

/// One backup job.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobConfig {
    /// Unique job name, used by `--job`, the lock file and `DVB_JOB`.
    pub name: String,

    /// Standard five field cron expression, evaluated in the `TZ` of the process.
    #[serde(default)]
    pub cron: Option<String>,

    /// Natural language schedule (e.g. `every 15 minutes`, `every friday at 18:00`).
    #[serde(default)]
    pub crontext: Option<String>,

    /// Optional timezone override for this job (e.g. `America/New_York`, `UTC`).
    #[serde(default)]
    pub timezone: Option<String>,

    /// Origin of the resolved schedule, populated during validation.
    #[serde(skip)]
    pub schedule_source: Option<ScheduleSource>,

    /// Paths inside the backup container to archive.
    pub source: Vec<PathBuf>,

    /// `chrono` strftime template for the remote object name.
    pub filename: String,

    #[serde(default)]
    pub compression: Compression,

    /// Delete backups older than this many days.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,

    /// Never delete below this many backups, regardless of age.
    #[serde(default = "default_min_keep")]
    pub min_keep: u32,

    /// Container names to stop for consistency, then restart.
    #[serde(default)]
    pub stop_containers: Vec<String>,

    /// Label selector (`key=value`) matching containers to stop.
    #[serde(default)]
    pub stop_label: Option<String>,

    /// Seconds to wait for containers to stop cleanly.
    #[serde(default = "default_stop_timeout")]
    pub stop_timeout_secs: u64,

    /// Store symlinks as symlinks instead of following them.
    #[serde(default)]
    pub follow_symlinks: bool,

    /// Where the archive is uploaded.
    pub storage: StorageConfig,

    #[serde(default)]
    pub pre: Vec<HookConfig>,

    #[serde(default)]
    pub post: Vec<HookConfig>,

    /// Run this job once at daemon start, before its first scheduled run.
    #[serde(default)]
    pub run_on_start: bool,

    /// Optional defaults for `dvb restore <job>`.
    #[serde(default)]
    pub restore: Option<RestoreConfig>,
}

/// Optional default settings for restoring a job's backup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreConfig {
    /// Default extraction target directory when no script is specified.
    #[serde(default)]
    pub dir: Option<PathBuf>,

    /// Optional default restore script to execute.
    #[serde(default)]
    pub script: Option<PathBuf>,

    /// Timeout in seconds for the restore script.
    #[serde(default = "default_script_timeout")]
    pub script_timeout_secs: u64,
}

const fn default_script_timeout() -> u64 {
    3600
}

/// Default grace period (in seconds) to wait for running jobs to finish on shutdown.
pub const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 60;

const fn default_shutdown_grace_secs() -> u64 {
    DEFAULT_SHUTDOWN_GRACE_SECS
}

const fn default_retention_days() -> u32 {
    14
}

const fn default_min_keep() -> u32 {
    3
}

const fn default_stop_timeout() -> u64 {
    30
}

const fn default_true() -> bool {
    true
}

/// Archive compression algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    /// zstd, the default: fast with a good ratio.
    #[default]
    Zstd,
    /// gzip, for backends or tooling that need it.
    Gzip,
    /// No compression at all.
    None,
}

impl Compression {
    /// File extension that matches this compression, if any.
    #[must_use]
    pub const fn extension(self) -> Option<&'static str> {
        match self {
            Self::Zstd => Some("zst"),
            Self::Gzip => Some("gz"),
            Self::None => None,
        }
    }

    /// zstd compression level used for [`Compression::Zstd`].
    const ZSTD_LEVEL: i32 = 3;

    /// gzip compression level used for [`Compression::Gzip`].
    const GZIP_LEVEL: u32 = 6;

    /// zstd level used for [`Compression::Zstd`], as accepted by
    /// `zstd::stream::write::Encoder`.
    #[must_use]
    pub const fn zstd_level() -> i32 {
        Self::ZSTD_LEVEL
    }

    /// gzip level used for [`Compression::Gzip`], as accepted by
    /// `flate2::write::GzEncoder`.
    #[must_use]
    pub const fn gzip_level() -> u32 {
        Self::GZIP_LEVEL
    }
}

/// A pre or post hook.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    /// Argument vector, executed without a shell.
    pub cmd: Vec<String>,

    /// When this hook should run (only meaningful for post hooks).
    #[serde(default)]
    pub run_on: RunOn,

    /// Container to run the hook in; `None` runs it locally.
    #[serde(default)]
    pub container: Option<String>,

    /// Seconds before the hook is killed.
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
}

const fn default_hook_timeout() -> u64 {
    300
}

/// When a post hook runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunOn {
    /// Only when the backup succeeded (default).
    #[default]
    Success,
    /// Only when the backup failed.
    Failure,
    /// Always.
    Always,
}

/// Where backups are uploaded. Internally tagged on `type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StorageConfig {
    /// Local filesystem, used for tests and NAS mounts.
    Fs(FsConfig),
    /// S3 compatible object storage.
    S3(S3Config),
    /// SFTP server.
    Sftp(SftpConfig),
    /// Dropbox.
    Dropbox(DropboxConfig),
}

impl StorageConfig {
    /// Backend name, for logs and error messages.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Fs(_) => "fs",
            Self::S3(_) => "s3",
            Self::Sftp(_) => "sftp",
            Self::Dropbox(_) => "dropbox",
        }
    }

    /// Key prefix prepended to every object name for this job.
    #[must_use]
    pub fn prefix(&self) -> &str {
        match self {
            Self::Fs(cfg) => &cfg.prefix,
            Self::S3(cfg) => &cfg.prefix,
            Self::Sftp(cfg) => &cfg.root,
            Self::Dropbox(cfg) => &cfg.prefix,
        }
    }

    /// Full remote path for `object`, honouring the prefix.
    #[must_use]
    pub fn remote_path(&self, object: &str) -> String {
        let prefix = self.prefix().trim_matches('/');
        if prefix.is_empty() {
            object.to_owned()
        } else {
            format!("{prefix}/{object}")
        }
    }

    /// Check that required keys are present and non-empty.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] when required fields are missing or empty.
    pub fn validate_static(&self) -> Result<()> {
        match self {
            Self::Fs(cfg) => {
                if cfg.root.as_os_str().is_empty() {
                    return Err(invalid("fs storage requires a non-empty `root`"));
                }
            }
            Self::S3(cfg) => {
                if cfg.bucket.trim().is_empty() {
                    return Err(invalid("s3 storage requires a non-empty `bucket`"));
                }
                if cfg.region.trim().is_empty() {
                    return Err(invalid("s3 storage requires a non-empty `region`"));
                }
            }
            Self::Sftp(cfg) => {
                if cfg.endpoint.trim().is_empty() {
                    return Err(invalid("sftp storage requires a non-empty `endpoint`"));
                }
                if cfg.user.trim().is_empty() {
                    return Err(invalid("sftp storage requires a non-empty `user`"));
                }
                if cfg.root.trim().is_empty() {
                    return Err(invalid("sftp storage requires a non-empty `root`"));
                }
            }
            Self::Dropbox(cfg) => {
                if cfg.root.trim().is_empty() {
                    return Err(invalid("dropbox storage requires a non-empty `root`"));
                }
            }
        }
        Ok(())
    }
}

/// `fs` backend settings.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FsConfig {
    /// Directory the archives are written to.
    pub root: PathBuf,
    /// Optional key prefix inside `root`.
    #[serde(default)]
    pub prefix: String,
}

/// `s3` backend settings. Credentials fall back to the default AWS chain.
///
/// Parsed and validated in phase 1; the operator is built in phase 2.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    /// Custom endpoint for `MinIO`, R2, Wasabi and similar services.
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub access_key_id: Option<SecretString>,
    #[serde(default)]
    pub secret_access_key: Option<SecretString>,
    /// Address the bucket as a path segment (`endpoint/bucket/key`) rather than
    /// as a subdomain (`bucket.endpoint/key`).
    ///
    /// Path style is the default and is what `MinIO` and most S3-compatible
    /// services need. Set this to `false` for virtual-host style, which some
    /// providers require.
    #[serde(default = "default_true")]
    pub force_path_style: bool,
}

/// `sftp` backend settings.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct SftpConfig {
    /// `host:port`.
    pub endpoint: String,
    /// Remote user.
    pub user: String,
    /// Remote directory that acts as the archive root.
    pub root: String,
    /// Private key file. When absent, `ssh`'s own defaults apply (agent,
    /// `~/.ssh/config`, `~/.ssh/id_*`).
    #[serde(default)]
    pub key_path: Option<PathBuf>,
    /// `strict` (default) or `accept_new`.
    #[serde(default)]
    pub known_hosts_strategy: KnownHostsStrategy,
}

/// How unknown SSH host keys are handled.
///
/// The underlying `openssh` session is always given a check strategy; there is
/// deliberately no way to disable host key verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KnownHostsStrategy {
    /// Refuse to connect to unknown hosts (default).
    #[default]
    Strict,
    /// Add unknown hosts to `known_hosts` on first use.
    AcceptNew,
}

/// `dropbox` backend settings.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DropboxConfig {
    /// Folder in the Dropbox account that acts as the archive root.
    #[serde(default = "default_dropbox_root")]
    pub root: String,
    /// Key prefix prepended to object names inside `root`.
    #[serde(default)]
    pub prefix: String,
    /// Direct access token or developer key.
    #[serde(default)]
    pub token: Option<SecretString>,
    /// Dropbox OAuth client ID (used with `refresh_token`).
    #[serde(default)]
    pub client_id: Option<SecretString>,
    /// Dropbox OAuth client secret (used with `refresh_token`).
    #[serde(default)]
    pub client_secret: Option<SecretString>,
    /// Dropbox OAuth refresh token (for long-lived access).
    #[serde(default)]
    pub refresh_token: Option<SecretString>,
}

fn default_dropbox_root() -> String {
    std::env::var("DROPBOX_ROOT").unwrap_or_else(|_| "/".to_owned())
}

/// A secret string that never shows up in logs.
///
/// `Debug` prints `***`; use [`SecretString::expose`] at the point of use.
#[derive(Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct SecretString(String);

// `expose` is used by the storage backends as they are wired up (phase 2 and 5).
#[allow(dead_code)]
impl SecretString {
    /// Wrap a value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Read the secret. Call sites should be few and obvious.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the secret is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***redacted***")
    }
}

impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***redacted***")
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Validation mode controlling whether filesystem checks are performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationMode {
    /// Schema-level checks only, no filesystem or network operations.
    Static,
    /// Schema-level checks plus filesystem checks (source paths exist, secrets readable, socket present).
    Full,
}

/// Validate whether `name` matches `^[a-z0-9][a-z0-9_-]{0,62}$`.
#[must_use]
pub fn is_valid_job_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl Config {
    /// Load, merge and validate the configuration file at `path`.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Read`] if the file cannot be read, [`ConfigError::Parse`]
    /// if it is not valid TOML or has an unexpected shape, and
    /// [`ConfigError::Invalid`] if it parses but a job does not make sense.
    pub fn load(path: &Path, mode: ValidationMode) -> Result<Self> {
        let figment = Figment::new()
            .merge(NormalizedToml(path))
            .merge(Env::prefixed(ENV_PREFIX).split("__").lowercase(true));

        let mut config: Self = figment.extract().map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

        match mode {
            ValidationMode::Static => config.validate_static()?,
            ValidationMode::Full => {
                config.validate_static()?;
                config.validate_runtime()?;
            }
        }
        Ok(config)
    }

    /// Look up a job by name.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownJob`], listing the configured job names, when no job
    /// matches.
    pub fn job(&self, name: &str) -> Result<&JobConfig> {
        self.jobs
            .iter()
            .find(|job| job.name == name)
            .ok_or_else(|| Error::UnknownJob {
                name: name.to_owned(),
                known: self
                    .jobs
                    .iter()
                    .map(|job| job.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            })
    }

    /// Number of configured jobs. Used by `dvb check` from phase 2 on.
    #[allow(dead_code)]
    #[must_use]
    pub fn job_count(&self) -> usize {
        self.jobs.len()
    }

    /// Schema-level checks only: names unique and matching path-safe rules,
    /// cron/crontext parse, filename contains a second-resolution timestamp,
    /// retention sane, source list non-empty and duplicate basenames rejected.
    ///
    /// Touches no filesystem or network.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] naming the first problem found.
    pub fn validate_static(&mut self) -> Result<()> {
        let mut seen = BTreeSet::new();
        for job in &mut self.jobs {
            if job.name.trim().is_empty() {
                return Err(invalid("job name must not be empty"));
            }
            if !is_valid_job_name(&job.name) {
                return Err(invalid(format!(
                    "job `{}`: invalid job name, must match ^[a-z0-9][a-z0-9_-]{{0,62}}$ (1-63 lowercase alphanumeric, `_` or `-`, starting with alphanumeric)",
                    job.name
                )));
            }
            if !seen.insert(job.name.clone()) {
                return Err(invalid(format!("duplicate job name `{}`", job.name)));
            }
            job.validate_static()?;
            if job.needs_docker() && self.docker.socket.is_none() {
                return Err(invalid(format!(
                    "job `{}` stops containers or runs a container hook, but [docker] socket is not set",
                    job.name
                )));
            }
        }
        Ok(())
    }

    /// Filesystem checks: source paths exist, secret files readable, Docker socket
    /// path present when containers need to be stopped or container hooks executed.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] naming the first missing path or unreadable file.
    pub fn validate_runtime(&self) -> Result<()> {
        if self.jobs.is_empty() {
            return Err(invalid("no [[job]] defined"));
        }
        for job in &self.jobs {
            job.validate_runtime(&self.docker)?;
        }
        Ok(())
    }

    /// Legacy validation method, performing static validation.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] naming the first problem found.
    pub fn validate(&mut self) -> Result<()> {
        self.validate_static()
    }
}

impl JobConfig {
    /// Return the job's effective timezone, falling back to `default_tz`.
    #[must_use]
    pub fn effective_timezone(&self, default_tz: chrono_tz::Tz) -> chrono_tz::Tz {
        if let Some(tz_str) = &self.timezone {
            parse_timezone(tz_str).unwrap_or(default_tz)
        } else {
            default_tz
        }
    }

    fn resolve_and_validate_schedule(&mut self) -> Result<()> {
        match (&self.cron, &self.crontext) {
            (Some(_), Some(_)) => {
                return Err(invalid(format!(
                    "job `{}`: cannot specify both `cron` and `crontext`",
                    self.name
                )));
            }
            (None, None) => {
                return Err(invalid(format!(
                    "job `{}`: must specify either `cron` or `crontext`",
                    self.name
                )));
            }
            (Some(cron), None) => {
                parse_cron(cron).map_err(|err| {
                    invalid(format!("job `{}`: invalid cron `{cron}`: {err}", self.name))
                })?;
                self.schedule_source = Some(ScheduleSource::Cron);
            }
            (None, Some(text)) => {
                let sched = crontext::parse(text).map_err(|err| {
                    invalid(format!(
                        "job `{}`: invalid crontext `{text}`: {err}",
                        self.name
                    ))
                })?;
                parse_cron(&sched.cron).map_err(|err| {
                    invalid(format!(
                        "job `{}`: resolved cron `{}` is invalid: {err}",
                        self.name, sched.cron
                    ))
                })?;
                self.cron = Some(sched.cron);
                self.schedule_source = Some(ScheduleSource::Crontext(text.clone()));
            }
        }

        if let Some(tz_str) = &self.timezone {
            parse_timezone(tz_str).map_err(|err| {
                invalid(format!(
                    "job `{}`: invalid timezone `{tz_str}`: {err}",
                    self.name
                ))
            })?;
        }
        Ok(())
    }

    fn validate_sources(&self) -> Result<()> {
        if self.source.is_empty() {
            return Err(invalid(format!(
                "job `{}`: source must not be empty",
                self.name
            )));
        }

        let mut basenames = BTreeSet::new();
        for source in &self.source {
            let basename = source
                .file_name()
                .ok_or_else(|| {
                    invalid(format!(
                        "job `{}`: source `{}` has no file name component",
                        self.name,
                        source.display()
                    ))
                })?
                .to_string_lossy()
                .into_owned();
            if !basenames.insert(basename) {
                return Err(invalid(format!(
                    "job `{}`: source `{}` duplicates another source's basename",
                    self.name,
                    source.display()
                )));
            }
        }
        Ok(())
    }

    fn validate_hooks(&self) -> Result<()> {
        for (phase, hooks) in [("pre", &self.pre), ("post", &self.post)] {
            for (index, hook) in hooks.iter().enumerate() {
                if hook.cmd.is_empty() {
                    return Err(invalid(format!(
                        "job `{}`: {phase} hook #{index} has an empty cmd",
                        self.name
                    )));
                }
                if hook.cmd.iter().any(String::is_empty) {
                    return Err(invalid(format!(
                        "job `{}`: {phase} hook #{index} has an empty argument",
                        self.name
                    )));
                }
                if hook.timeout_secs == 0 {
                    return Err(invalid(format!(
                        "job `{}`: {phase} hook #{index} timeout_secs must be >= 1",
                        self.name
                    )));
                }
            }
        }
        Ok(())
    }

    /// Schema-level checks for a single job and schedule resolution.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] naming the first problem found, prefixed with
    /// the job name so a bad job among many is easy to spot.
    pub fn validate_static(&mut self) -> Result<()> {
        self.resolve_and_validate_schedule()?;

        if !self.filename.contains("%Y") {
            return Err(invalid(format!(
                "job `{}`: filename template must contain a timestamp (%Y, got `{}`)",
                self.name, self.filename
            )));
        }
        if !self.filename.contains("%S") {
            return Err(invalid(format!(
                "job `{}`: filename template must contain a second-resolution timestamp (%S, got `{}`)",
                self.name, self.filename
            )));
        }

        if self.retention_days < 1 {
            return Err(invalid(format!(
                "job `{}`: retention_days must be >= 1",
                self.name
            )));
        }
        if self.min_keep < 1 {
            return Err(invalid(format!(
                "job `{}`: min_keep must be >= 1",
                self.name
            )));
        }

        self.validate_sources()?;

        if self.stop_timeout_secs == 0 {
            return Err(invalid(format!(
                "job `{}`: stop_timeout_secs must be >= 1",
                self.name
            )));
        }

        self.validate_hooks()?;
        self.storage
            .validate_static()
            .map_err(|err| invalid(format!("job `{}`: {err}", self.name)))?;

        Ok(())
    }

    /// Filesystem checks for this job: sources exist, secrets readable, Docker socket present if needed.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] naming the first missing path or unreadable file.
    pub fn validate_runtime(&self, docker: &DockerConfig) -> Result<()> {
        for source in &self.source {
            if !source.exists() {
                return Err(invalid(format!(
                    "job `{}`: source path `{}` does not exist",
                    self.name,
                    source.display()
                )));
            }
        }

        if let StorageConfig::Sftp(cfg) = &self.storage
            && let Some(key_path) = &cfg.key_path
        {
            if !key_path.exists() {
                return Err(invalid(format!(
                    "job `{}`: sftp key_path `{}` does not exist",
                    self.name,
                    key_path.display()
                )));
            }
            if std::fs::File::open(key_path).is_err() {
                return Err(invalid(format!(
                    "job `{}`: sftp key_path `{}` is not readable",
                    self.name,
                    key_path.display()
                )));
            }
        }

        if self.needs_docker() || self.wants_stop() {
            match &docker.socket {
                Some(socket) if !socket.exists() => {
                    return Err(invalid(format!(
                        "job `{}` stops containers or runs a container hook, but Docker socket `{}` does not exist",
                        self.name,
                        socket.display()
                    )));
                }
                None => {
                    return Err(invalid(format!(
                        "job `{}` stops containers or runs a container hook, but [docker] socket is not set",
                        self.name
                    )));
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Legacy validate method.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] naming the first problem found.
    pub fn validate(&mut self) -> Result<()> {
        self.validate_static()
    }

    /// Whether this job wants containers stopped for consistency.
    #[must_use]
    pub fn wants_stop(&self) -> bool {
        !self.stop_containers.is_empty() || self.stop_label.is_some()
    }

    /// Whether this job needs the Docker socket at all.
    ///
    /// Stops, starts and container hooks all go through the daemon; a job with
    /// none of those runs without one, which is what keeps `[docker]` optional.
    #[must_use]
    pub fn needs_docker(&self) -> bool {
        self.wants_stop()
            || self
                .pre
                .iter()
                .chain(self.post.iter())
                .any(|hook| hook.container.is_some())
    }

    /// Render the remote object name for `now`, in UTC.
    #[must_use]
    pub fn object_name(&self, now: DateTime<Utc>) -> String {
        let rendered = now.format(&self.filename).to_string();
        match self.compression.extension() {
            // The template may already spell out the extension.
            Some(ext) if rendered.ends_with(&format!(".{ext}")) => rendered,
            Some(ext) => format!("{rendered}.{ext}"),
            None => rendered,
        }
    }

    /// The literal prefix that precedes the timestamp in [`Self::filename`].
    #[allow(dead_code)] // read by retention.rs in phase 2
    fn name_prefix(&self) -> &str {
        let end = self
            .filename
            .char_indices()
            .find(|(_, ch)| !(ch.is_ascii_alphanumeric() || *ch == '-'))
            .map_or(self.filename.len(), |(idx, _)| idx);
        &self.filename[..end]
    }

    /// Split a remote object name into the literal prefix and the stem that the
    /// timestamp is parsed from.
    ///
    /// `None` means the name does not belong to this job, which is how unrelated
    /// files under the prefix are left alone.
    #[allow(dead_code)] // read by retention.rs in phase 2
    fn split_object_name<'a>(&self, object: &'a str) -> Option<&'a str> {
        let name = object.rsplit('/').next().unwrap_or(object);

        // `object_name` appends the compression extension unless the template
        // already spells it out, so exactly one `.ext` comes off here.
        let stem = match self.compression.extension() {
            Some(ext) => name.strip_suffix(&format!(".{ext}")).unwrap_or(name),
            None => name,
        };

        let prefix = self.name_prefix();
        if prefix.is_empty() {
            Some(stem)
        } else {
            stem.strip_prefix(prefix)
        }
    }

    /// Parse a remote object name back into the timestamp it encodes.
    ///
    /// Returns `None` when the name does not match the job's template, which is
    /// how unrelated files under the prefix are left alone.
    #[allow(dead_code)] // read by retention.rs in phase 2
    #[must_use]
    pub fn parse_object_name(&self, object: &str) -> Option<DateTime<Utc>> {
        let stem = self.split_object_name(object)?;
        let format = self.stem_format()?;
        if format.is_empty() {
            return None;
        }
        // `NaiveDateTime`, not `DateTime`: the template's `Z` is a literal
        // character, not a chrono offset specifier, and `DateTime::parse_from_str`
        // would demand a real `%z`. Object names are always rendered in UTC by
        // `object_name`, so interpreting the stem as UTC is exact.
        chrono::NaiveDateTime::parse_from_str(stem, &format)
            .ok()
            .map(|naive| Utc.from_utc_datetime(&naive))
    }

    /// The part of the template that follows the leading literal and precedes
    /// the compression extension: a chrono format string that parses the stem
    /// left by [`Self::split_object_name`].
    ///
    /// `db-%Y%m%dT%H%M%SZ.tar.zst` with zstd gives `%Y%m%dT%H%M%SZ.tar`, which
    /// parses `20240301T010203Z.tar` out of `db-20240301T010203Z.tar.zst`.
    #[allow(dead_code)] // read by retention.rs in phase 2
    fn stem_format(&self) -> Option<String> {
        let end = self
            .filename
            .char_indices()
            .find(|(_, ch)| !(ch.is_ascii_alphanumeric() || *ch == '-'))
            .map_or(self.filename.len(), |(idx, _)| idx);
        let mut format = self.filename[end..].to_owned();

        // Drop the trailing `.ext` only when the template carries it, matching
        // what `object_name` did on the way out.
        if let Some(ext) = self.compression.extension() {
            let suffix = format!(".{ext}");
            if format.ends_with(&suffix) {
                format.truncate(format.len() - suffix.len());
            }
        }

        // A format with no conversion specifier would match anything, which
        // would let unrelated files be treated as backups.
        if !format.contains('%') {
            return None;
        }
        Some(format)
    }

    /// Whether an object name looks like a backup produced by this job.
    #[allow(dead_code)] // read by retention.rs in phase 2
    #[must_use]
    pub fn matches_object_name(&self, object: &str) -> bool {
        self.parse_object_name(object).is_some()
    }
}

/// Parse a standard five field cron expression.
///
/// Seconds are optional so `"0 3 * * *"` means 03:00:00, not a six field
/// pattern starting at second 0.
///
/// # Errors
///
/// Returns `croner`'s parse error when the expression is not a valid five or six
/// field pattern.
pub fn parse_cron(
    expression: &str,
) -> std::result::Result<croner::Cron, croner::errors::CronError> {
    use croner::parser::{CronParser, Seconds};
    CronParser::builder()
        .seconds(Seconds::Optional)
        .build()
        .parse(expression)
}

/// Parse an IANA timezone string like `America/New_York` or `UTC`.
///
/// Leading `:` (POSIX style) and whitespace are stripped.
///
/// # Errors
///
/// Returns [`Error::Config`] if `tz_str` cannot be parsed as an IANA timezone name.
pub fn parse_timezone(tz_str: &str) -> Result<chrono_tz::Tz> {
    let clean = tz_str.trim().strip_prefix(':').unwrap_or(tz_str.trim());
    if clean.is_empty() {
        Ok(chrono_tz::UTC)
    } else {
        clean
            .parse::<chrono_tz::Tz>()
            .map_err(|err| invalid(format!("invalid timezone `{tz_str}`: {err}")))
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Config(ConfigError::Invalid(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn job_yaml() -> String {
        r#"
cron = "0 3 * * *"
source = ["/backup/pgdata"]
filename = "pgdata-%Y%m%dT%H%M%SZ.tar.zst"
retention_days = 14
min_keep = 3

  [job.storage]
  type = "fs"
  root = "/tmp/backups"
"#
        .to_owned()
    }

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap_or_else(|e| panic!("write config: {e}"));
        path
    }

    /// Parse and validate `body`, returning either error as an [`Error`].
    fn parse(body: &str, dir: &Path) -> Result<Config> {
        let path = write_config(dir, body);
        let figment = Figment::new()
            .merge(NormalizedToml(&path))
            .merge(Env::prefixed(ENV_PREFIX).split("__").lowercase(true));
        let mut config: Config = figment.extract().map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn parses_a_minimal_job() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = format!("[[job]]\nname = \"db\"\n{}\n", job_yaml());
        let config = parse(&body, dir.path()).expect("valid config");

        assert_eq!(config.job_count(), 1);
        let job = config.job("db").expect("job exists");
        assert_eq!(job.compression, Compression::Zstd);
        assert_eq!(job.retention_days, 14);
        assert_eq!(job.min_keep, 3);
        assert!(!job.follow_symlinks);
        assert!(!job.wants_stop());
        assert_eq!(job.stop_timeout_secs, 30);
    }

    #[test]
    fn rejects_duplicate_job_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = format!(
            "[[job]]\nname = \"db\"\n{}\n[[job]]\nname = \"db\"\n{}\n",
            job_yaml(),
            job_yaml()
        );
        let err = parse(&body, dir.path()).unwrap_err();
        assert!(err.to_string().contains("duplicate job name `db`"), "{err}");
    }

    #[test]
    fn rejects_duplicate_source_basenames() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
source = ["/a/data", "/b/data"]
filename = "data-%Y%m%dT%H%M%SZ"
retention_days = 1
min_keep = 1
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let err = parse(body, dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("duplicates another source"),
            "{err}"
        );
    }

    #[test]
    fn rejects_a_filename_without_a_year() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
source = ["/data"]
filename = "db-%m%d.tar"
retention_days = 1
min_keep = 1
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let err = parse(body, dir.path()).unwrap_err();
        assert!(err.to_string().contains("%Y"), "{err}");
    }

    #[test]
    fn rejects_a_filename_without_seconds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
source = ["/data"]
filename = "db-%Y%m%dT%H%M.tar"
retention_days = 1
min_keep = 1
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let err = parse(body, dir.path()).unwrap_err();
        assert!(err.to_string().contains("%S"), "{err}");
    }

    #[test]
    fn rejects_zero_retention_and_min_keep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let zero_retention = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
retention_days = 0
min_keep = 1
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let err = parse(zero_retention, dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("retention_days must be >= 1"),
            "{err}"
        );

        let zero_min_keep = zero_retention
            .replace("retention_days = 0", "retention_days = 1")
            .replace("min_keep = 1", "min_keep = 0");
        let err = parse(&zero_min_keep, dir.path()).unwrap_err();
        assert!(err.to_string().contains("min_keep must be >= 1"), "{err}");
    }

    #[test]
    fn rejects_an_empty_source_list() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
source = []
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let err = parse(body, dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("source must not be empty"),
            "{err}"
        );
    }

    #[test]
    fn rejects_an_invalid_cron() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "not a cron"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let err = parse(body, dir.path()).unwrap_err();
        assert!(err.to_string().contains("invalid cron"), "{err}");
    }

    #[test]
    fn accepts_a_valid_cron() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let config = parse(body, dir.path()).expect("valid config");
        assert_eq!(
            config.job("db").expect("job").cron.as_deref(),
            Some("0 3 * * *")
        );
    }

    #[test]
    fn rejects_an_unknown_top_level_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Placed before `[[job]]`, otherwise TOML would nest it in the table.
        let body = format!("nonsense = true\n[[job]]\nname = \"db\"\n{}\n", job_yaml());
        let err = parse(&body, dir.path()).unwrap_err();
        assert!(err.to_string().contains("nonsense"), "{err}");
    }

    #[test]
    fn rejects_an_unknown_storage_type() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "carrier-pigeon"
"#;
        let err = parse(body, dir.path()).unwrap_err();
        assert!(err.to_string().contains("storage"), "{err}");
    }

    #[test]
    fn unknown_job_error_lists_configured_jobs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = format!("[[job]]\nname = \"db\"\n{}\n", job_yaml());
        let config = parse(&body, dir.path()).expect("valid config");
        let err = config.job("nope").unwrap_err();
        assert!(err.to_string().contains("configured jobs: db"), "{err}");
    }

    #[test]
    fn docker_socket_is_optional() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = format!("[[job]]\nname = \"db\"\n{}\n", job_yaml());
        let config = parse(&body, dir.path()).expect("valid config");
        assert!(config.docker.socket.is_none());

        let with_socket = format!(
            "[docker]\nsocket = \"/var/run/docker.sock\"\n[[job]]\nname = \"db\"\n{}\n",
            job_yaml()
        );
        let config = parse(&with_socket, dir.path()).expect("valid config");
        assert_eq!(
            config.docker.socket,
            Some(PathBuf::from("/var/run/docker.sock"))
        );
    }

    #[test]
    fn secrets_are_redacted_in_debug_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "dropbox"
  root = "/backups"
  client_id = "super-secret-id"
  client_secret = "super-secret-value"
  refresh_token = "super-secret-token"
"#;
        let config = parse(body, dir.path()).expect("valid config");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret-id"), "{rendered}");
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert!(!rendered.contains("super-secret-token"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    #[test]
    fn secret_string_exposes_on_demand() {
        let secret = SecretString::new("hunter2");
        assert_eq!(secret.expose(), "hunter2");
        assert!(!secret.is_empty());
        assert!(SecretString::default().is_empty());
    }

    #[test]
    fn object_name_is_rendered_in_utc() {
        let job = JobConfig {
            name: "db".to_owned(),
            cron: Some("0 3 * * *".to_owned()),
            crontext: None,
            timezone: None,
            schedule_source: Some(ScheduleSource::Cron),
            source: vec![PathBuf::from("/backup/pgdata")],
            filename: "pgdata-%Y%m%dT%H%M%SZ.tar".to_owned(),
            compression: Compression::Zstd,
            retention_days: 14,
            min_keep: 3,
            stop_containers: vec![],
            stop_label: None,
            stop_timeout_secs: 30,
            follow_symlinks: false,
            storage: StorageConfig::Fs(FsConfig {
                root: PathBuf::from("/tmp"),
                prefix: String::new(),
            }),
            pre: vec![],
            post: vec![],
            run_on_start: false,
            restore: None,
        };
        let now = Utc.with_ymd_and_hms(2024, 3, 5, 6, 7, 8).unwrap();
        assert_eq!(job.object_name(now), "pgdata-20240305T060708Z.tar.zst");
    }

    #[test]
    fn compression_extension_matches_the_algorithm() {
        assert_eq!(Compression::Zstd.extension(), Some("zst"));
        assert_eq!(Compression::Gzip.extension(), Some("gz"));
        assert_eq!(Compression::None.extension(), None);
        assert!(Compression::zstd_level() > 0);
        assert!(Compression::gzip_level() > 0);
    }

    #[test]
    fn remote_path_applies_the_prefix() {
        let storage = StorageConfig::Fs(FsConfig {
            root: PathBuf::from("/tmp"),
            prefix: "dvb/db".to_owned(),
        });
        assert_eq!(storage.remote_path("db-1.tar.zst"), "dvb/db/db-1.tar.zst");
        assert_eq!(storage.kind(), "fs");

        let no_prefix = StorageConfig::Fs(FsConfig {
            root: PathBuf::from("/tmp"),
            prefix: String::new(),
        });
        assert_eq!(no_prefix.remote_path("db-1.tar.zst"), "db-1.tar.zst");
    }

    #[test]
    fn stop_configuration_is_detected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[docker]
socket = "/var/run/docker.sock"

[[job]]
name = "db"
cron = "0 3 * * *"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
stop_containers = ["postgres"]
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let config = parse(body, dir.path()).expect("valid config");
        assert!(config.job("db").expect("job").wants_stop());
    }

    #[test]
    fn a_job_that_needs_docker_requires_a_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
stop_containers = ["postgres"]
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let err = parse(body, dir.path()).expect_err("accepted a job with no socket");
        assert!(err.to_string().contains("[docker]"), "{err}");

        // The same job is fine once the socket is configured.
        assert!(parse(&with_socket(body), dir.path()).is_ok());
    }

    /// The same TOML with a `[docker]` section, for a job that does need the
    /// daemon to be reachable.
    fn with_socket(body: &str) -> String {
        format!("[docker]\nsocket = \"/var/run/docker.sock\"\n\n{body}")
    }

    #[test]
    fn hooks_round_trip_with_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[docker]
socket = "/var/run/docker.sock"

[[job]]
name = "db"
cron = "0 3 * * *"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"

  [[job.pre]]
  cmd = ["pg_dump", "-f", "/backup/pgdata/dump.sql"]
  container = "postgres"
  timeout_secs = 300

  [[job.post]]
  cmd = ["/bin/notify.sh"]
  run_on = "always"
"#;
        let config = parse(body, dir.path()).expect("valid config");
        let job = config.job("db").expect("job");
        assert_eq!(job.pre.len(), 1);
        assert_eq!(job.pre[0].container.as_deref(), Some("postgres"));
        assert_eq!(job.pre[0].timeout_secs, 300);
        assert_eq!(job.post.len(), 1);
        assert_eq!(job.post[0].run_on, RunOn::Always);
        assert_eq!(job.post[0].timeout_secs, 300);
    }

    #[test]
    fn rejects_an_empty_hook_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"

  [[job.post]]
  cmd = []
"#;
        let err = parse(body, dir.path()).unwrap_err();
        assert!(err.to_string().contains("empty cmd"), "{err}");
    }

    #[test]
    fn compression_levels_are_sane() {
        // Ranges as documented by zstd (1..=19) and gzip (1..=9).
        assert!((1..=19).contains(&Compression::zstd_level()));
        assert!((1..=9).contains(&Compression::gzip_level()));
    }

    #[test]
    fn parses_restore_configuration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"

  [job.restore]
  dir = "/restore"
  script = "/scripts/pg_restore.sh"
  script_timeout_secs = 1800
"#;
        let config = parse(body, dir.path()).unwrap();
        let restore = config.jobs[0].restore.as_ref().expect("restore configured");
        assert_eq!(restore.dir.as_deref(), Some(Path::new("/restore")));
        assert_eq!(
            restore.script.as_deref(),
            Some(Path::new("/scripts/pg_restore.sh"))
        );
        assert_eq!(restore.script_timeout_secs, 1800);
    }

    #[test]
    fn rejects_unknown_fields_in_restore() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"

  [job.restore]
  dir = "/restore"
  unknown = true
"#;
        let err = parse(body, dir.path()).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
        assert!(err.to_string().contains("`unknown`"), "{err}");
    }

    #[test]
    fn rejects_both_cron_and_crontext() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
crontext = "every day at 03:00"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let err = parse(body, dir.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot specify both `cron` and `crontext`"),
            "{err}"
        );
    }

    #[test]
    fn rejects_neither_cron_nor_crontext() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let err = parse(body, dir.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("must specify either `cron` or `crontext`"),
            "{err}"
        );
    }

    #[test]
    fn accepts_crontext_and_resolves_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
crontext = "every friday at 18:00"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let config = parse(body, dir.path()).expect("valid config");
        let job = config.job("db").expect("job");
        assert_eq!(job.cron.as_deref(), Some("0 18 * * 5"));
        assert_eq!(
            job.schedule_source,
            Some(ScheduleSource::Crontext("every friday at 18:00".to_owned()))
        );
    }

    #[test]
    fn rejects_invalid_crontext() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
crontext = "every 5 hours"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let err = parse(body, dir.path()).unwrap_err();
        assert!(err.to_string().contains("does not divide 24"), "{err}");
    }

    #[test]
    fn rejects_invalid_timezone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
timezone = "Mars/Olympus_Mons"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let err = parse(body, dir.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid timezone `Mars/Olympus_Mons`"),
            "{err}"
        );
    }

    #[test]
    fn accepts_valid_job_timezone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = r#"
[[job]]
name = "db"
cron = "0 3 * * *"
timezone = "America/New_York"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#;
        let config = parse(body, dir.path()).expect("valid config");
        let job = config.job("db").expect("job");
        assert_eq!(
            job.effective_timezone(chrono_tz::UTC),
            chrono_tz::America::New_York
        );
    }

    #[test]
    fn env_override_crontext_works() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            r#"
[[job]]
name = "db"
source = ["/data"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "/tmp/x"
"#,
        );

        let figment = Figment::new()
            .merge(NormalizedToml(&path))
            .merge(("job.0.crontext", "every 15 minutes"));
        let mut config: Config = figment.extract().expect("extract");
        config.validate().expect("validate");
        let job = config.job("db").expect("job");
        assert_eq!(job.cron.as_deref(), Some("*/15 * * * *"));
        assert_eq!(
            job.schedule_source,
            Some(ScheduleSource::Crontext("every 15 minutes".to_owned()))
        );
    }

    #[test]
    fn validate_static_accepts_nonexistent_source_and_runtime_rejects_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let non_existent = dir.path().join("definitely-does-not-exist-dir");
        let body = format!(
            r#"
[[job]]
name = "db"
cron = "0 3 * * *"
source = ["{}"]
filename = "pgdata-%Y%m%dT%H%M%SZ.tar.zst"
retention_days = 14
min_keep = 3

  [job.storage]
  type = "fs"
  root = "{}"
"#,
            non_existent.display(),
            dir.path().display()
        );

        let path = write_config(dir.path(), &body);
        let config = Config::load(&path, ValidationMode::Static)
            .expect("validate_static must accept non-existent source dirs");

        let runtime_err = config
            .validate_runtime()
            .expect_err("validate_runtime must reject non-existent source dirs");
        assert!(
            runtime_err.to_string().contains("does not exist"),
            "{runtime_err}"
        );

        // When source dir is created, validate_runtime succeeds
        std::fs::create_dir_all(&non_existent).expect("create source");
        config
            .validate_runtime()
            .expect("validate_runtime must pass once source exists");
    }

    #[test]
    fn job_name_rules() {
        // Direct helper checks
        assert!(is_valid_job_name("db"));
        assert!(is_valid_job_name("pg-main_1"));
        assert!(is_valid_job_name("a"));
        assert!(is_valid_job_name("1"));
        assert!(is_valid_job_name(&"a".repeat(63)));

        assert!(!is_valid_job_name(""));
        assert!(!is_valid_job_name("../x"));
        assert!(!is_valid_job_name("A"));
        assert!(!is_valid_job_name("-x"));
        assert!(!is_valid_job_name("_x"));
        assert!(!is_valid_job_name(&"a".repeat(64)));
        assert!(!is_valid_job_name("job/name"));
        assert!(!is_valid_job_name("job name"));
        assert!(!is_valid_job_name("job\"name"));
        assert!(!is_valid_job_name("job'name"));

        // Config parsing checks
        let dir = tempfile::tempdir().expect("tempdir");
        let valid_body = format!("[[job]]\nname = \"pg-main_1\"\n{}\n", job_yaml());
        assert!(parse(&valid_body, dir.path()).is_ok());

        for bad in [
            "../x",
            "A",
            "-x",
            "",
            &"a".repeat(64),
            "job/name",
            "job name",
        ] {
            let bad_body = format!("[[job]]\nname = \"{bad}\"\n{}\n", job_yaml());
            let err =
                parse(&bad_body, dir.path()).expect_err(&format!("accepted bad name `{bad}`"));
            assert!(
                err.to_string().contains("job name")
                    || err.to_string().contains("invalid job name"),
                "unexpected error message: {err}"
            );
        }
    }
}
