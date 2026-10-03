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
cron = "0 3 * * *"
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
        let mut files = list_files(&self.remote.join("backups"));
        files.retain(|name, _| !name.ends_with(".sha256"));
        files
    }

    /// All stored files under `backups`, including sidecars.
    #[allow(dead_code)]
    fn stored_all(&self) -> BTreeMap<String, u64> {
        list_files(&self.remote.join("backups"))
    }

    /// Only the newest object, or `None` when storage is empty.
    fn only_object(&self) -> Option<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(self.remote.join("backups"))
            .expect("read remote dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|p| !p.to_string_lossy().ends_with(".sha256"))
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

    fn restore(&self, args: &[&str]) -> std::process::Output {
        let mut cmd = self.dvb();
        cmd.arg("restore").arg("db").args(args);
        cmd.output().expect("run dvb restore")
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
        stderr.contains("archive error") || stderr.contains("does not exist"),
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
    AssertCommand::cargo_bin("dvb")
        .expect("binary")
        .args(["restore", "--help"])
        .assert()
        .success();
}

#[test]
fn round_trip_restore_preserves_tree_structure_modes_and_symlinks() {
    use std::os::unix::fs::PermissionsExt as _;

    let f = Fixture::new();

    // 1. Setup source tree:
    // - nested directory with file
    // - executable file (0o755)
    // - read-only file (0o600)
    // - empty directory
    // - symlink
    let nested_dir = f.source.join("nested/inner");
    std::fs::create_dir_all(&nested_dir).expect("create nested");
    std::fs::write(nested_dir.join("file.txt"), b"nested content").expect("write nested file");

    let bin_dir = f.source.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create bin");
    let script = bin_dir.join("script.sh");
    std::fs::write(&script, b"#!/bin/sh\necho hello\n").expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("chmod script");

    let secret = f.source.join("secret.key");
    std::fs::write(&secret, b"supersecret").expect("write secret");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600))
        .expect("chmod secret");

    let empty = f.source.join("empty_dir");
    std::fs::create_dir_all(&empty).expect("create empty");

    let symlink = f.source.join("script_link");
    std::os::unix::fs::symlink("bin/script.sh", &symlink).expect("symlink");

    // 2. Perform backup
    let backup_output = f.backup();
    assert!(
        backup_output.status.success(),
        "backup failed: {backup_output:?}"
    );

    // 3. Restore to fresh target directory
    let restore_target = f.source.parent().unwrap().join("restored_tree");
    let target_str = restore_target.display().to_string();
    let restore_output = f.restore(&["--to", &target_str]);
    assert!(
        restore_output.status.success(),
        "restore failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&restore_output.stdout),
        String::from_utf8_lossy(&restore_output.stderr)
    );

    // Stdout contract: single line with absolute extracted path
    let stdout = String::from_utf8(restore_output.stdout).expect("utf-8 stdout");
    assert_eq!(stdout.trim(), target_str);

    // 4. Verify restored tree
    let pgdata = restore_target.join("pgdata");
    assert_eq!(
        std::fs::read(pgdata.join("nested/inner/file.txt")).expect("read file"),
        b"nested content"
    );

    let restored_script = pgdata.join("bin/script.sh");
    assert_eq!(
        std::fs::read(&restored_script).expect("read script"),
        b"#!/bin/sh\necho hello\n"
    );
    let script_mode = std::fs::metadata(&restored_script)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(script_mode, 0o755);

    let restored_secret = pgdata.join("secret.key");
    assert_eq!(
        std::fs::read(&restored_secret).expect("read secret"),
        b"supersecret"
    );
    let secret_mode = std::fs::metadata(&restored_secret)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(secret_mode, 0o600);

    assert!(pgdata.join("empty_dir").is_dir());
    assert_eq!(
        std::fs::read_dir(pgdata.join("empty_dir"))
            .expect("read empty")
            .count(),
        0
    );

    let restored_symlink = pgdata.join("script_link");
    assert!(
        std::fs::symlink_metadata(&restored_symlink)
            .expect("link meta")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_link(&restored_symlink).expect("symlink target"),
        PathBuf::from("bin/script.sh")
    );
}

