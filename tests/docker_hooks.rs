//! Docker stop/restart and hook integration tests.
//!
//! These need Docker and a real container, so they are `#[ignore]`d by default:
//!
//! ```sh
//! scripts/run-tests.sh --all
//! # or: cargo test --test docker_hooks -- --ignored
//! ```
//!
//! What they cover that the unit tests cannot: that a container is actually
//! stopped before the archive is taken and restarted afterwards, that a run
//! which fails or is interrupted still restarts it, and that container hooks
//! reach the daemon.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use testcontainers::core::wait::WaitFor;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

#[path = "support/mod.rs"]
mod support;

/// Name of the job in every fixture config.
const JOB: &str = "db";

/// A `dvb` run with its config, lock dir and storage root in a temp dir.
struct Fixture {
    config: PathBuf,
    source: PathBuf,
    remote: PathBuf,
    tmp: tempfile::TempDir,
}

impl Fixture {
    /// A config whose job stops the containers named in `stop_toml`, which is
    /// the literal `stop_containers = [...]` / `stop_label = ...` snippet.
    fn new(stop_toml: &str) -> Self {
        Self::with_extra(stop_toml, "")
    }

    /// [`Fixture::new`] plus an extra TOML snippet (hooks, compression) for the
    /// job.
    fn with_extra(stop_toml: &str, extra: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let source = tmp.path().join("source");
        let remote = tmp.path().join("remote");
        std::fs::create_dir_all(&source).expect("create source");
        std::fs::create_dir_all(&remote).expect("create remote");

        let config = tmp.path().join("config.toml");
        let body = format!(
            r#"
[docker]
socket = "{}"

[[job]]
name = "{JOB}"
source = [{}]
filename = "db-%Y%m%dT%H%M%SZ.tar.zst"
retention_days = 14
min_keep = 3
{stop_toml}
{extra}
  [job.storage]
  type = "fs"
  root = {}
  prefix = "backups"
"#,
            socket_path().display(),
            toml_path(&source),
            toml_path(&remote),
        );
        std::fs::write(&config, body).expect("write config");

        Self {
            config,
            source,
            remote,
            tmp,
        }
    }

