//! Restore core: download, decompress, validate, and safely extract a backup.
//!
//! Restores stream directly from storage through decompression to `tar::Archive`
//! without intermediate archive buffering on disk or in memory.

use std::fs;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use futures_util::TryStreamExt as _;
use opendal::Operator;

use crate::config::{Compression, JobConfig};
use crate::error::{Error, Result};
use crate::lock::JobLock;
use crate::retention::{self, Backup};

/// CLI options driving a restore operation.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct RestoreOptions {
    /// Exact backup object name to restore.
    pub name: Option<String>,
    /// Target cutoff time (RFC3339 or YYYY-MM-DD).
    pub at: Option<String>,
    /// Explicit target extraction directory.
    pub to: Option<PathBuf>,
    /// Optional restore script to execute (Phase 7).
    pub script: Option<PathBuf>,
    /// Timeout in seconds for the restore script.
    pub script_timeout: Option<u64>,
    /// Allow extracting into an existing non-empty directory.
    pub force: bool,
    /// Delete extracted directory after the script succeeds (Phase 7).
    pub cleanup: bool,
    /// Stop containers while the restore script runs (Phase 7).
    pub stop_containers: bool,
    /// Skip checksum verification (Phase 9).
    pub no_verify: bool,
    /// Resolve the backup and print the plan without downloading.
    pub dry_run: bool,
    /// Restore file ownership when running as root.
    pub preserve_owner: bool,
    /// Maximum allowed extracted bytes (decompression bomb guard).
    pub max_extracted_bytes: Option<u64>,
    /// Extra arguments passed to the script after `--`.
    pub extra_args: Vec<String>,
}

/// Select which backup to restore based on `--name` or `--at` rules.
///
/// * `--name`: exact match on the object filename. Must not contain path separators.
/// * `--at`: newest backup at or before the given timestamp (RFC3339 or `YYYY-MM-DD` UTC end-of-day).
/// * Default (neither flag): newest backup.
///
/// # Errors
///
/// Returns [`Error::Restore`] when no backups exist, when the name is invalid or not found,
/// or when no backup is at or before the requested cutoff time.
pub fn select_backup<'a>(
    backups: &'a [Backup],
    job: &JobConfig,
    name_filter: Option<&str>,
    at_filter: Option<&str>,
) -> Result<&'a Backup> {
    if backups.is_empty() {
        return Err(Error::Restore(format!(
            "no backups found for job `{}` under prefix `{}`",
            job.name,
            job.storage.prefix()
        )));
    }

    if let Some(name) = name_filter {
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(Error::Restore(format!(
                "invalid backup name `{name}`: must not contain '/', '\\', or '..'"
            )));
        }

        if job.parse_object_name(name).is_none() {
            return Err(Error::Restore(format!(
                "backup name `{name}` does not match filename pattern for job `{}`",
                job.name
            )));
        }

        let found = backups.iter().find(|b| {
            let filename = b.path.rsplit('/').next().unwrap_or(&b.path);
            filename == name
        });

        return if let Some(backup) = found {
            Ok(backup)
        } else {
            let oldest = &backups[backups.len() - 1];
            let newest = &backups[0];
            Err(Error::Restore(format!(
                "backup `{name}` not found for job `{}` (available: oldest {}, newest {})",
                job.name,
                oldest.timestamp.to_rfc3339(),
                newest.timestamp.to_rfc3339()
            )))
        };
    }

    if let Some(at_str) = at_filter {
        let cutoff = parse_at_timestamp(at_str)?;
        let found = backups.iter().find(|b| b.timestamp <= cutoff);
        return if let Some(backup) = found {
            Ok(backup)
        } else {
            let oldest = &backups[backups.len() - 1];
            let newest = &backups[0];
            Err(Error::Restore(format!(
                "no backup found at or before `{at_str}` for job `{}` (available: oldest {}, newest {})",
                job.name,
                oldest.timestamp.to_rfc3339(),
                newest.timestamp.to_rfc3339()
            )))
        };
    }

    // Default: newest backup (backups is sorted newest first).
    Ok(&backups[0])
}

/// Parse an `--at` argument as RFC3339 or `YYYY-MM-DD` (end of that day in UTC).
fn parse_at_timestamp(s: &str) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        && let Some(dt) = date.and_hms_nano_opt(23, 59, 59, 999_999_999)
    {
        return Ok(dt.and_utc());
    }

    Err(Error::Restore(format!(
        "invalid `--at` timestamp `{s}`: expected RFC3339 (e.g. 2026-10-01T12:00:00Z) or YYYY-MM-DD (e.g. 2026-10-01)"
    )))
}

