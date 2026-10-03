use std::time::Instant;

use assert_cmd::Command;
use serde_json::Value;

fn dvb() -> Command {
    Command::cargo_bin("dvb").expect("dvb binary is built by cargo test")
}

#[test]
fn missing_config_file_exits_1_with_dvb_init_hint() {
    let assert = dvb()
        .args(["jobs", "--config", "/tmp/nonexistent-dvb-config.toml"])
        .assert()
        .failure()
        .code(1);

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf-8 stderr");
    assert!(
        stderr.contains(
            "config not found at /tmp/nonexistent-dvb-config.toml; run \"dvb init\" to create one"
        ),
        "stderr missing init hint: {stderr}"
    );
}

#[test]
fn empty_jobs_prints_no_jobs_configured_and_exits_0() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("empty.toml");
    std::fs::write(&config_path, "shutdown_grace_secs = 30\n").expect("write empty config");

    // Table format
    let assert = dvb()
        .args(["jobs", "--config", config_path.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
    assert_eq!(stdout.trim(), "no jobs configured");

    // JSON format
    let assert_json = dvb()
        .args([
            "jobs",
            "--config",
            config_path.to_str().unwrap(),
            "--format",
            "json",
        ])
        .assert()
        .success();
    let stdout_json =
        String::from_utf8(assert_json.get_output().stdout.clone()).expect("utf-8 stdout");
    let parsed: Value = serde_json::from_str(&stdout_json).expect("valid json");
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["jobs"], serde_json::json!([]));
}

#[test]
fn jobs_offline_golden_cli() {
    let fixture_path = "tests/fixtures/jobs_fixture.toml";
    let now_arg = "2026-10-02T12:00:00Z";

    // Table output (piped, so non-TTY plain style)
    let assert_table = dvb()
        .args(["jobs", "--config", fixture_path, "--now", now_arg])
        .assert()
        .success();
    let stdout_table =
        String::from_utf8(assert_table.get_output().stdout.clone()).expect("utf-8 stdout");
    let golden_table = include_str!("golden/jobs_table.golden");
    assert_eq!(stdout_table, golden_table);

    // JSON output (compact when piped)
    let assert_json = dvb()
        .args([
            "jobs",
            "--config",
            fixture_path,
            "--format",
            "json",
            "--now",
            now_arg,
        ])
        .assert()
        .success();
    let stdout_json =
        String::from_utf8(assert_json.get_output().stdout.clone()).expect("utf-8 stdout");
    let parsed: Value = serde_json::from_str(&stdout_json).expect("valid json");
    let golden_val: Value =
        serde_json::from_str(include_str!("golden/jobs_json.golden")).expect("valid golden json");
    assert_eq!(parsed, golden_val);
}

#[test]
fn jobs_ignores_missing_sources_and_touches_no_docker_or_locks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock_dir = dir.path().join("locks");
    std::fs::create_dir_all(&lock_dir).expect("create lock dir");

    let config_path = dir.path().join("dvb.toml");
    std::fs::write(
        &config_path,
        r#"
[[job]]
name = "standalone"
cron = "0 3 * * *"
source = ["/completely/nonexistent/path/12345"]
filename = "bk-%Y%m%dT%H%M%SZ.tar.zst"
  [job.storage]
  type = "fs"
  root = "/tmp"
"#,
    )
    .expect("write config");

    let assert = dvb()
        .args(["jobs", "--config", config_path.to_str().unwrap()])
        .env("DVB_LOCK_DIR", lock_dir.to_str().unwrap())
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
    assert!(stdout.contains("standalone"));

    // Verify lock directory has no files created
    let lock_files: Vec<_> = std::fs::read_dir(&lock_dir)
        .expect("read lock dir")
        .collect();
    assert!(lock_files.is_empty(), "lock files must not be created");
}