    fn dvb(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("dvb"));
        cmd.env("DVB_CONFIG", &self.config)
            .env(
                "DVB_LOCK_DIR",
                self.config.parent().expect("parent").join("locks"),
            )
            .env("DVB_LOG", "warn");
        cmd
    }

    fn backup(&self) -> std::process::Output {
        self.dvb()
            .args(["backup", JOB])
            .output()
            .expect("run dvb backup")
    }

    fn path(&self) -> &Path {
        self.tmp.path()
    }

    /// Start `dvb backup` with its log going to a file, so a test can read the
    /// reason a run stopped making progress while it is still going.
    fn backup_to_log(&self) -> (std::process::Child, PathBuf) {
        let log = self.path().join("dvb.log");
        let file = std::fs::File::create(&log).expect("create log");
        let child = self
            .dvb()
            // Info, not warn: the assertion needs to see which stage the run
            // reached before it stalled.
            .env("DVB_LOG", "info")
            .args(["backup", JOB])
            .stdout(Stdio::null())
            .stderr(Stdio::from(file))
            .spawn()
            .expect("spawn dvb");
        (child, log)
    }

    /// Standard error of a finished run, for assertion messages.
    fn stderr_of(output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// Names of the objects currently in storage.
    fn stored(&self) -> Vec<String> {
        let dir = self.remote.join("backups");
        if !dir.exists() {
            return Vec::new();
        }
        std::fs::read_dir(dir)
            .expect("read remote dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// Fill `<source>/<name>` with `mib` of random bytes, so archiving takes
    /// long enough for a signal to land inside it, or enough data is already
    /// in storage when a later entry makes the run fail.
    fn write_payload(&self, name: &str, mib: u64) {
        use std::io::Read as _;

        let urandom = std::fs::File::open("/dev/urandom").expect("open /dev/urandom");
        let mut file = std::fs::File::create(self.source.join(name)).expect("create payload");
        std::io::copy(&mut urandom.take(mib * 1024 * 1024), &mut file).expect("write payload");
    }
}

/// The socket `dvb` in the fixture should use, following `DOCKER_HOST`.
fn socket_path() -> PathBuf {
    std::env::var("DOCKER_HOST")
        .ok()
        .and_then(|host| host.strip_prefix("unix://").map(str::to_owned))
        .map_or_else(|| PathBuf::from("/var/run/docker.sock"), PathBuf::from)
}

fn toml_path(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

/// A container that reports readiness and then sleeps for an hour.
///
/// The trap is what makes it stoppable: as PID 1 a process's default signal
/// dispositions are ignored, so a bare `sleep` never exits on SIGTERM and
/// `docker stop` always waits out its full timeout before SIGKILL.
async fn sleeper() -> ContainerAsync<GenericImage> {
    support::start(
        GenericImage::new("alpine", "3.20")
            .with_wait_for(WaitFor::message_on_stdout("ready"))
            .with_cmd([
                "sh",
                "-c",
                "trap 'exit 0' TERM INT; echo ready; sleep 3600 & wait",
            ]),
    )
    .await
}

/// Whether the daemon currently has `id` running.
async fn is_running(id: &str) -> bool {
    found(id).await.expect("container exists").running
}

/// The daemon's view of `id`.
async fn found(id: &str) -> Option<dvb::docker::Container> {
    let client = dvb::docker::Client::connect_default().expect("connect to docker");
    client
        .resolve(&[id.to_owned()], None)
        .await
        .expect("resolve container")
        .into_iter()
        .next()
}

/// `State.StartedAt`, which changes whenever the container is restarted.
///
/// This is what proves a stop and start actually happened; "it is running
/// afterwards" is also true when nothing was ever done to it.
async fn started_at(id: &str) -> String {
    let docker =
        testcontainers::bollard::Docker::connect_with_unix_defaults().expect("connect to docker");
    let info = docker
        .inspect_container(id, None)
        .await
        .expect("inspect container");
    info.state
        .and_then(|state| state.started_at)
        .expect("state.started_at")
}

/// Poll until `id` is stopped.
///
/// If `dvb` exits first there was nothing to interrupt, and if it neither
/// exits nor stops the container it is stuck; either way panic with its own
/// log rather than a bare "never stopped".
async fn wait_until_stopped(id: &str, child: &mut std::process::Child, log: &Path) {
    for _ in 0..600 {
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!(
                "dvb exited ({status}) before it stopped the container: {}",
                read_log(log)
            );
        }
        if !is_running(id).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "the container was never stopped, so the backup never got that far: {}",
        read_log(log)
    );
}

fn read_log(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_else(|err| format!("<no log: {err}>"))
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_running_container_is_stopped_then_restarted() {
    let container = sleeper().await;
    let id = container.id();
    let before = started_at(id).await;
    let fixture = Fixture::new(&format!("stop_containers = [\"{id}\"]"));

    let out = fixture.backup();
    assert!(
        out.status.success(),
        "backup failed: {}",
        Fixture::stderr_of(&out)
    );

    assert!(
        is_running(id).await,
        "the container was not restarted after the backup"
    );
    assert_ne!(
        started_at(id).await,
        before,
        "the container was left exactly as it was: it was never stopped"
    );
    assert_eq!(fixture.stored().len(), 1, "expected exactly one archive");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn an_already_stopped_container_is_left_stopped() {
    let container = sleeper().await;
    let id = container.id().to_owned();
    container.stop_with_timeout(None).await.expect("stop");
    let before = started_at(&id).await;

    let fixture = Fixture::new(&format!("stop_containers = [\"{id}\"]"));
    let out = fixture.backup();
    assert!(
        out.status.success(),
        "backup failed: {}",
        Fixture::stderr_of(&out)
    );

    assert!(
        !is_running(&id).await,
        "a container dvb did not start was started anyway"
    );
    assert_eq!(
        started_at(&id).await,
        before,
        "the container was restarted even though it was already stopped"
    );
    assert_eq!(fixture.stored().len(), 1, "expected exactly one archive");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_failed_upload_still_restarts_the_containers() {
    let container = sleeper().await;
    let id = container.id().to_owned();
    let before = started_at(&id).await;
    let fixture = Fixture::with_extra(
        &format!("stop_containers = [\"{id}\"]"),
        "follow_symlinks = true",
    );

    // Entries are walked in sorted order, so the payload below is already in
    // storage when the dangling link is read and the run fails part way
    // through. That is the failure that must not leave containers stopped.
    fixture.write_payload("a-payload.bin", 8);
    std::os::unix::fs::symlink("missing-target", fixture.source.join("z-dangling"))
        .expect("dangling symlink");

    let out = fixture.backup();
    assert!(
        !out.status.success(),
        "a backup over a dangling symlink reported success"
    );
    assert_eq!(out.status.code(), Some(1), "unexpected exit code");
    assert!(
        is_running(&id).await,
        "the container was left stopped after a failed upload"
    );
    assert_ne!(
        started_at(&id).await,
        before,
        "the container was never stopped, so nothing was restored"
    );
    assert!(
        fixture.stored().is_empty(),
        "a partial object was left behind: {:?}",
        fixture.stored()
    );
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_failing_container_hook_aborts_before_anything_is_stopped() {
    let container = sleeper().await;
    let id = container.id().to_owned();
    let before = started_at(&id).await;
    let fixture = Fixture::new(&format!("stop_containers = [\"{id}\"]"));
    // A pre hook runs before the containers stop, so a failure here must leave
    // everything exactly as it was.
    let body = std::fs::read_to_string(&fixture.config).expect("read config");
    let body = body.replace(
        &format!("stop_containers = [\"{id}\"]"),
        &format!(
            "stop_containers = [\"{id}\"]\n  [[job.pre]]\n  cmd = [\"sh\", \"-c\", \"exit 4\"]\n  container = \"{id}\""
        ),
    );
    std::fs::write(&fixture.config, body).expect("write config");

    let out = fixture.backup();
    assert!(
        !out.status.success(),
        "a failing pre hook reported success: {}",
        Fixture::stderr_of(&out)
    );
    assert!(
        is_running(&id).await,
        "the container was stopped even though the pre hook failed"
    );
    assert_eq!(
        started_at(&id).await,
        before,
        "the containers were stopped before the pre hooks had passed"
    );
    assert!(
        fixture.stored().is_empty(),
        "an archive was written despite the failed pre hook: {:?}",
        fixture.stored()
    );
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_container_hook_writes_through_the_daemon() {
    let container = sleeper().await;
    let id = container.id().to_owned();
    let before = started_at(&id).await;
    let fixture = Fixture::new(&format!("stop_containers = [\"{id}\"]"));
    // The hook runs before the containers stop, so its marker survives.
    let body = std::fs::read_to_string(&fixture.config).expect("read config");
    let body = body.replace(
        &format!("stop_containers = [\"{id}\"]"),
        &format!(
            "stop_containers = [\"{id}\"]\n  [[job.pre]]\n  cmd = [\"sh\", \"-c\", \"touch /tmp/dvb-hook\"]\n  container = \"{id}\""
        ),
    );
    std::fs::write(&fixture.config, body).expect("write config");

    let out = fixture.backup();
    assert!(
        out.status.success(),
        "backup failed: {}",
        Fixture::stderr_of(&out)
    );

    let client = dvb::docker::Client::connect_default().expect("connect to docker");
    let probe = [
        "test".to_owned(),
        "-f".to_owned(),
        "/tmp/dvb-hook".to_owned(),
    ];
    let code = client
        .exec(&id, &probe, &[], Duration::from_secs(30))
        .await
        .expect("exec probe");
    assert_eq!(code, 0, "the pre hook did not create its marker");
    assert!(is_running(&id).await, "the container was not restarted");
    assert_ne!(
        started_at(&id).await,
        before,
        "the container was never stopped around the archive"
    );
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn sigterm_restarts_the_containers_and_leaves_no_partial_object() {
    let container = sleeper().await;
    let id = container.id().to_owned();
    let before = started_at(&id).await;

    // gzip of random data is slow enough that the signal lands mid-upload.
    let fixture = Fixture::with_extra(
        &format!("stop_containers = [\"{id}\"]"),
        "compression = \"gzip\"",
    );
    fixture.write_payload("payload.bin", 64);

    let (mut child, log) = fixture.backup_to_log();

    // Signal once the run has stopped the container: at that point the archive
    // is in flight and the object in storage is still partial.
    wait_until_stopped(&id, &mut child, &log).await;
    let signalled = terminate(child.id()).expect("send SIGTERM");
    assert!(
        signalled.status.success(),
        "kill failed: {}",
        String::from_utf8_lossy(&signalled.stderr)
    );

    let status = child.wait().expect("wait for dvb");
    let text = read_log(&log);
    assert!(!status.success(), "dvb exited 0 after being interrupted");
    assert!(
        is_running(&id).await,
        "the container was left stopped after SIGTERM"
    );
    assert_ne!(
        started_at(&id).await,
        before,
        "the container was never restarted after the interruption"
    );
    assert!(
        fixture.stored().is_empty(),
        "a partial object survived the interruption: {:?}",
        fixture.stored()
    );
    assert!(
        text.contains("shutdown requested"),
        "the process did not take the graceful path, so it was killed outright:\n{text}"
    );
}

/// Send SIGTERM to `pid`, using the system `kill` rather than a libc binding.
fn terminate(pid: u32) -> std::io::Result<std::process::Output> {
    std::process::Command::new("/usr/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .output()
}
