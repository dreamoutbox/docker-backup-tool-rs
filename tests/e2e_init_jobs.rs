use std::fs;

use assert_cmd::Command;
use serde_json::Value;
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};

fn dvb() -> Command {
    Command::cargo_bin("dvb").expect("dvb binary is built by cargo test")
}

#[test]
fn e2e_init_edit_jobs_check_backup_remote() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let lock_dir = tmp.path().join("locks");
    fs::create_dir_all(&lock_dir).expect("create lock dir");

    let config_path = tmp.path().join("dvb.toml");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");
    fs::write(data_dir.join("sample.txt"), b"e2e test payload").expect("write sample file");

    let storage_dir = tmp.path().join("storage");
    fs::create_dir_all(&storage_dir).expect("create storage dir");

    // 1. dvb init -o <config_path>
    dvb()
        .args(["init", "-o", config_path.to_str().unwrap()])
        .assert()
        .success();

    assert!(config_path.exists());

    // 2. Modify config with toml_edit
    let raw_toml = fs::read_to_string(&config_path).expect("read generated config");
    let mut doc: DocumentMut = raw_toml.parse().expect("parse toml with toml_edit");

    // Update job source. The reference template declares `[job.backup]`.
    let job = doc["job"].get_mut("backup").expect("backup job");
    let mut sources = Array::new();
    sources.push(data_dir.to_str().unwrap());
    job["source"] = Item::Value(TomlValue::Array(sources));

    // Replace job storage with the fs backend (`[job.backup.storage]`).
    let mut storage_tbl = Table::new();
    storage_tbl.insert("type", Item::Value("fs".into()));
    storage_tbl.insert("root", Item::Value(storage_dir.to_str().unwrap().into()));
    job["storage"] = Item::Table(storage_tbl);

    fs::write(&config_path, doc.to_string()).expect("write updated config");

    // 3. dvb jobs lists the job
    let jobs_out = dvb()
        .args(["jobs", "--config", config_path.to_str().unwrap()])
        .env("DVB_LOCK_DIR", &lock_dir)
        .assert()
        .success();
    let stdout = String::from_utf8(jobs_out.get_output().stdout.clone()).expect("utf-8 stdout");
    assert!(
        stdout.contains("backup"),
        "jobs table should contain job name 'backup': {stdout}"
    );
    assert!(
        stdout.contains("file://"),
        "jobs table should contain file:// storage: {stdout}"
    );

    // 4. dvb check passes
    dvb()
        .args(["check", "--config", config_path.to_str().unwrap()])
        .env("DVB_LOCK_DIR", &lock_dir)
        .assert()
        .success();

    // 5. dvb backup backup succeeds
    dvb()
        .args([
            "backup",
            "backup",
            "--config",
            config_path.to_str().unwrap(),
        ])
        .env("DVB_LOCK_DIR", &lock_dir)
        .assert()
        .success();

    // 6. dvb jobs --remote --format json shows remote.count == 1
    let remote_out = dvb()
        .args([
            "jobs",
            "--config",
            config_path.to_str().unwrap(),
            "--remote",
            "--format",
            "json",
        ])
        .env("DVB_LOCK_DIR", &lock_dir)
        .assert()
        .success();

    let remote_stdout =
        String::from_utf8(remote_out.get_output().stdout.clone()).expect("utf-8 stdout");
    let parsed: Value = serde_json::from_str(&remote_stdout).expect("valid json");
    let jobs = parsed["jobs"].as_array().expect("jobs array");
    assert_eq!(jobs.len(), 1);
    let job_summary = &jobs[0];
    assert_eq!(job_summary["name"], "backup");
    let remote = &job_summary["remote"];
    assert_eq!(remote["count"], 1);
    assert!(remote["total_bytes"].as_u64().unwrap_or(0) > 0);
    assert!(!remote["last_backup"].is_null());
    assert!(remote["error"].is_null());
}