#[test]
#[allow(clippy::too_many_lines)]
fn secret_leak_prevention_on_cli_output() {
    let sentinels = [
        "SENTINEL_SECRET_S3_KEY_CLI",
        "SENTINEL_SECRET_S3_SECRET_CLI",
        "SENTINEL_SECRET_SFTP_KEY_PATH_CLI",
        "SENTINEL_SECRET_DROPBOX_CLIENT_ID_CLI",
        "SENTINEL_SECRET_DROPBOX_CLIENT_SECRET_CLI",
        "SENTINEL_SECRET_DROPBOX_REFRESH_TOKEN_CLI",
        "SENTINEL_SECRET_HOOK_ARG_CLI",
    ];

    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("sentinel.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[docker]
socket = "/var/run/docker.sock"

[[job]]
name = "s3job"
cron = "0 3 * * *"
source = ["/data"]
filename = "s3-%Y%m%dT%H%M%SZ.tar.zst"
  [job.storage]
  type = "s3"
  bucket = "mybucket"
  region = "us-east-1"
  access_key_id = "{}"
  secret_access_key = "{}"
  [[job.pre]]
  cmd = ["pg_dump", "{}"]
  container = "postgres"

[[job]]
name = "sftpjob"
cron = "0 4 * * *"
source = ["/data"]
filename = "sftp-%Y%m%dT%H%M%SZ.tar.zst"
  [job.storage]
  type = "sftp"
  endpoint = "sftp.example.com:22"
  user = "backupuser"
  root = "/remote"
  key_path = "{}"

[[job]]
name = "dropboxjob"
cron = "0 5 * * *"
source = ["/data"]
filename = "dbx-%Y%m%dT%H%M%SZ.tar.zst"
  [job.storage]
  type = "dropbox"
  root = "/backups"
  client_id = "{}"
  client_secret = "{}"
  refresh_token = "{}"
"#,
            sentinels[0],
            sentinels[1],
            sentinels[6],
            sentinels[2],
            sentinels[3],
            sentinels[4],
            sentinels[5],
        ),
    )
    .expect("write sentinel config");

    let invocations: &[&[&str]] = &[
        &["jobs", "--config", config_path.to_str().unwrap()],
        &[
            "jobs",
            "--config",
            config_path.to_str().unwrap(),
            "--remote",
            "--remote-timeout",
            "1",
        ],
        &[
            "jobs",
            "--config",
            config_path.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[
            "jobs",
            "--config",
            config_path.to_str().unwrap(),
            "--format",
            "json",
            "--remote",
            "--remote-timeout",
            "1",
        ],
    ];

    for args in invocations {
        let output = dvb().args(*args).output().expect("execute command");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        for sentinel in &sentinels {
            assert!(
                !stdout.contains(sentinel),
                "secret leak in stdout for args {args:?}: found {sentinel}"
            );
            assert!(
                !stderr.contains(sentinel),
                "secret leak in stderr for args {args:?}: found {sentinel}"
            );
        }
    }
}

#[test]
fn remote_stats_against_fs_backend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage_dir = dir.path().join("storage").join("bk");
    std::fs::create_dir_all(&storage_dir).expect("create storage dir");

    // Write 2 valid backups
    let b1 = storage_dir.join("data-20261001T000000Z.tar.zst");
    std::fs::write(&b1, vec![1; 100]).expect("write b1");

    let b2 = storage_dir.join("data-20261002T000000Z.tar.zst");
    std::fs::write(&b2, vec![2; 200]).expect("write b2");

    // Write sidecar checksum file (should be ignored from count and size)
    let s2 = storage_dir.join("data-20261002T000000Z.tar.zst.sha256");
    std::fs::write(&s2, "checksum").expect("write sidecar");

    // Write unrelated file (should be ignored from count and size)
    let unrelated = storage_dir.join("unrelated.txt");
    std::fs::write(&unrelated, "hello world").expect("write unrelated");

    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[job]]
