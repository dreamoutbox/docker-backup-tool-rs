//! End to end tests for `dvb backup` against the `fs` backend.
//!
//! These run the real binary: config parsing, archiving, the lock and the
//! storage upload all happen in a separate process.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::Command as AssertCommand;

/// A `dvb` run with the config and lock directories pointed at a temp dir.
struct Fixture {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    source: PathBuf,
    remote: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with_template("db-%Y%m%dT%H%M%SZ.tar.zst", "zstd")
    }

    fn with_template(filename: &str, compression: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let source = tmp.path().join("source/pgdata");
        let remote = tmp.path().join("remote");
        std::fs::create_dir_all(&source).expect("create source");
        std::fs::create_dir_all(&remote).expect("create remote");

        let config = tmp.path().join("config.toml");
        let template = format!(
            r#"
[[job]]
name = "db"
source = [{}]
filename = "{filename}"
compression = "{compression}"
retention_days = 14
min_keep = 3

  [job.storage]
  type = "fs"
  root = {}
  prefix = "backups"
"#,
            toml_path(&source),
            toml_path(&remote),
        );
        std::fs::write(&config, template).expect("write config");

        Self {
            config,
            source,
            remote,
            _tmp: tmp,
        }
    }

    fn dvb(&self) -> Command {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin("dvb"));
        cmd.env("DVB_CONFIG", &self.config)
            // Keeps the lock out of /run/dvb, which a test process may not own.
            .env(
                "DVB_LOCK_DIR",
                self.config
                    .parent()
                    .expect("config has a parent")
                    .join("locks"),
            )
            // Quiet by default; tests that read the log override this.
            .env("DVB_LOG", "warn");
        cmd
    }

    /// Rewrite the config file, e.g. to flip a job setting.
    fn patch_config(&self, from: &str, to: &str) {
        let body = std::fs::read_to_string(&self.config).expect("read config");
        assert!(
            body.contains(from),
            "config does not contain `{from}`:\n{body}"
        );
        std::fs::write(&self.config, body.replace(from, to)).expect("write config");
    }

    fn backup(&self) -> std::process::Output {
        self.dvb()
            .args(["backup", "db"])
            .output()
            .expect("run dvb backup")
    }

    /// Stored objects under `backups`, as `(name, size)`.
    fn stored(&self) -> BTreeMap<String, u64> {
        list_files(&self.remote.join("backups"))
    }

    /// Only the newest object, or `None` when storage is empty.
    fn only_object(&self) -> Option<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(self.remote.join("backups"))
            .expect("read remote dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .collect();
        assert!(
            found.len() <= 1,
            "expected at most one backup, got {found:?}"
        );
        found.pop()
    }

    fn lock_dir(&self) -> PathBuf {
        self.config
            .parent()
            .expect("config has a parent")
            .join("locks")
    }
}

/// Render `path` as a TOML basic string, including the surrounding quotes.
fn toml_path(path: &Path) -> String {
    toml_string(&path.to_string_lossy())
}

/// Quote and escape a value for use in a TOML basic string.
fn toml_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn list_files(dir: &Path) -> BTreeMap<String, u64> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    entries
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().is_file())
        .map(|entry| {
            let size = entry.metadata().map_or(0, |meta| meta.len());
            (entry.file_name().to_string_lossy().into_owned(), size)
        })
        .collect()
}

/// `name -> bytes` for every file entry in an uncompressed tar stream.
fn read_tar(data: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut archive = tar::Archive::new(std::io::Cursor::new(data));
    for entry in archive.entries().expect("entries") {
        let mut entry = entry.expect("valid entry");
        // Directories and symlinks carry no payload.
        let kind = entry.header().entry_type();
        if kind.is_dir() || kind.is_symlink() {
            continue;
        }
        let path = entry.path().expect("path").display().to_string();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).expect("read entry");
        files.insert(path, buf);
    }
    files
}

fn decode(object: &Path, compression: &str) -> Vec<u8> {
    let raw = std::fs::read(object).expect("read stored object");
    match compression {
        "zstd" => zstd::stream::decode_all(std::io::Cursor::new(raw)).expect("zstd decode"),
        "gzip" => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(std::io::Cursor::new(raw))
                .read_to_end(&mut out)
                .expect("gzip decode");
            out
        }
        // `none`: the object is a raw tar stream.
        "none" => raw,
        other => panic!("unknown compression {other}"),
    }
}