/// Resolve the extraction target directory according to config and CLI flags.
///
/// Rules:
/// 1. If `--to` is explicitly passed: extract directly to `<dir>`.
/// 2. If `--to` is omitted and no script is specified: default to `job.restore.dir`.
///    If `job.restore.dir` is not configured, fail before downloading.
/// 3. If `--to` is omitted and a script is specified: default to
///    `<job.restore.dir or $DVB_TMP_DIR or temp_dir()>/restore-<job>-<timestamp>`.
///
/// # Errors
///
/// Returns [`Error::Restore`] when no directory is passed and none is configured.
pub fn resolve_target_dir(
    job: &JobConfig,
    to: Option<&Path>,
    script: Option<&Path>,
    archive_timestamp: DateTime<Utc>,
) -> Result<PathBuf> {
    if let Some(dest) = to {
        return Ok(dest.to_path_buf());
    }

    let effective_script =
        script.or_else(|| job.restore.as_ref().and_then(|r| r.script.as_deref()));

    if effective_script.is_none() {
        if let Some(dir) = job.restore.as_ref().and_then(|r| r.dir.as_deref()) {
            return Ok(dir.to_path_buf());
        }
        return Err(Error::Restore(format!(
            "no target directory specified for job `{}`: pass `--to <dir>` or configure `[job.restore].dir` in the config file",
            job.name
        )));
    }

    let base_dir = if let Some(dir) = job.restore.as_ref().and_then(|r| r.dir.as_deref()) {
        dir.to_path_buf()
    } else if let Some(env_tmp) = std::env::var_os("DVB_TMP_DIR") {
        PathBuf::from(env_tmp)
    } else {
        std::env::temp_dir()
    };

    let dir_name = format!(
        "restore-{}-{}",
        job.name,
        archive_timestamp.format("%Y%m%dT%H%M%SZ")
    );
    Ok(base_dir.join(dir_name))
}

/// Validate that the target directory satisfies non-empty constraints before downloading.
///
/// # Errors
///
/// Returns [`Error::Restore`] if target is non-empty without `--force`, or [`Error::Io`] on read failure.
pub fn validate_target_dir(target_dir: &Path, force: bool) -> Result<()> {
    if target_dir.exists() {
        if !target_dir.is_dir() {
            return Err(Error::Restore(format!(
                "target path `{}` exists and is not a directory",
                target_dir.display()
            )));
        }
        let mut entries = fs::read_dir(target_dir).map_err(|source| Error::Io {
            path: target_dir.to_path_buf(),
            source,
        })?;
        if entries.next().is_some() && !force {
            return Err(Error::Restore(format!(
                "target directory `{}` exists and is not empty; pass `--force` to extract into it",
                target_dir.display()
            )));
        }
    }
    Ok(())
}

/// Detect compression algorithm from the first bytes of the archive.
#[must_use]
pub fn detect_compression(magic: &[u8]) -> Compression {
    if magic.len() >= 4 && magic[..4] == [0x28, 0xB5, 0x2F, 0xFD] {
        Compression::Zstd
    } else if magic.len() >= 2 && magic[..2] == [0x1F, 0x8B] {
        Compression::Gzip
    } else {
        Compression::None
    }
}

/// Check if an entry type in a tar archive is safe to unpack.
fn is_allowed_entry_type(entry_type: tar::EntryType) -> bool {
    matches!(
        entry_type,
        tar::EntryType::Regular
            | tar::EntryType::Continuous
            | tar::EntryType::Directory
            | tar::EntryType::Symlink
            | tar::EntryType::Link
            | tar::EntryType::GNULongName
            | tar::EntryType::GNULongLink
            | tar::EntryType::XGlobalHeader
            | tar::EntryType::XHeader
    )
}

