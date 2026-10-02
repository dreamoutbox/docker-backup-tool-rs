//! Black box tests for the `dvb` command line surface.

use assert_cmd::Command;

fn dvb() -> Command {
    Command::cargo_bin("dvb").expect("dvb binary is built by cargo test")
}

#[test]
fn help_lists_every_subcommand() {
    let output = dvb().arg("--help").assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).expect("utf-8 stdout");
    for subcommand in ["run", "backup", "prune", "list", "check"] {
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
fn not_yet_implemented_subcommands_exit_with_failure() {
    for args in [
        vec!["run"],
        vec!["prune", "db"],
        vec!["list", "db"],
        vec!["check"],
    ] {
        dvb()
            .args(args)
            .assert()
            .failure()
            .code(1)
            .stderr(predicates::str::contains("not implemented yet"));
    }
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
