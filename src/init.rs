//! Implementation of `dvb init`.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::error::{ConfigError, EXIT_SUCCESS, Error, Result};

/// The embedded reference configuration template.
pub const REFERENCE_TEMPLATE: &str = include_str!("../templates/dvb.toml");

/// Execute the `dvb init` command.
///
/// # Errors
///
/// Returns [`Error::Io`] on write failure, or an error if the destination exists and `force` is not set.
pub fn run_init(output: &str, force: bool) -> Result<u8> {
    if output == "-" {
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(REFERENCE_TEMPLATE.as_bytes())
            .map_err(|source| Error::Io {
                path: Path::new("-").to_path_buf(),
                source,
            })?;
        stdout.flush().map_err(|source| Error::Io {
            path: Path::new("-").to_path_buf(),
            source,
        })?;
        return Ok(EXIT_SUCCESS);
    }

    let target_path = Path::new(output);
    write_init_file(target_path, force)?;

    eprintln!("Configuration written to {}", target_path.display());
    eprintln!("Next steps:");
    eprintln!(
        "  1. Edit storage credentials and settings in {}",
        target_path.display()
    );
    eprintln!("  2. Run `dvb jobs` to verify configured jobs");
    eprintln!("  3. Run `dvb check` to test storage and daemon connectivity");

    Ok(EXIT_SUCCESS)
}