/// Warn when target filesystem has less than 3x free space compared to the compressed archive.
fn check_disk_space(target_dir: &Path, object_size: u64) {
    let check_path = if target_dir.exists() {
        target_dir.to_path_buf()
    } else {
        let mut cur = target_dir;
        while let Some(parent) = cur.parent() {
            if parent.exists() {
                cur = parent;
                break;
            }
            cur = parent;
        }
        cur.to_path_buf()
    };

    if let Ok(vfs) = rustix::fs::statvfs(&check_path) {
        let free_bytes = vfs.f_bavail.saturating_mul(vfs.f_frsize);
        let needed_bytes = object_size.saturating_mul(3);
        if free_bytes < needed_bytes {
            tracing::warn!(
                free_bytes,
                needed_bytes,
                target = %target_dir.display(),
                "target filesystem has less than 3x free space ({free_bytes} bytes available) compared to compressed backup size ({object_size} bytes)"
            );
        }
    }
}

/// Wrapping reader that enforces a maximum byte limit on decompression.
struct BoundedReader<R> {
    inner: R,
    bytes_read: u64,
    limit: Option<u64>,
}

impl<R: std::io::Read> std::io::Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes_read = self.bytes_read.saturating_add(n as u64);
        if let Some(limit) = self.limit
            && self.bytes_read > limit
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("decompressed archive exceeded maximum size limit of {limit} bytes"),
            ));
        }
        Ok(n)
    }
}

/// Validate path and link safety of an archive entry before unpacking.
fn validate_archive_entry<R: std::io::Read>(entry: &tar::Entry<'_, R>) -> Result<PathBuf> {
    let entry_type = entry.header().entry_type();
    if !is_allowed_entry_type(entry_type) {
        return Err(Error::Restore(format!(
            "archive contains disallowed entry type {:?} for `{}`: only regular files, directories, symlinks, and hardlinks are permitted",
            entry_type,
            entry
                .path()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        )));
    }

    let path = entry.path().map_err(Error::archive_other)?.to_path_buf();
    if path.is_absolute() {
        return Err(Error::Restore(format!(
            "archive entry has absolute path `{}`",
            path.display()
        )));
    }
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                return Err(Error::Restore(format!(
                    "archive entry `{}` contains `..` path component",
                    path.display()
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::Restore(format!(
                    "archive entry `{}` has absolute or prefix component",
                    path.display()
                )));
            }
            _ => {}
        }
    }

    if entry_type.is_hard_link() {
        let link_name = entry.link_name().map_err(Error::archive_other)?;
        match link_name {
            Some(target) => {
                if target.is_absolute() {
                    return Err(Error::Restore(format!(
                        "hardlink `{}` points to absolute path `{}`",
                        path.display(),
                        target.display()
                    )));
                }
                for comp in target.components() {
                    if let Component::ParentDir = comp {
                        return Err(Error::Restore(format!(
                            "hardlink `{}` contains `..` in target `{}`",
                            path.display(),
                            target.display()
                        )));
                    }
                }
            }
            None => {
                return Err(Error::Restore(format!(
                    "hardlink `{}` has no link target",
                    path.display()
                )));
            }
        }
    } else if entry_type.is_symlink() {
        let link_name = entry.link_name().map_err(Error::archive_other)?;
        match link_name {
            Some(target) => {
                if target.as_os_str().is_empty() {
                    return Err(Error::Restore(format!(
                        "symlink `{}` has empty target",
                        path.display()
                    )));
                }
            }
            None => {
                return Err(Error::Restore(format!(
                    "symlink `{}` has no link target",
                    path.display()
                )));
            }
        }
    }

    Ok(path)
}

/// Safely unpack entries from `reader` into `extract_dir`.
///
/// # Errors
///
/// Returns [`Error::Restore`] or [`Error::Archive`] if path validation or unpacking fails.
pub fn extract_archive<R: std::io::Read>(
    reader: R,
    extract_dir: &Path,
    force: bool,
    preserve_owner: bool,
    max_extracted_bytes: Option<u64>,
) -> Result<()> {
    let bounded = BoundedReader {
        inner: reader,
        bytes_read: 0,
        limit: max_extracted_bytes,
    };

    let mut archive = tar::Archive::new(bounded);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);

    let is_root = rustix::process::getuid().is_root();
    archive.set_preserve_ownerships(preserve_owner && is_root);
    archive.set_overwrite(force);

    let entries = archive.entries().map_err(Error::archive_other)?;
    for entry_result in entries {
        let mut entry = entry_result.map_err(Error::archive_other)?;
        let path = validate_archive_entry(&entry)?;

        let unpacked = entry.unpack_in(extract_dir).map_err(Error::archive_other)?;
        if !unpacked {
            return Err(Error::Restore(format!(
                "archive entry `{}` failed path safety check when unpacking in `{}`",
                path.display(),
                extract_dir.display()
            )));
        }
    }

    Ok(())
}

