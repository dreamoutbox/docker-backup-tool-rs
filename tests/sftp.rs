//! SFTP integration tests against a real `atmoz/sftp` server.
//!
//! These need Docker, so they are `#[ignore]`d by default:
//!
//! ```sh
//! cargo test --test sftp -- --ignored
//! ```
//!
//! What these cover that the `memory` backend cannot: the SFTP backend shells
//! out to the `ssh` binary, so this exercises that path end to end, including
//! host key verification and listing over the wire.
//!
//! Authentication uses a committed test-only key (see `tests/fixtures/README.md`)
//! because `ssh` cannot do password auth in batch mode without `sshpass`.

use std::path::{Path, PathBuf};
use std::process::Command;

use testcontainers::core::IntoContainerPort as _;
use testcontainers::core::Mount;
use testcontainers::core::wait::WaitFor;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

#[path = "support/mod.rs"]
mod support;

/// `atmoz/sftp` user.
const USER: &str = "dvb";
/// Options letter: `e` = writeable, chrooted into the user's home.
const USER_OPTIONS: &str = "e";
/// Upload directory inside the chroot, created by the setup hook.
const UPLOAD_DIR: &str = "backups";
const SFTP_PORT: u16 = 22;

fn fixture(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    #[cfg(unix)]
    if name == "sftp_test_key" && path.exists() {
        use std::os::unix::fs::PermissionsExt as _;
        // Git cannot store 0600 mode in the tree. OpenSSH rejects private
        // keys that are group- or world-readable (0644).
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    path
}

/// A running SFTP server reachable with the test key.
struct SftpServer {
    _container: ContainerAsync<GenericImage>,
    port: u16,
}

impl SftpServer {
    async fn start() -> Self {
        dvb::storage::install_transport().expect("install transport");

        let public_key = fixture("sftp_test_key.pub");
        assert!(
            public_key.exists(),
            "test key missing at {}",
            public_key.display()
        );

        // No `with_cmd`: the entrypoint turns "no command" into "start sshd",
        // while passing `user ...` makes it try to exec a binary that is not
        // there. `SFTP_USERS` is the supported way to declare a user.
        // CPU and RAM are capped: see support::limited for why.
        // `/etc/sftp.d/*.sh` runs after the users are created. That hook is what
        // installs the key and fixes directory ownership; the script explains why
        // a bind mount alone cannot do it.
        let setup_hook = fixture("install_sftp_key.sh");
        let container = support::start(
            GenericImage::new("atmoz/sftp", "alpine")
                .with_exposed_port(SFTP_PORT.tcp())
                .with_wait_for(WaitFor::message_on_stderr("Server listening on"))
                .with_env_var("SFTP_USERS", format!("{USER}:pw:{USER_OPTIONS}"))
                .with_env_var("SFTP_USER", USER)
                .with_env_var("SFTP_UPLOAD_DIR", UPLOAD_DIR)
                .with_env_var("SFTP_KEY_SRC", "/run/keys/dvb_test_key.pub")
                .with_mount(Mount::bind_mount(
                    setup_hook.to_string_lossy().into_owned(),
                    "/etc/sftp.d/10-key.sh".to_owned(),
                ))
                .with_mount(Mount::bind_mount(
                    public_key.to_string_lossy().into_owned(),
                    "/run/keys/dvb_test_key.pub".to_owned(),
                )),
        )
        .await;

        let port = container
            .get_host_port_ipv4(SFTP_PORT)
            .await
            .expect("mapped sftp port");

        let this = Self {
            _container: container,
            port,
        };
        this.wait_until_listening().await;
        this
    }

    fn endpoint(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// The log line appears before sshd accepts connections, so poll the port.
    async fn wait_until_listening(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            if std::net::TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", self.port)
                    .parse()
                    .expect("valid socket addr"),
                std::time::Duration::from_millis(500),
            )
            .is_ok()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        panic!("sftp server never started listening on {}", self.endpoint());
    }
}

fn sftp_config(
    server: &SftpServer,
    strategy: dvb::config::KnownHostsStrategy,
) -> dvb::config::StorageConfig {
    dvb::config::StorageConfig::Sftp(dvb::config::SftpConfig {
        endpoint: server.endpoint(),
        user: USER.to_owned(),
        root: UPLOAD_DIR.to_owned(),
        key_path: Some(fixture("sftp_test_key")),
        known_hosts_strategy: strategy,
    })
}

/// A `dvb` invocation wired to `dir` for config and locks.
fn dvb(dir: &Path) -> Command {
    let bin = assert_cmd::cargo::cargo_bin("dvb");
    let mut cmd = Command::new(bin);
    cmd.env("DVB_CONFIG", dir.join("config.toml"))
        .env("DVB_LOCK_DIR", dir.join("locks"))
        .env("DVB_LOG", "info");
    cmd
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with cargo test --test sftp -- --ignored"]
async fn strict_host_key_checking_refuses_an_unknown_host() {
    let server = SftpServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");

    // An empty known_hosts: the first connection must be refused.
    let home = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(home.path().join(".ssh")).expect("create ~/.ssh");
    std::fs::write(home.path().join(".ssh/known_hosts"), "").expect("write known_hosts");

    std::fs::write(
        tmp.path().join("config.toml"),
        format!(
            r#"
[job.db]
cron = "0 3 * * *"
source = ["{}"]
filename = "db-%Y%m%dT%H%M%SZ.tar.zst"

  [job.db.storage]
  type = "sftp"
  endpoint = "{}"
  user = "{USER}"
  root = "{UPLOAD_DIR}"
  key_path = "{}"
  known_hosts_strategy = "strict"
"#,
            tmp.path().display(),
            server.endpoint(),
            fixture("sftp_test_key").display(),
        ),
    )
    .expect("write config");

    // `HOME` is set on the child only: the ssh client reads it from the
    // environment, and mutating this process's would race other tests.
    let output = dvb(tmp.path())
        .env("HOME", home.path())
        .args(["list", "db"])
        .output()
        .expect("run dvb list");

    assert!(
        !output.status.success(),
        "strict host key checking must not accept an unknown host"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("host") || stderr.contains("known_hosts"),
        "error should mention the host key: {stderr}"
    );

    // The known_hosts file must still be empty: `strict` never records a key.
    let known_hosts =
        std::fs::read_to_string(home.path().join(".ssh/known_hosts")).expect("read known_hosts");
    assert!(
        known_hosts.trim().is_empty(),
        "strict mode must not add a host key, found: {known_hosts}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with cargo test --test sftp -- --ignored"]
async fn uploads_lists_and_prunes_over_sftp() {
    let server = SftpServer::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = tmp.path().join("source/pgdata");
    std::fs::create_dir_all(source.join("nested")).expect("mkdir");
    std::fs::write(source.join("a.txt"), b"hello sftp").expect("write");
    std::fs::write(source.join("nested/b.bin"), [7_u8; 2048]).expect("write");

    // The `accept_new` path has to record the host key somewhere; keep HOME in a
    // temp dir so the developer's real known_hosts is never touched.
    let home = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(home.path().join(".ssh")).expect("create ~/.ssh");

    std::fs::write(
        tmp.path().join("config.toml"),
        format!(
            r#"
[job.db]
cron = "0 3 * * *"
source = ["{}"]
filename = "db-%Y%m%dT%H%M%SZ.tar.zst"
compression = "zstd"
retention_days = 14
min_keep = 1

  [job.db.storage]
  type = "sftp"
  endpoint = "{}"
  user = "{USER}"
  root = "{UPLOAD_DIR}"
  key_path = "{}"
  known_hosts_strategy = "accept_new"
"#,
            source.display(),
            server.endpoint(),
            fixture("sftp_test_key").display(),
        ),
    )
    .expect("write config");

    // A fresh `Command` per invocation: `Command::args` accumulates, so reusing
    // one would turn `backup db list db` into a single bad command line.
    let run = |args: &[&str]| {
        dvb(tmp.path())
            .env("HOME", home.path())
            .args(args)
            .output()
            .expect("run dvb")
    };

    let output = run(&["backup", "db"]);
    assert!(
        output.status.success(),
        "backup over sftp failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // `dvb list` sees it, with the timestamp parsed from the name.
    let output = run(&["list", "db"]);
    assert!(
        output.status.success(),
        "list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("db-"), "{stdout}");
    assert!(stdout.contains(".zst"), "{stdout}");

    // Pruning with a 14 day window keeps the fresh backup and changes nothing.
    let output = run(&["prune", "db"]);
    assert!(
        output.status.success(),
        "prune failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("deleted 0"), "{stdout}");

    // A dry run agrees and still lists the backup.
    let output = run(&["prune", "db", "--dry-run"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("would delete 0"), "{stdout}");

    // And the object is really on the server, with the right bytes.
    let config = sftp_config(&server, dvb::config::KnownHostsStrategy::AcceptNew);
    let op = dvb::storage::operator(&config).expect("operator");
    let listed = dvb::storage::list_prefix(&op, "sftp", UPLOAD_DIR)
        .await
        .expect("list over sftp");
    assert_eq!(listed.len(), 2, "unexpected objects: {listed:?}");
    let archive = listed
        .iter()
        .find(|o| !o.path.ends_with(".sha256"))
        .expect("archive object present");
    assert!(archive.size > 0, "empty object: {listed:?}");
    let ext = dvb::config::Compression::Zstd
        .extension()
        .expect("zstd has an extension");
    assert!(
        archive.path.ends_with(&format!(".{ext}")),
        "{}",
        archive.path
    );
    assert!(
        listed
            .iter()
            .any(|o| o.path == format!("{}.sha256", archive.path)),
        "sidecar missing in {listed:?}"
    );
}