/// Write the reference configuration to `target_path`.
///
/// # Errors
///
/// Returns [`Error::Config`] if `target_path` already exists and `force` is false,
/// or [`Error::Io`] on filesystem error.
pub fn write_init_file(target_path: &Path, force: bool) -> Result<()> {
    if let Some(parent) = target_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    if force {
        let parent = target_path.parent().unwrap_or_else(|| Path::new("."));
        let mut temp = tempfile::Builder::new()
            .prefix(".dvb-init-")
            .tempfile_in(parent)
            .map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;

        #[cfg(unix)]
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|source| Error::Io {
                path: target_path.to_path_buf(),
                source,
            })?;

        temp.write_all(REFERENCE_TEMPLATE.as_bytes())
            .map_err(|source| Error::Io {
                path: target_path.to_path_buf(),
                source,
            })?;
        temp.flush().map_err(|source| Error::Io {
            path: target_path.to_path_buf(),
            source,
        })?;

        temp.persist(target_path).map_err(|err| Error::Io {
            path: target_path.to_path_buf(),
            source: err.error,
        })?;
    } else {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);

        match options.open(target_path) {
            Ok(mut file) => {
                file.write_all(REFERENCE_TEMPLATE.as_bytes())
                    .map_err(|source| Error::Io {
                        path: target_path.to_path_buf(),
                        source,
                    })?;
                file.flush().map_err(|source| Error::Io {
                    path: target_path.to_path_buf(),
                    source,
                })?;
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                return Err(Error::Config(ConfigError::Invalid(format!(
                    "file `{}` already exists; use --force to overwrite",
                    target_path.display()
                ))));
            }
            Err(source) => {
                return Err(Error::Io {
                    path: target_path.to_path_buf(),
                    source,
                });
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ValidationMode};

    fn parse_and_validate(toml_str: &str) -> Config {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dvb.toml");
        std::fs::write(&path, toml_str).expect("write toml");
        Config::load(&path, ValidationMode::Static).expect("valid static config")
    }

    #[test]
    fn template_parses_and_validates_as_shipped() {
        let config = parse_and_validate(REFERENCE_TEMPLATE);
        assert_eq!(config.jobs.len(), 1);
        let job = &config.jobs[0];
        assert_eq!(job.name, "backup");
        assert_eq!(job.cron.as_deref(), Some("0 3 * * *"));
        assert_eq!(
            job.schedule_source,
            Some(crate::config::ScheduleSource::Crontext(
                "every day at 03:00".to_owned()
            ))
        );
        assert_eq!(job.filename, "backup-%Y%m%dT%H%M%SZ.tar.zst");
    }

    #[test]
    fn uncommenting_optional_lines_stays_valid() {
        let mut in_alt_storage = false;
        let mut lines = Vec::new();

        for line in REFERENCE_TEMPLATE.lines() {
            if line.starts_with("## alternative storage:") {
                in_alt_storage = true;
                lines.push(line.to_owned());
                continue;
            }
            if in_alt_storage {
                if line.starts_with('#') {
                    lines.push(line.to_owned());
                    continue;
                }
                in_alt_storage = false;
            }

            // Skip alternative cron line since crontext is active
            if line.trim() == "# cron = \"0 3 * * *\"" {
                lines.push(line.to_owned());
                continue;
            }

            // Uncomment optional configuration lines
            if let Some(uncommented) = line.strip_prefix("# ") {
                lines.push(uncommented.to_owned());
            } else {
                lines.push(line.to_owned());
            }
        }

        let full_toml = lines.join("\n");
        let config = parse_and_validate(&full_toml);
        assert_eq!(config.jobs.len(), 1);
        let job = &config.jobs[0];
        assert_eq!(job.timezone.as_deref(), Some("Europe/Berlin"));
        assert_eq!(job.stop_containers, vec!["postgres".to_string()]);
        assert_eq!(job.pre.len(), 1);
        assert_eq!(job.post.len(), 1);
    }

    #[test]
    fn alternative_storage_fs_stays_valid() {
        let fs_block = r#"
  [job.storage]
  type = "fs"
  root = "/backup/storage"
  prefix = "backups"
"#;
        let replaced = replace_storage_block(REFERENCE_TEMPLATE, fs_block);
        let config = parse_and_validate(&replaced);
        assert!(matches!(
            config.jobs[0].storage,
            crate::config::StorageConfig::Fs(_)
        ));
    }

    #[test]
    fn alternative_storage_sftp_stays_valid() {
        let sftp_block = r#"
  [job.storage]
  type = "sftp"
  endpoint = "sftp.example.com:22"
  user = "backupuser"
  root = "/remote/backups"
  key_path = "/run/secrets/id_ed25519"
  known_hosts_strategy = "strict"
"#;
        let replaced = replace_storage_block(REFERENCE_TEMPLATE, sftp_block);
        let config = parse_and_validate(&replaced);
        assert!(matches!(
            config.jobs[0].storage,
            crate::config::StorageConfig::Sftp(_)
        ));
    }

    #[test]
    fn alternative_storage_dropbox_stays_valid() {
        let dropbox_block = r#"
  [job.storage]
  type = "dropbox"
  root = "/backups"
  client_id = "/run/secrets/dropbox_client_id"
  client_secret = "/run/secrets/dropbox_client_secret"
  refresh_token = "/run/secrets/dropbox_refresh_token"
"#;
        let replaced = replace_storage_block(REFERENCE_TEMPLATE, dropbox_block);
        let config = parse_and_validate(&replaced);
        assert!(matches!(
            config.jobs[0].storage,
            crate::config::StorageConfig::Dropbox(_)
        ));
    }

    #[test]
    fn cron_replaces_crontext_stays_valid() {
        let mut lines = Vec::new();
        for line in REFERENCE_TEMPLATE.lines() {
            if line.starts_with("crontext = ") {
                lines.push("cron = \"0 3 * * *\"".to_owned());
            } else if line.trim() == "# cron = \"0 3 * * *\"" {
                // omit the commented cron
            } else {
                lines.push(line.to_owned());
            }
        }
        let replaced = lines.join("\n");
        let config = parse_and_validate(&replaced);
        assert_eq!(config.jobs[0].cron.as_deref(), Some("0 3 * * *"));
        assert_eq!(
            config.jobs[0].schedule_source,
            Some(crate::config::ScheduleSource::Cron)
        );
    }

    fn replace_storage_block(template: &str, new_storage: &str) -> String {
        let mut lines = Vec::new();
        let mut in_active_storage = false;

        for line in template.lines() {
            if line.trim() == "[job.storage]" {
                in_active_storage = true;
                lines.push(new_storage.to_owned());
                continue;
            }
            if in_active_storage {
                if line.starts_with("## alternative storage:") {
                    in_active_storage = false;
                    lines.push(line.to_owned());
                }
                continue;
            }
            lines.push(line.to_owned());
        }

        lines.join("\n")
    }

    #[test]
    fn overwrite_protection_and_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out_path = dir.path().join("dvb.toml");

        // First creation succeeds
        write_init_file(&out_path, false).expect("initial write");
        assert!(out_path.exists());

        #[cfg(unix)]
        {
            let meta = std::fs::metadata(&out_path).expect("metadata");
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }

        let initial_bytes = std::fs::read(&out_path).expect("read initial");
        assert_eq!(initial_bytes, REFERENCE_TEMPLATE.as_bytes());

        // Second creation without force fails
        let err = write_init_file(&out_path, false).expect_err("overwrite without force must fail");
        assert!(err.to_string().contains("--force"));
        let unchanged_bytes = std::fs::read(&out_path).expect("read after failed overwrite");
        assert_eq!(unchanged_bytes, initial_bytes);

        // Third creation with force succeeds
        write_init_file(&out_path, true).expect("overwrite with force");
        #[cfg(unix)]
        {
            let meta = std::fs::metadata(&out_path).expect("metadata");
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn reference_template_matches_golden() {
        let golden = include_str!("../tests/golden/dvb.toml.golden");
        assert_eq!(REFERENCE_TEMPLATE, golden);
    }
}