/// Prepare target and optional staging directory.
fn prepare_extract_dirs(target_dir: &Path, force: bool) -> Result<(PathBuf, Option<PathBuf>)> {
    if force {
        if let Some(parent) = target_dir.parent() {
            fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::create_dir_all(target_dir).map_err(|source| Error::Io {
            path: target_dir.to_path_buf(),
            source,
        })?;
        Ok((target_dir.to_path_buf(), None))
    } else {
        let staging = PathBuf::from(format!("{}.dvb-partial", target_dir.display()));
        if staging.exists() {
            let _ = fs::remove_dir_all(&staging);
        }
        if let Some(parent) = staging.parent() {
            fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::create_dir_all(&staging).map_err(|source| Error::Io {
            path: staging.clone(),
            source,
        })?;
        Ok((staging.clone(), Some(staging)))
    }
}

/// Execute the core restore operation: lock, select, download, decompress, and extract.
///
/// # Errors
///
/// Returns [`Error::Restore`] or [`Error::Storage`] if downloading or extraction fails.
pub async fn run_restore(
    op: &Operator,
    job: &JobConfig,
    options: &RestoreOptions,
) -> Result<PathBuf> {
    // 1. Acquire job lock so restore never overlaps backup or prune for the same job.
    let _lock = JobLock::try_acquire(&job.name)?;

    // 2. List backups and select the target backup.
    let (backups, _) = retention::list_backups(op, job).await?;
    let backup = select_backup(
        &backups,
        job,
        options.name.as_deref(),
        options.at.as_deref(),
    )?;

    // 3. Resolve target directory and make it absolute.
    let target_dir = resolve_target_dir(
        job,
        options.to.as_deref(),
        options.script.as_deref(),
        backup.timestamp,
    )?;
    let abs_target_dir = if target_dir.is_absolute() {
        target_dir
    } else {
        let cwd = std::env::current_dir().map_err(|source| Error::Io {
            path: PathBuf::from("."),
            source,
        })?;
        cwd.join(target_dir)
    };

    // 4. Dry-run: print resolved details and exit early without downloading.
    if options.dry_run {
        let script_display = options
            .script
            .as_deref()
            .or_else(|| job.restore.as_ref().and_then(|r| r.script.as_deref()))
            .map_or_else(|| "none".to_owned(), |p| p.display().to_string());
        println!("job: {}", job.name);
        println!("backup: {}", backup.path);
        println!("size: {} bytes", backup.size);
        println!("timestamp: {}", backup.timestamp.to_rfc3339());
        println!("target dir: {}", abs_target_dir.display());
        println!("script: {script_display}");
        return Ok(abs_target_dir);
    }

    // 5. Early validation before any storage download.
    validate_target_dir(&abs_target_dir, options.force)?;
    check_disk_space(&abs_target_dir, backup.size);

    // 6. Setup extraction / staging directory.
    let (extract_dir, staging_dir) = prepare_extract_dirs(&abs_target_dir, options.force)?;

    // 7. Download and extract from storage.
    let extract_result = download_and_extract(op, job, &backup.path, &extract_dir, options).await;

    // 8. Promote staging or cleanup on failure.
    promote_or_cleanup_staging(extract_result, &abs_target_dir, staging_dir.as_deref())
}

async fn download_and_extract(
    op: &Operator,
    job: &JobConfig,
    backup_path: &str,
    extract_path: &Path,
    options: &RestoreOptions,
) -> Result<()> {
    let reader = op
        .reader(backup_path)
        .await
        .map_err(|source| Error::Storage {
            backend: job.storage.kind(),
            source,
        })?;

    let byte_stream = reader
        .into_bytes_stream(..)
        .await
        .map_err(|source| Error::Storage {
            backend: job.storage.kind(),
            source,
        })?
        .map_err(|err| std::io::Error::other(err.to_string()));

    let async_reader = tokio_util::io::StreamReader::new(byte_stream);

    let force = options.force;
    let preserve_owner = options.preserve_owner;
    let max_extracted_bytes = options.max_extracted_bytes;
    let configured_compression = job.compression;
    let extract_dir = extract_path.to_path_buf();

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut sync_reader = tokio_util::io::SyncIoBridge::new(async_reader);

        let mut magic_buf = [0u8; 4];
        let mut magic_read = 0;
        while magic_read < 4 {
            match sync_reader.read(&mut magic_buf[magic_read..]) {
                Ok(0) => break,
                Ok(n) => magic_read += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::archive_other(e)),
            }
        }

        let detected = detect_compression(&magic_buf[..magic_read]);
        if detected != configured_compression {
            tracing::warn!(
                detected = ?detected,
                configured = ?configured_compression,
                "detected archive compression differs from job configuration; trusting magic bytes"
            );
        }

        let chained = std::io::Cursor::new(magic_buf[..magic_read].to_vec()).chain(sync_reader);
        let decompressed: Box<dyn std::io::Read + Send> = match detected {
            Compression::Zstd => {
                Box::new(zstd::stream::read::Decoder::new(chained).map_err(Error::archive_other)?)
            }
            Compression::Gzip => Box::new(flate2::read::GzDecoder::new(chained)),
            Compression::None => Box::new(chained),
        };

        extract_archive(
            decompressed,
            &extract_dir,
            force,
            preserve_owner,
            max_extracted_bytes,
        )
    })
    .await
    .map_err(|join_err| Error::archive_other(std::io::Error::other(join_err.to_string())))?
}