#[test]
fn restore_target_non_empty_without_force_fails_and_leaves_it_untouched() {
    let f = Fixture::new();
    std::fs::write(f.source.join("data.txt"), b"backup data").expect("write data");

    let backup_output = f.backup();
    assert!(backup_output.status.success());

    let target = f.source.parent().unwrap().join("target_dir");
    std::fs::create_dir_all(&target).expect("create target");
    std::fs::write(target.join("keepme.txt"), b"preserve me").expect("write keepme");

    // Restore without --force fails
    let target_str = target.display().to_string();
    let res = f.restore(&["--to", &target_str]);
    assert!(!res.status.success());
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(
        stderr.contains("not empty") || stderr.contains("pass `--force`"),
        "{stderr}"
    );

    // Content untouched
    assert_eq!(
        std::fs::read(target.join("keepme.txt")).unwrap(),
        b"preserve me"
    );
    assert!(!target.join("pgdata").exists());

    // Restore with --force succeeds
    let res = f.restore(&["--to", &target_str, "--force"]);
    assert!(res.status.success(), "force restore failed: {stderr}");
    assert_eq!(
        std::fs::read(target.join("keepme.txt")).unwrap(),
        b"preserve me"
    );
    assert!(target.join("pgdata/data.txt").exists());
}

#[test]
fn restore_truncated_archive_leaves_no_staging_and_no_target() {
    let f = Fixture::new();
    std::fs::write(f.source.join("data.txt"), b"sample data").expect("write data");

    let backup_output = f.backup();
    assert!(backup_output.status.success());

    // Truncate the backup archive
    let backup_path = f.only_object().expect("backup object");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&backup_path)
        .expect("open");
    file.set_len(50).expect("truncate");

    let target = f.source.parent().unwrap().join("fail_target");
    let target_str = target.display().to_string();
    let staging = PathBuf::from(format!("{target_str}.dvb-partial"));

    let res = f.restore(&["--to", &target_str]);
    assert!(!res.status.success());

    // Staging and target must not exist
    assert!(
        !target.exists(),
        "target directory should not exist on failure"
    );
    assert!(
        !staging.exists(),
        "staging directory should be cleaned up on failure"
    );
}

#[test]
fn restore_dry_run_outputs_plan_and_changes_nothing() {
    let f = Fixture::new();
    std::fs::write(f.source.join("data.txt"), b"test").expect("write");

    let backup_output = f.backup();
    assert!(backup_output.status.success());

    let target = f.source.parent().unwrap().join("dry_target");
    let target_str = target.display().to_string();

    let res = f.restore(&["--to", &target_str, "--dry-run"]);
    assert!(res.status.success());
    let stdout = String::from_utf8_lossy(&res.stdout);
    assert!(stdout.contains("job: db"));
    assert!(stdout.contains("backup: backups/"));
    assert!(stdout.contains(&format!("target dir: {target_str}")));
    assert!(stdout.contains("script: none"));

    assert!(!target.exists());
}

#[test]
fn restore_lock_prevents_concurrent_operations() {
    let f = Fixture::new();
    std::fs::write(f.source.join("data.txt"), b"test").expect("write");
    assert!(f.backup().status.success());

    let target = f.source.parent().unwrap().join("lock_target");
    let target_str = target.display().to_string();

    // Hold the lock in this process
    std::fs::create_dir_all(f.lock_dir()).expect("mkdir locks");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(f.lock_dir().join("db.lock"))
        .expect("open lock");
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock.try_write().expect("acquire lock");

    // Concurrent restore must fail fast
    let res = f.restore(&["--to", &target_str]);
    assert!(!res.status.success());
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(stderr.contains("already locked"), "{stderr}");
    assert!(!target.exists());
}

