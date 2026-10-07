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
[job.db]
source = ["{}"]
filename = "db-%Y%m%dT%H%M%SZ"
  [job.db.storage]
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
        .env("DVB__JOB__DB__CRONTEXT", "every 15 minutes")
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

#[test]
fn init_stdout_writes_pure_toml_and_stderr_has_no_config() {
    let assert = dvb().args(["init", "-o", "-"]).assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf-8 stderr");

    assert_ne!(stdout, "");
    assert!(stdout.contains("[job.backup]"));
    // Stdout must parse as valid TOML
    let parsed: toml::Value = toml::from_str(&stdout).expect("stdout must be valid TOML");
    assert!(parsed.get("job").is_some());

    // Stderr must not contain config data
    assert!(!stderr.contains("[job.backup]"));
}

#[test]
fn init_file_creation_overwrite_protection_and_force() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out_path = dir.path().join("sub").join("dvb.toml");

    // 1. Initial write creates file and parent dirs
    let assert = dvb()
        .args(["init", "-o", out_path.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf-8 stderr");
    assert!(stdout.is_empty(), "stdout must be empty on file write");
    assert!(stderr.contains("Configuration written to"));
    assert!(stderr.contains("Next steps:"));
    assert!(out_path.exists());

    let original_content = std::fs::read_to_string(&out_path).expect("read file");
    assert!(original_content.contains("[job.backup]"));

    // Modify file
    std::fs::write(&out_path, "modified = true\n").expect("modify file");

    // 2. Overwrite without force fails and leaves file untouched
    let fail_assert = dvb()
        .args(["init", "-o", out_path.to_str().unwrap()])
        .assert()
        .failure()
        .code(1);
    let fail_stderr = String::from_utf8_lossy(&fail_assert.get_output().stderr);
    assert!(fail_stderr.contains("--force"));
    assert_eq!(
        std::fs::read_to_string(&out_path).unwrap(),
        "modified = true\n"
    );

    // 3. Overwrite with --force succeeds and restores template
    let force_assert = dvb()
        .args(["init", "-o", out_path.to_str().unwrap(), "--force"])
        .assert()
        .success();
    let force_stderr = String::from_utf8_lossy(&force_assert.get_output().stderr);
    assert!(force_stderr.contains("Configuration written to"));
    assert_eq!(
        std::fs::read_to_string(&out_path).unwrap(),
        original_content
    );
}