fn promote_or_cleanup_staging(
    extract_result: Result<()>,
    abs_target_dir: &Path,
    staging_dir: Option<&Path>,
) -> Result<PathBuf> {
    match extract_result {
        Ok(()) => {
            if let Some(staging) = staging_dir {
                if abs_target_dir.exists() {
                    let _ = fs::remove_dir(abs_target_dir);
                }
                fs::rename(staging, abs_target_dir).map_err(|source| Error::Io {
                    path: abs_target_dir.to_path_buf(),
                    source,
                })?;
            }
            Ok(abs_target_dir.to_path_buf())
        }
        Err(err) => {
            if let Some(staging) = staging_dir {
                let _ = fs::remove_dir_all(staging);
            } else {
                tracing::warn!(
                    target = %abs_target_dir.display(),
                    "restore failed with --force; partial files left in target directory"
                );
            }
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn test_job() -> JobConfig {
        use crate::testutil::JobBuilder;
        JobBuilder::new("db")
            .filename("db-%Y%m%dT%H%M%SZ.tar.zst")
            .build()
    }

    #[test]
    fn select_backup_default_picks_newest() {
        let job = test_job();
        let b1 = Backup {
            path: "db/db-20261001T100000Z.tar.zst".to_owned(),
            size: 100,
            timestamp: Utc.with_ymd_and_hms(2026, 10, 1, 10, 0, 0).unwrap(),
        };
        let b2 = Backup {
            path: "db/db-20261002T100000Z.tar.zst".to_owned(),
            size: 100,
            timestamp: Utc.with_ymd_and_hms(2026, 10, 2, 10, 0, 0).unwrap(),
        };
        let backups = vec![b2.clone(), b1.clone()]; // newest first

        let selected = select_backup(&backups, &job, None, None).expect("selected");
        assert_eq!(selected, &b2);
    }

    #[test]
    fn select_backup_at_filter_boundaries() {
        let job = test_job();
        let b1 = Backup {
            path: "db/db-20261001T100000Z.tar.zst".to_owned(),
            size: 100,
            timestamp: Utc.with_ymd_and_hms(2026, 10, 1, 10, 0, 0).unwrap(),
        };
        let b2 = Backup {
            path: "db/db-20261002T100000Z.tar.zst".to_owned(),
            size: 100,
            timestamp: Utc.with_ymd_and_hms(2026, 10, 2, 10, 0, 0).unwrap(),
        };
        let backups = vec![b2.clone(), b1.clone()];

        // Exactly equal to b2
        let s = select_backup(&backups, &job, None, Some("2026-10-02T10:00:00Z")).unwrap();
        assert_eq!(s, &b2);

        // One second before b2 -> selects b1
        let s = select_backup(&backups, &job, None, Some("2026-10-02T09:59:59Z")).unwrap();
        assert_eq!(s, &b1);

        // Date YYYY-MM-DD covers whole day in UTC
        let s = select_backup(&backups, &job, None, Some("2026-10-01")).unwrap();
        assert_eq!(s, &b1);

        // Before all backups -> error with available range
        let err = select_backup(&backups, &job, None, Some("2026-09-30T23:59:59Z")).unwrap_err();
        assert!(
            err.to_string().contains("available: oldest 2026-10-01"),
            "{err}"
        );
    }

    #[test]
    fn select_backup_name_filter_validation() {
        let job = test_job();
        let b1 = Backup {
            path: "db/db-20261001T100000Z.tar.zst".to_owned(),
            size: 100,
            timestamp: Utc.with_ymd_and_hms(2026, 10, 1, 10, 0, 0).unwrap(),
        };
        let backups = vec![b1.clone()];

        // Valid match
        let s = select_backup(&backups, &job, Some("db-20261001T100000Z.tar.zst"), None).unwrap();
        assert_eq!(s, &b1);

        // Path traversal / slashes rejected
        let err = select_backup(&backups, &job, Some("../db-20261001T100000Z.tar.zst"), None)
            .unwrap_err();
        assert!(err.to_string().contains("must not contain"), "{err}");

        let err = select_backup(&backups, &job, Some("db/db-20261001T100000Z.tar.zst"), None)
            .unwrap_err();
        assert!(err.to_string().contains("must not contain"), "{err}");

        // Pattern mismatch
        let err = select_backup(&backups, &job, Some("unrelated.tar"), None).unwrap_err();
        assert!(
            err.to_string().contains("does not match filename pattern"),
            "{err}"
        );
    }

    #[test]
    fn compression_detection_detects_zstd_gzip_and_plain() {
        assert_eq!(
            detect_compression(&[0x28, 0xB5, 0x2F, 0xFD]),
            Compression::Zstd
        );
        assert_eq!(
            detect_compression(&[0x1F, 0x8B, 0x08, 0x00]),
            Compression::Gzip
        );
        assert_eq!(detect_compression(b"ustar"), Compression::None);
    }

    #[test]
    fn resolve_target_dir_rules() {
        let mut job = test_job();
        let time = Utc.with_ymd_and_hms(2026, 10, 1, 12, 0, 0).unwrap();

        // 1. Explicit --to takes precedence
        let to = Path::new("/custom/path");
        let res = resolve_target_dir(&job, Some(to), None, time).unwrap();
        assert_eq!(res, PathBuf::from("/custom/path"));

        // 2. No script, no job.restore.dir -> error
        let err = resolve_target_dir(&job, None, None, time).unwrap_err();
        assert!(
            err.to_string().contains("no target directory specified"),
            "{err}"
        );

        // 3. No script, job.restore.dir configured -> default to job.restore.dir
        job.restore = Some(crate::config::RestoreConfig {
            dir: Some(PathBuf::from("/configured/restore")),
            script: None,
            script_timeout_secs: 3600,
        });
        let res = resolve_target_dir(&job, None, None, time).unwrap();
        assert_eq!(res, PathBuf::from("/configured/restore"));

        // 4. Script specified -> default to <base>/restore-<job>-<timestamp>
        let script = Path::new("/scripts/restore.sh");
        let res = resolve_target_dir(&job, None, Some(script), time).unwrap();
        assert_eq!(
            res,
            PathBuf::from("/configured/restore/restore-db-20261001T120000Z")
        );
    }

    #[test]
    fn validate_target_dir_enforces_force_for_non_empty() {
        let dir = tempfile::tempdir().unwrap();
        // Empty dir -> ok without force
        assert!(validate_target_dir(dir.path(), false).is_ok());

        // File inside -> non-empty -> fails without force
        fs::write(dir.path().join("file.txt"), b"test").unwrap();
        let err = validate_target_dir(dir.path(), false).unwrap_err();
        assert!(err.to_string().contains("exists and is not empty"), "{err}");

        // Passes with force
        assert!(validate_target_dir(dir.path(), true).is_ok());
    }

    fn craft_raw_tar(path_bytes: &[u8], data: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.as_mut_bytes()[..path_bytes.len()].copy_from_slice(path_bytes);
        header.set_cksum();

        let mut tar_bytes = Vec::new();
        tar_bytes.extend_from_slice(header.as_bytes());
        tar_bytes.extend_from_slice(data);
        let padding = (512 - (data.len() % 512)) % 512;
        tar_bytes.extend(std::iter::repeat_n(0, padding));
        tar_bytes.extend(std::iter::repeat_n(0, 1024));
        tar_bytes
    }

    #[test]
    fn malicious_archive_parent_dir_fails() {
        let tar_bytes = craft_raw_tar(b"../evil.txt", b"evil content");

        let extract_dir = tempfile::tempdir().unwrap();
        let err = extract_archive(
            std::io::Cursor::new(tar_bytes),
            extract_dir.path(),
            false,
            false,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("`..`"), "{err}");
    }

    #[test]
    fn malicious_archive_absolute_path_fails() {
        let tar_bytes = craft_raw_tar(b"/etc/evil.txt", b"evil content");

        let extract_dir = tempfile::tempdir().unwrap();
        let err = extract_archive(
            std::io::Cursor::new(tar_bytes),
            extract_dir.path(),
            false,
            false,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("absolute path"), "{err}");
    }

    #[test]
    fn malicious_archive_hardlink_to_outside_fails() {
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_entry_type(tar::EntryType::Link);
        let path = b"pwned_link";
        header.as_mut_bytes()[..path.len()].copy_from_slice(path);
        // Link target in standard tar header is at offset 157..257 (100 bytes)
        let link_target = b"../../../../../../etc/passwd";
        header.as_mut_bytes()[157..157 + link_target.len()].copy_from_slice(link_target);
        header.set_cksum();

        let mut tar_bytes = Vec::new();
        tar_bytes.extend_from_slice(header.as_bytes());
        tar_bytes.extend(std::iter::repeat_n(0, 1024));

        let extract_dir = tempfile::tempdir().unwrap();
        let err = extract_archive(
            std::io::Cursor::new(tar_bytes),
            extract_dir.path(),
            false,
            false,
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("hardlink") || err.to_string().contains("`..`"),
            "{err}"
        );
    }

    #[test]
    fn malicious_archive_device_node_fails() {
        let mut tar_builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_entry_type(tar::EntryType::Block);
        header.set_cksum();
        tar_builder
            .append_data(&mut header, "dev_node", &[][..])
            .unwrap();
        let tar_bytes = tar_builder.into_inner().unwrap();

        let extract_dir = tempfile::tempdir().unwrap();
        let err = extract_archive(
            std::io::Cursor::new(tar_bytes),
            extract_dir.path(),
            false,
            false,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("disallowed entry type"), "{err}");
    }

    #[test]
    fn malicious_archive_symlink_escaping_write_through_fails() {
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = outside_dir.path().join("outside.txt");

        let mut tar_builder = tar::Builder::new(Vec::new());

        // 1. Symlink pointing outside
        let mut sym_header = tar::Header::new_gnu();
        sym_header.set_size(0);
        sym_header.set_entry_type(tar::EntryType::Symlink);
        sym_header.set_cksum();
        tar_builder
            .append_link(&mut sym_header, "sym_out", outside_dir.path())
            .unwrap();

        // 2. Write through symlink
        let data = b"pwned";
        let mut file_header = tar::Header::new_gnu();
        file_header.set_size(data.len() as u64);
        file_header.set_mode(0o644);
        file_header.set_entry_type(tar::EntryType::Regular);
        file_header.set_cksum();
        tar_builder
            .append_data(&mut file_header, "sym_out/pwned.txt", &data[..])
            .unwrap();

        let tar_bytes = tar_builder.into_inner().unwrap();
        let extract_dir = tempfile::tempdir().unwrap();

        let err = extract_archive(
            std::io::Cursor::new(tar_bytes),
            extract_dir.path(),
            false,
            false,
            None,
        )
        .unwrap_err();

        // Must fail and not write to outside_file
        assert!(!outside_file.exists());
        assert!(!outside_dir.path().join("pwned.txt").exists());
        assert!(
            err.to_string().contains("outside of destination path")
                || err.to_string().contains("failed path safety check")
        );
    }

    #[test]
    fn max_extracted_bytes_aborts_decompression_bomb() {
        let mut tar_builder = tar::Builder::new(Vec::new());
        let data = vec![b'A'; 2048];
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        tar_builder
            .append_data(&mut header, "big.txt", &data[..])
            .unwrap();
        let tar_bytes = tar_builder.into_inner().unwrap();

        let extract_dir = tempfile::tempdir().unwrap();
        let err = extract_archive(
            std::io::Cursor::new(tar_bytes),
            extract_dir.path(),
            false,
            false,
            Some(500), // limit to 500 bytes
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("exceeded maximum size limit"),
            "{err}"
        );
    }
}