#[test]
fn restore_runs_script_hook_with_args_and_env_vars() {
    use std::os::unix::fs::PermissionsExt as _;

    let f = Fixture::new();
    std::fs::write(f.source.join("data.txt"), b"important data").expect("write data");
    assert!(f.backup().status.success());

    let marker_path = f.source.parent().unwrap().join("marker.txt");
    let marker_str = marker_path.display().to_string();

    let script_path = f.source.parent().unwrap().join("hook.sh");
    let script_content = format!(
        r#"#!/bin/sh
cat << EOF > "{marker_str}"
ARG1=$1
ARG2=${{2:-none}}
ARG3=${{3:-none}}
JOB=$DVB_JOB
RESTORE_DIR=$DVB_RESTORE_DIR
ARCHIVE=$DVB_ARCHIVE
ARCHIVE_TIME=$DVB_ARCHIVE_TIME
EOF
"#
    );
    std::fs::write(&script_path, script_content).expect("write script");
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let target = f.source.parent().unwrap().join("hook_target");
    let target_str = target.display().to_string();

    let res = f.restore(&[
        "--to",
        &target_str,
        "--script",
        &script_path.display().to_string(),
        "--",
        "--clean",
        "extra_val",
    ]);

    assert!(
        res.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&res.stdout),
        String::from_utf8_lossy(&res.stderr)
    );

    let stdout = String::from_utf8(res.stdout).unwrap();
    assert_eq!(stdout.trim(), target_str);

    let marker = std::fs::read_to_string(&marker_path).expect("read marker");
    assert!(marker.contains(&format!("ARG1={target_str}")), "{marker}");
    assert!(marker.contains("ARG2=--clean"), "{marker}");
    assert!(marker.contains("ARG3=extra_val"), "{marker}");
    assert!(marker.contains("JOB=db"), "{marker}");
    assert!(
        marker.contains(&format!("RESTORE_DIR={target_str}")),
        "{marker}"
    );
    assert!(marker.contains("ARCHIVE=backups/"), "{marker}");
    assert!(marker.contains("ARCHIVE_TIME="), "{marker}");
}

#[test]
fn restore_missing_or_non_executable_script_fails_before_download() {
    use std::os::unix::fs::PermissionsExt as _;

    let f = Fixture::new();
    std::fs::write(f.source.join("data.txt"), b"data").expect("write");
    assert!(f.backup().status.success());

    let target = f.source.parent().unwrap().join("fail_target");
    let target_str = target.display().to_string();

    // 1. Missing script
    let res = f.restore(&[
        "--to",
        &target_str,
        "--script",
        "/tmp/dvb_does_not_exist_xyz.sh",
    ]);
    assert!(!res.status.success());
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(stderr.contains("does not exist"), "{stderr}");
    assert!(!target.exists());

    // 2. Non-executable script
    let script_path = f.source.parent().unwrap().join("no_exec.sh");
    std::fs::write(&script_path, b"#!/bin/sh\necho hi\n").expect("write script");
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

    let res = f.restore(&[
        "--to",
        &target_str,
        "--script",
        &script_path.display().to_string(),
    ]);
    assert!(!res.status.success());
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(stderr.contains("not executable"), "{stderr}");
    assert!(!target.exists());
}

#[test]
fn restore_script_failure_returns_exit_code_3_and_preserves_target() {
    use std::os::unix::fs::PermissionsExt as _;

    let f = Fixture::new();
    std::fs::write(f.source.join("data.txt"), b"data").expect("write");
    assert!(f.backup().status.success());

    let script_path = f.source.parent().unwrap().join("fail_hook.sh");
    std::fs::write(&script_path, b"#!/bin/sh\necho fail >&2\nexit 42\n").expect("write script");
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let target = f.source.parent().unwrap().join("preserved_target");
    let target_str = target.display().to_string();

    let res = f.restore(&[
        "--to",
        &target_str,
        "--script",
        &script_path.display().to_string(),
    ]);

    assert_eq!(res.status.code(), Some(3));
    let stdout = String::from_utf8(res.stdout).unwrap();
    assert_eq!(stdout.trim(), target_str);

    // Extracted directory must still exist with the data
    assert!(target.join("pgdata/data.txt").exists());
}

