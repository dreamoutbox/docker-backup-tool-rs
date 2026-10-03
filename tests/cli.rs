use assert_cmd::Command;

fn dvb() -> Command {
    Command::cargo_bin("dvb").expect("dvb binary is built by cargo test")
}

#[test]
fn help_lists_every_subcommand() {
    let output = dvb().arg("--help").assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).expect("utf-8 stdout");
    for subcommand in [
        "run", "backup", "prune", "list", "check", "restore", "crontext", "init", "jobs",
    ] {
        assert!(
            stdout.contains(subcommand),
            "`{subcommand}` missing from help output:\n{stdout}"
        );
    }
}

#[test]
fn version_is_reported() {
    dvb()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn global_flags_accept_the_documented_env_vars() {
    dvb()
        .args(["backup", "db", "--config", "/tmp/does-not-exist.toml"])
        .env("DVB_LOG_FORMAT", "json")
        .assert()
        .failure()
        .code(1);
}

#[test]
fn run_without_a_config_file_fails() {
    dvb().arg("run").assert().failure().code(1);
}

#[test]
fn backup_without_a_config_file_fails() {
    dvb()
        .args(["backup", "db"])
        .env("DVB_CONFIG", "/nonexistent/dvb.toml")
        .assert()
        .failure()
        .code(1);
}

#[test]
fn unknown_subcommand_is_rejected() {
    dvb().arg("frobnicate").assert().failure().code(2);
}

#[test]
fn env_override_crontext() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let storage_dir = dir.path().join("storage");
    std::fs::create_dir_all(&storage_dir).unwrap();

    std::fs::write(
        &config_path,
        format!(
            r#"
[[job]]
name = "db"
source = ["{}"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.storage]
  type = "fs"
  root = "{}"
"#,
            data_dir.display(),
            storage_dir.display(),
        ),
    )
    .unwrap();

    let assert = dvb()
        .args(["check", "--config", config_path.to_str().unwrap()])
        .env("DVB__JOB__0__CRONTEXT", "every 15 minutes")
        .assert();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    println!("STDOUT:\n{stdout}\nSTDERR:\n{stderr}");
    assert.success();
}

#[test]
fn crontext_cli_evaluates_schedule() {
    let output = dvb()
        .args(["crontext", "every friday at 18:00", "--timezone", "UTC"])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).expect("utf-8 stdout");
    assert!(stdout.contains("0 18 * * 5"));
    assert!(stdout.contains("every friday at 18:00"));
    assert!(stdout.contains("timezone:    UTC"));
    assert!(stdout.contains("next 5 fire times:"));
}

#[test]
fn init_help_lists_all_flags() {
    let output = dvb().args(["init", "--help"]).assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).expect("utf-8 stdout");
    assert!(stdout.contains("--output") && stdout.contains("-o"));
    assert!(stdout.contains("--force"));
}

#[test]
fn jobs_help_lists_all_flags() {
    let output = dvb().args(["jobs", "--help"]).assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).expect("utf-8 stdout");
    assert!(stdout.contains("--config"));
    assert!(stdout.contains("--format"));
    assert!(stdout.contains("--remote"));
    assert!(stdout.contains("--remote-timeout"));
}
