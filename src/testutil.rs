//! Shared helpers for unit tests.
//!
//! Only compiled under `cfg(test)`. Integration tests in `tests/` drive the real
//! binary and build their own fixtures instead.

use std::path::PathBuf;

use crate::config::{Compression, FsConfig, JobConfig, StorageConfig};

/// Builder for a [`JobConfig`] with sensible defaults, so tests only state the
/// fields they actually care about.
pub struct JobBuilder {
    job: JobConfig,
}

// Each setter is used by whichever phase owns the corresponding feature; keeping
// them all means later tests do not have to re-extend the builder.
#[allow(dead_code)]
impl JobBuilder {
    /// A job archiving `/data` to a `fs` root at `/tmp/dvb-test`.
    pub fn new(name: &str) -> Self {
        Self {
            job: JobConfig {
                name: name.to_owned(),
                cron: Some("0 3 * * *".to_owned()),
                crontext: None,
                timezone: None,
                schedule_source: Some(crate::config::ScheduleSource::Cron),
                source: vec![PathBuf::from("/data")],
                filename: "db-%Y%m%dT%H%M%SZ.tar.zst".to_owned(),
                compression: Compression::Zstd,
                retention_days: 14,
                min_keep: 3,
                stop_containers: Vec::new(),
                stop_label: None,
                stop_timeout_secs: 30,
                follow_symlinks: false,
                storage: StorageConfig::Fs(FsConfig {
                    root: PathBuf::from("/tmp/dvb-test"),
                    prefix: "db".to_owned(),
                }),
                pre_backup: None,
                post_backup: None,
                run_on_start: false,
                pre_restore: None,
                post_restore: None,
            },
        }
    }

    pub fn source(mut self, source: impl Into<PathBuf>) -> Self {
        self.job.source = vec![source.into()];
        self
    }

    pub fn filename(mut self, filename: &str) -> Self {
        self.job.filename = filename.to_owned();
        self
    }

    pub fn compression(mut self, compression: Compression) -> Self {
        self.job.compression = compression;
        self
    }

    pub fn retention(mut self, days: u32) -> Self {
        self.job.retention_days = days;
        self
    }

    pub fn min_keep(mut self, count: u32) -> Self {
        self.job.min_keep = count;
        self
    }

    pub fn storage(mut self, storage: StorageConfig) -> Self {
        self.job.storage = storage;
        self
    }

    pub fn follow_symlinks(mut self, follow: bool) -> Self {
        self.job.follow_symlinks = follow;
        self
    }

    pub fn stop_containers(mut self, names: &[&str]) -> Self {
        self.job.stop_containers = names.iter().map(|n| (*n).to_owned()).collect();
        self
    }

    pub fn stop_label(mut self, label: &str) -> Self {
        self.job.stop_label = Some(label.to_owned());
        self
    }

    pub fn build(self) -> JobConfig {
        self.job
    }
}

impl From<JobBuilder> for JobConfig {
    fn from(builder: JobBuilder) -> Self {
        builder.build()
    }
}