name = "data"
cron = "0 3 * * *"
source = ["/dummy"]
filename = "data-%Y%m%dT%H%M%SZ.tar.zst"
  [job.storage]
  type = "fs"
  root = "{}"
  prefix = "bk"
"#,
            dir.path().join("storage").display()
        ),
    )
    .expect("write config");

    let assert = dvb()
        .args([
            "jobs",
            "--config",
            config_path.to_str().unwrap(),
            "--remote",
            "--format",
            "json",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
    let parsed: Value = serde_json::from_str(&stdout).expect("valid json");

    let job = &parsed["jobs"][0];
    let remote = &job["remote"];
    assert_eq!(remote["count"], 2);
    assert_eq!(remote["total_bytes"], 300);
    assert_eq!(remote["last_backup"], "2026-10-02T00:00:00+00:00");
    assert!(remote["error"].is_null());
}

#[test]
fn remote_unreachable_backend_populates_error_and_exits_2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let good_storage1 = dir.path().join("storage1");
    let good_storage2 = dir.path().join("storage2");
    std::fs::create_dir_all(&good_storage1).expect("create storage1");
    std::fs::create_dir_all(&good_storage2).expect("create storage2");

    std::fs::write(
        good_storage1.join("j1-20261001T000000Z.tar.zst"),
        vec![0; 50],
    )
    .unwrap();
    std::fs::write(
        good_storage2.join("j3-20261002T000000Z.tar.zst"),
        vec![0; 75],
    )
    .unwrap();

    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[job]]
name = "job1"
cron = "0 1 * * *"
source = ["/dummy"]
filename = "j1-%Y%m%dT%H%M%SZ.tar.zst"
  [job.storage]
  type = "fs"
  root = "{}"

[[job]]
name = "job2_bad"
cron = "0 2 * * *"
source = ["/dummy"]
filename = "j2-%Y%m%dT%H%M%SZ.tar.zst"
  [job.storage]
  type = "s3"
  bucket = "badbucket"
  region = "us-east-1"
  endpoint = "http://192.0.2.1:1"
  access_key_id = "test"
  secret_access_key = "test"

[[job]]
name = "job3"
cron = "0 3 * * *"
source = ["/dummy"]
filename = "j3-%Y%m%dT%H%M%SZ.tar.zst"
  [job.storage]
  type = "fs"
  root = "{}"
"#,
            good_storage1.display(),
            good_storage2.display(),
        ),
    )
    .expect("write config");

    let start = Instant::now();
    let assert = dvb()
        .args([
            "jobs",
            "--config",
            config_path.to_str().unwrap(),
            "--remote",
            "--remote-timeout",
            "1",
            "--format",
            "json",
        ])
        .assert()
        .code(2);

    let duration = start.elapsed();
    // Bounded by timeout (1s) rather than sequential sum
    assert!(duration.as_secs() < 10, "query took too long: {duration:?}");

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
    let parsed: Value = serde_json::from_str(&stdout).expect("valid json");
    let jobs = parsed["jobs"].as_array().expect("jobs array");
    assert_eq!(jobs.len(), 3);

    // Job 1 (good)
    assert_eq!(jobs[0]["name"], "job1");
    assert_eq!(jobs[0]["remote"]["count"], 1);
    assert_eq!(jobs[0]["remote"]["total_bytes"], 50);
    assert!(jobs[0]["remote"]["error"].is_null());

    // Job 2 (bad)
    assert_eq!(jobs[1]["name"], "job2_bad");
    assert!(jobs[1]["remote"]["error"].is_string());
    let err_msg = jobs[1]["remote"]["error"].as_str().unwrap();
    assert!(
        err_msg.contains("s3"),
        "error message should indicate backend: {err_msg}"
    );

    // Job 3 (good)
    assert_eq!(jobs[2]["name"], "job3");
    assert_eq!(jobs[2]["remote"]["count"], 1);
    assert_eq!(jobs[2]["remote"]["total_bytes"], 75);
    assert!(jobs[2]["remote"]["error"].is_null());
}