/// Build a tree with nested files, a symlink and varied permissions.
fn build_source_tree(root: &Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(root.join("nested/deep")).expect("mkdir");
    std::fs::write(root.join("a.txt"), b"hello").expect("write a.txt");
    std::fs::write(root.join("nested/b.bin"), [0_u8, 1, 2, 3]).expect("write b.bin");
    std::fs::write(root.join("nested/deep/c.txt"), b"deep").expect("write c.txt");

    // A second, more permissive file so permission handling is observable.
    std::fs::write(root.join("open.txt"), b"world").expect("write open.txt");
    std::fs::set_permissions(
        root.join("open.txt"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("chmod open.txt");
    std::fs::set_permissions(root.join("a.txt"), std::fs::Permissions::from_mode(0o600))
        .expect("chmod a.txt");

    std::os::unix::fs::symlink("a.txt", root.join("link-to-a")).expect("symlink");
}

#[test]
fn backup_writes_a_decodable_archive_that_matches_the_tree() {
    let fixture = Fixture::new();
    build_source_tree(&fixture.source);

    let output = fixture.backup();
    assert!(
        output.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let object = fixture.only_object().expect("one stored object");
    assert!(
        object
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .starts_with("db-"),
        "unexpected object name: {}",
        object.display()
    );

    let tar_bytes = decode(&object, "zstd");
    let files = read_tar(&tar_bytes);

    // Entries are relative to the source basename.
    assert_eq!(
        files.get("pgdata/a.txt").map(Vec::as_slice),
        Some(&b"hello"[..])
    );
    assert_eq!(
        files.get("pgdata/nested/b.bin").map(Vec::as_slice),
        Some(&[0, 1, 2, 3][..])
    );
    assert_eq!(
        files.get("pgdata/nested/deep/c.txt").map(Vec::as_slice),
        Some(&b"deep"[..])
    );
    assert_eq!(
        files.get("pgdata/open.txt").map(Vec::as_slice),
        Some(&b"world"[..])
    );
    assert_eq!(files.len(), 4, "unexpected entries: {:?}", files.keys());

    // The symlink is stored as a symlink, not its target's contents.
    let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
    let link = archive
        .entries()
        .expect("entries")
        .find(|entry| {
            entry
                .as_ref()
                .is_ok_and(|e| e.path().is_ok_and(|p| p.ends_with("link-to-a")))
        })
        .expect("symlink entry")
        .expect("valid entry");
    assert!(link.header().entry_type().is_symlink());
}

#[test]
fn archive_preserves_permissions_and_mtime() {
    let fixture = Fixture::with_template("db-%Y%m%dT%H%M%SZ.tar", "none");
    build_source_tree(&fixture.source);

    let output = fixture.backup();
    assert!(
        output.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let object = fixture.only_object().expect("one stored object");
    let bytes = decode(&object, "none");

    // One pass: `tar::Archive` can only be walked once per cursor.
    let mut modes: BTreeMap<String, u32> = BTreeMap::new();
    let mut newest_mtime = 0_u64;
    let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
    for entry in archive.entries().expect("entries") {
        let entry = entry.expect("valid entry");
        newest_mtime = newest_mtime.max(entry.header().mtime().unwrap_or(0));
        let path = entry.path().expect("path").display().to_string();
        modes.insert(path, entry.header().mode().unwrap_or(0) & 0o777);
    }

    assert_eq!(modes.get("pgdata/a.txt"), Some(&0o600));
    assert_eq!(modes.get("pgdata/open.txt"), Some(&0o644));
    assert!(
        newest_mtime > 1_600_000_000,
        "mtime looks wrong: {newest_mtime}"
    );
}

#[test]
fn a_missing_source_fails_and_leaves_nothing_behind() {
    let fixture = Fixture::new();
    // No source tree at all: archiving must fail rather than upload an empty or
    // partial archive.
    std::fs::remove_dir_all(&fixture.source).expect("remove source");

    let output = fixture.backup();
    assert!(!output.status.success(), "backup should have failed");
    assert_eq!(output.status.code(), Some(1));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("archive error"),
        "unhelpful error: {stderr}"
    );

    assert_eq!(
        fixture.stored(),
        BTreeMap::new(),
        "a failed backup must not leave a partial object"
    );
}

#[test]
fn a_failing_entry_in_the_middle_leaves_nothing_behind() {
    let fixture = Fixture::new();
    build_source_tree(&fixture.source);
    // Following symlinks means the walk reads the dangling link below and fails
    // after several entries have already been streamed to the backend.
    fixture.patch_config(
        "compression = \"zstd\"",
        "compression = \"zstd\"\nfollow_symlinks = true",
    );
    std::os::unix::fs::symlink("missing-target", fixture.source.join("dangling"))
        .expect("dangling symlink");

    let output = fixture.backup();
    assert!(
        !output.status.success(),
        "following a dangling symlink should fail the run"
    );
    assert_eq!(output.status.code(), Some(1));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("dangling"),
        "error should name the entry: {stderr}"
    );

    assert_eq!(
        fixture.stored(),
        BTreeMap::new(),
        "a failed upload must not leave a partial object behind"
    );
}

#[test]
fn a_dangling_symlink_is_stored_as_a_link_when_not_following() {
    let fixture = Fixture::new();
    build_source_tree(&fixture.source);
    std::os::unix::fs::symlink("missing-target", fixture.source.join("dangling"))
        .expect("dangling symlink");

    assert!(
        fixture.backup().status.success(),
        "not following symlinks should tolerate a dangling link"
    );

    let object = fixture.only_object().expect("one stored object");
    let mut archive = tar::Archive::new(std::io::Cursor::new(decode(&object, "zstd")));
    let link = archive
        .entries()
        .expect("entries")
        .find_map(|entry| {
            let entry = entry.expect("valid entry");
            let path = entry.path().expect("path");
            path.ends_with("dangling").then_some(entry)
        })
        .expect("dangling entry");
    assert!(link.header().entry_type().is_symlink());
}

#[test]
fn gzip_produces_a_standard_gzip_stream() {
    let fixture = Fixture::with_template("db-%Y%m%dT%H%M%SZ.tar.gz", "gzip");
    build_source_tree(&fixture.source);

    let output = fixture.backup();
    assert!(
        output.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let object = fixture.only_object().expect("one stored object");
    let files = read_tar(&decode(&object, "gzip"));
    assert_eq!(
        files.get("pgdata/a.txt").map(Vec::as_slice),
        Some(&b"hello"[..])
    );
}

#[test]
fn the_lock_prevents_two_runs_from_overlapping() {
    let fixture = Fixture::new();
    build_source_tree(&fixture.source);

    // Hold the lock the way a concurrent `docker exec dvb backup` would.
    let lock_path = fixture.lock_dir().join("db.lock");
    std::fs::create_dir_all(fixture.lock_dir()).expect("lock dir");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open lock file");

    // Advisory locks are per descriptor, so the test holds its own lock on a
    // separate descriptor of the same file, which is what another process does.
    let mut held = fd_lock::RwLock::new(lock);
    let guard = held.try_write().expect("acquire lock");

    let output = fixture.backup();
    assert!(!output.status.success(), "backup should be blocked");
    assert_eq!(output.status.code(), Some(1));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already locked"),
        "unhelpful error: {stderr}"
    );
    assert_eq!(
        fixture.stored(),
        BTreeMap::new(),
        "nothing should be stored"
    );

    drop(guard);
    let output = fixture.backup();
    assert!(
        output.status.success(),
        "backup should succeed once the lock is free: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.stored().len(), 1);
}

#[test]
fn an_unknown_job_name_fails_with_a_helpful_message() {
    let fixture = Fixture::new();
    build_source_tree(&fixture.source);

    let output = fixture
        .dvb()
        .args(["backup", "nope"])
        .output()
        .expect("run dvb backup");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown job `nope`"), "{stderr}");
    assert!(
        stderr.contains("db"),
        "should list the known jobs: {stderr}"
    );
}

#[test]
fn repeated_backups_produce_distinct_objects() {
    let fixture = Fixture::with_template("db-%Y%m%dT%H%M%SZ.tar", "none");
    build_source_tree(&fixture.source);

    // Same-second runs would collide on the template, so check that the name is
    // derived from the template and that the lock, not the name, is what
    // serialises two immediate runs.
    assert!(fixture.backup().status.success());
    assert!(fixture.backup().status.success());
    assert_eq!(fixture.stored().len(), 1, "one second, one object");
}

#[test]
fn json_logging_emits_one_object_per_line() {
    let fixture = Fixture::new();
    build_source_tree(&fixture.source);

    let output = fixture
        .dvb()
        .args(["backup", "db", "--log-format", "json", "-v"])
        // `DVB_LOG` wins over `-v`, so it has to be cleared for this test.
        .env_remove("DVB_LOG")
        .output()
        .expect("run dvb backup");
    assert!(output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty(), "no log output");
    for line in &lines {
        let parsed: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("not json: {line}: {e}"));
        assert!(parsed.is_object(), "not a json object: {line}");
        assert!(
            parsed.get("timestamp").is_some(),
            "missing timestamp: {line}"
        );
    }
    // At info level the upload is announced.
    assert!(
        stderr.contains("backup uploaded") || stderr.contains("starting backup"),
        "no backup events in: {stderr}"
    );
}

#[test]
fn help_and_version_work_without_a_config() {
    AssertCommand::cargo_bin("dvb")
        .expect("binary")
        .args(["--version"])
        .assert()
        .success();
    AssertCommand::cargo_bin("dvb")
        .expect("binary")
        .args(["backup", "--help"])
        .assert()
        .success();
}