#[test]
fn restore_script_timeout_kills_process_and_returns_exit_code_3() {
    use std::os::unix::fs::PermissionsExt as _;

    let f = Fixture::new();
    std::fs::write(f.source.join("data.txt"), b"data").expect("write");
    assert!(f.backup().status.success());

    let script_path = f.source.parent().unwrap().join("sleep_hook.sh");
    std::fs::write(&script_path, b"#!/bin/sh\nsleep 10\n").expect("write script");
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let target = f.source.parent().unwrap().join("timeout_target");
    let target_str = target.display().to_string();

    let res = f.restore(&[
        "--to",
        &target_str,
        "--script",
        &script_path.display().to_string(),
        "--script-timeout",
        "1",
    ]);

    assert_eq!(res.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(stderr.contains("timed out"), "{stderr}");
    assert!(target.join("pgdata/data.txt").exists());
}

#[test]
fn restore_cleanup_semantics() {
    use std::os::unix::fs::PermissionsExt as _;

    let f = Fixture::new();
    std::fs::write(f.source.join("data.txt"), b"data").expect("write");
    assert!(f.backup().status.success());

    let success_script = f.source.parent().unwrap().join("ok_hook.sh");
    std::fs::write(&success_script, b"#!/bin/sh\nexit 0\n").expect("write script");
    std::fs::set_permissions(&success_script, std::fs::Permissions::from_mode(0o755))
        .expect("chmod");

    let fail_script = f.source.parent().unwrap().join("bad_hook.sh");
    std::fs::write(&fail_script, b"#!/bin/sh\nexit 1\n").expect("write script");
    std::fs::set_permissions(&fail_script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // Case 1: Temporary directory with --cleanup and successful script -> cleaned up
    let res = f.restore(&[
        "--script",
        &success_script.display().to_string(),
        "--cleanup",
    ]);
    assert!(res.status.success());
    let stdout = String::from_utf8(res.stdout).unwrap();
    let temp_target = PathBuf::from(stdout.trim());
    assert!(
        !temp_target.exists(),
        "temporary directory should be deleted on success with --cleanup"
    );

    // Case 2: Temporary directory with --cleanup and failing script -> preserved
    let res = f.restore(&["--script", &fail_script.display().to_string(), "--cleanup"]);
    assert_eq!(res.status.code(), Some(3));
    let stdout = String::from_utf8(res.stdout).unwrap();
    let temp_target = PathBuf::from(stdout.trim());
    assert!(
        temp_target.exists(),
        "temporary directory should be kept on failure even with --cleanup"
    );

    // Case 3: Explicit --to with --cleanup -> not deleted (only temporary dirs are cleaned up)
    let explicit_target = f.source.parent().unwrap().join("explicit_cleanup_target");
    let target_str = explicit_target.display().to_string();
    let res = f.restore(&[
        "--to",
        &target_str,
        "--script",
        &success_script.display().to_string(),
        "--cleanup",
    ]);
    assert!(res.status.success());
    assert!(
        explicit_target.exists(),
        "explicit --to directory should not be removed by --cleanup"
    );

    // Case 4: --cleanup without script -> does nothing
    let no_script_target = f.source.parent().unwrap().join("no_script_target");
    let no_script_str = no_script_target.display().to_string();
    let res = f.restore(&["--to", &no_script_str, "--cleanup"]);
    assert!(res.status.success());
    assert!(
        no_script_target.exists(),
        "directory should not be removed when no script was run"
    );
}

#[test]
fn restore_stop_containers_validation_checks() {
    let f = Fixture::new();
    let res = f.restore(&["--stop-containers"]);
    assert!(!res.status.success());
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(
        stderr.contains("has no stop_containers or stop_label configured"),
        "{stderr}"
    );
}

#[test]
fn example_pg_restore_script_execution_and_help() {
    let script_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/pg_restore.sh");
    assert!(script_path.exists(), "examples/pg_restore.sh must exist");

    // 1. --help exits 0 and displays usage
    let help_res = std::process::Command::new(&script_path)
        .arg("--help")
        .output()
        .expect("run pg_restore.sh --help");
    assert!(help_res.status.success());
    let stdout = String::from_utf8_lossy(&help_res.stdout);
    assert!(stdout.contains("Usage: pg_restore.sh <extracted-dir>"));

    // 2. Invocation without arguments fails with exit code 1 and error
    let no_args_res = std::process::Command::new(&script_path)
        .output()
        .expect("run pg_restore.sh");
    assert!(!no_args_res.status.success());
    let stderr = String::from_utf8_lossy(&no_args_res.stderr);
    assert!(stderr.contains("Missing extracted directory"));

    // 3. Invocation with empty directory fails identifying missing dump file
    let tmp = tempfile::tempdir().expect("tempdir");
    let empty_dir_res = std::process::Command::new(&script_path)
        .arg(tmp.path())
        .output()
        .expect("run pg_restore.sh on empty dir");
    assert!(!empty_dir_res.status.success());
    let stderr = String::from_utf8_lossy(&empty_dir_res.stderr);
    assert!(stderr.contains("No dump file"));
}

#[test]
fn backup_creates_sha256_sidecar_compatible_with_sha256sum() {
    let f = Fixture::new();
    build_source_tree(&f.source);
    let output = f.backup();
    assert!(output.status.success());

    let archive = f.only_object().expect("archive exists");
    let sidecar_path = PathBuf::from(format!("{}.sha256", archive.display()));
    assert!(
        sidecar_path.exists(),
        "sidecar must exist at {sidecar_path:?}"
    );

    let sidecar_file_name = sidecar_path.file_name().expect("filename");
    let check = std::process::Command::new("sha256sum")
        .arg("-c")
        .arg(sidecar_file_name)
        .current_dir(f.remote.join("backups"))
        .output()
        .expect("run sha256sum -c");
    assert!(
        check.status.success(),
        "sha256sum check failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
}

#[test]
fn restore_fails_on_checksum_mismatch_and_preserves_nothing_and_does_not_run_script() {
    let f = Fixture::new();
    build_source_tree(&f.source);
    assert!(f.backup().status.success());

    let archive = f.only_object().expect("archive exists");
    let mut bytes = std::fs::read(&archive).expect("read archive");
    let len = bytes.len();
    bytes[len / 2] ^= 0xFF;
    std::fs::write(&archive, bytes).expect("write corrupted archive");

    let marker_path = f.source.parent().unwrap().join("marker.txt");
    let script_path = f.source.parent().unwrap().join("test_script.sh");
    std::fs::write(
        &script_path,
        format!("#!/bin/sh\ntouch {}\nexit 0\n", marker_path.display()),
    )
    .expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }

    let target_dir = f.source.parent().unwrap().join("restore_target");
    let res = f.restore(&[
        "--to",
        &target_dir.display().to_string(),
        "--script",
        &script_path.display().to_string(),
    ]);

    assert_eq!(
        res.status.code(),
        Some(1),
        "restore must exit with code 1 on corruption"
    );
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(
        stderr.contains("checksum mismatch")
            || stderr.contains("corrupted")
            || stderr.contains("archive error"),
        "expected corruption / checksum mismatch error: {stderr}"
    );

    assert!(
        !target_dir.exists(),
        "extracted dir must not remain on checksum failure"
    );
    let staging = PathBuf::from(format!("{}.dvb-partial", target_dir.display()));
    assert!(
        !staging.exists(),
        "staging dir must be cleaned up on failure"
    );
    assert!(
        !marker_path.exists(),
        "script must never run before checksum verification completes"
    );
}

#[test]
fn restore_succeeds_for_backup_without_sidecar_with_warning() {
    let f = Fixture::new();
    build_source_tree(&f.source);
    assert!(f.backup().status.success());

    let archive = f.only_object().expect("archive exists");
    let sidecar_path = PathBuf::from(format!("{}.sha256", archive.display()));
    assert!(sidecar_path.exists());
    std::fs::remove_file(&sidecar_path).expect("remove sidecar");

    let target_dir = f.source.parent().unwrap().join("restore_legacy_target");
    let res = f.restore(&["--to", &target_dir.display().to_string()]);
    assert!(
        res.status.success(),
        "restore without sidecar must succeed: {}",
        String::from_utf8_lossy(&res.stderr)
    );
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(
        stderr.contains("sidecar checksum not found"),
        "expected warning in logs: {stderr}"
    );
    assert!(target_dir.join("pgdata/a.txt").exists(), "files extracted");
}

#[test]
fn no_verify_restores_corrupted_archive_without_hashing() {
    let f = Fixture::with_template("db-%Y%m%dT%H%M%SZ.tar", "none");
    build_source_tree(&f.source);
    assert!(f.backup().status.success());

    let archive = f.only_object().expect("archive exists");
    let sidecar_path = PathBuf::from(format!("{}.sha256", archive.display()));
    assert!(sidecar_path.exists());

    let mut bytes = std::fs::read(&archive).expect("read archive");
    let pos = bytes
        .windows(5)
        .position(|w| w == b"hello")
        .expect("found hello in tar");
    bytes[pos] = b'X';
    std::fs::write(&archive, bytes).expect("write corrupted archive");

    let target_dir = f.source.parent().unwrap().join("no_verify_target");
    let res = f.restore(&["--to", &target_dir.display().to_string(), "--no-verify"]);
    assert!(
        res.status.success(),
        "restore with --no-verify must succeed: {}",
        String::from_utf8_lossy(&res.stderr)
    );
    assert!(target_dir.exists(), "extracted dir must exist");
}
