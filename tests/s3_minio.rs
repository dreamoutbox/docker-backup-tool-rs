//! S3 integration tests against a real `SeaweedFS` S3 gateway.
//!
//! These need Docker, so they are `#[ignore]`d by default:
//!
//! ```sh
//! cargo test --test s3_minio -- --ignored
//! ```
//!
//! (`MinIO` is gone from Docker Hub, so this runs `SeaweedFS`'s S3 gateway
//! instead; the S3 surface `OpenDAL` exercises is the same.)
//!
//! What these cover that `memory` and `fs` cannot: multipart upload through the
//! chunked writer, and proof that a >50 MiB archive arrives byte-identical.

use std::process::Command;

use testcontainers::core::IntoContainerPort as _;
use testcontainers::core::wait::WaitFor;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

#[path = "support/mod.rs"]
mod support;

/// Credentials `SeaweedFS` accepts for anonymous-style access in tests.
const ACCESS_KEY: &str = "dvbtest";
const SECRET_KEY: &str = "dvbtest-secret";
const BUCKET: &str = "dvb";
/// `SeaweedFS`'s S3 gateway port.
const S3_PORT: u16 = 8333;

/// A running `SeaweedFS` with an S3 gateway.
struct SeaweedS3 {
    _container: ContainerAsync<GenericImage>,
    endpoint: String,
}

impl SeaweedS3 {
    /// Start the gateway and wait until it answers.
    ///
    /// `SeaweedFS` needs the `server` command with `-s3`: running `weed s3`
    /// alone answers no requests because there is no filer behind it.
    async fn start() -> Self {
        // The test process builds operators directly instead of going through
        // `main`, so it has to install the HTTP transport itself.
        dvb::storage::install_transport().expect("install transport");

        // CPU and RAM are capped: see support::limited for why.
        let container = support::start(
            GenericImage::new("chrislusf/seaweedfs", "latest")
                .with_exposed_port(S3_PORT.tcp())
                .with_wait_for(WaitFor::message_on_stderr("Start"))
                .with_env_var("AWS_ACCESS_KEY_ID", ACCESS_KEY)
                .with_env_var("AWS_SECRET_ACCESS_KEY", SECRET_KEY)
                .with_cmd(["server", "-s3", &format!("-s3.port={S3_PORT}")]),
        )
        .await;

        let port = container
            .get_host_port_ipv4(S3_PORT)
            .await
            .expect("mapped s3 port");
        let endpoint = format!("http://127.0.0.1:{port}");

        let this = Self {
            _container: container,
            endpoint,
        };
        this.wait_until_ready().await;
        this
    }

    /// Poll `GET /` until the gateway stops resetting the connection.
    ///
    /// The log line appears before the listener is actually accepting, so this
    /// is what makes the tests reliable rather than timing-dependent.
    async fn wait_until_ready(&self) {
        let client = reqwest::Client::new();
        for _ in 0..60 {
            if client
                .get(&self.endpoint)
                .timeout(std::time::Duration::from_secs(2))
                .send()
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        panic!(
            "seaweedfs s3 gateway never became ready at {}",
            self.endpoint
        );
    }

    /// Write a config file pointing at this gateway and return its path.
    fn write_config(&self, dir: &std::path::Path, source: &std::path::Path, prefix: &str) {
        std::fs::write(
            dir.join("config.toml"),
            format!(
                r#"
[[job]]
name = "db"
source = ["{}"]
filename = "db-%Y%m%dT%H%M%SZ.tar.zst"
compression = "zstd"
retention_days = 14
min_keep = 2

  [job.storage]
  type = "s3"
  bucket = "{BUCKET}"
  region = "us-east-1"
  endpoint = "{}"
  prefix = "{prefix}"
  access_key_id = "{ACCESS_KEY}"
  secret_access_key = "{SECRET_KEY}"
  force_path_style = true
"#,
                source.display(),
                self.endpoint,
            ),
        )
        .expect("write config");
    }

    fn operator(&self) -> opendal::Operator {
        dvb::storage::operator(&s3_config(&self.endpoint)).expect("build s3 operator from config")
    }
}

fn s3_config(endpoint: &str) -> dvb::config::StorageConfig {
    dvb::config::StorageConfig::S3(dvb::config::S3Config {
        bucket: BUCKET.to_owned(),
        region: "us-east-1".to_owned(),
        endpoint: Some(endpoint.to_owned()),
        prefix: String::new(),
        access_key_id: Some(dvb::config::SecretString::new(ACCESS_KEY)),
        secret_access_key: Some(dvb::config::SecretString::new(SECRET_KEY)),
        force_path_style: true,
    })
}

/// `len` bytes of xorshift output, deterministic in `seed`.
///
/// Incompressible enough for zstd that 60 MiB stays above 50 MiB, which is the
/// point: the test needs a real multi-part upload, not a compressed single part.
fn pseudo_random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        // xorshift64*
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        out.push((state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33).to_le_bytes()[0]);
    }
    out
}

/// A `dvb` invocation wired to `dir` for config and locks.
fn dvb(dir: &std::path::Path) -> Command {
    let bin = assert_cmd::cargo::cargo_bin("dvb");
    let mut cmd = Command::new(bin);
    cmd.env("DVB_CONFIG", dir.join("config.toml"))
        .env("DVB_LOCK_DIR", dir.join("locks"))
        .env("DVB_LOG", "warn");
    cmd
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with cargo test --test s3_minio -- --ignored"]
async fn uploads_an_archive_and_lists_it() {
    let seaweed = SeaweedS3::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = tmp.path().join("source/pgdata");
    std::fs::create_dir_all(source.join("nested")).expect("mkdir");
    std::fs::write(source.join("a.txt"), b"hello s3").expect("write");
    std::fs::write(source.join("nested/b.bin"), [1_u8; 4096]).expect("write");

    seaweed.write_config(tmp.path(), &source, "db");
    let output = dvb(tmp.path())
        .args(["backup", "db"])
        .output()
        .expect("run dvb backup");
    assert!(
        output.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The object really landed in the bucket, at the right key.
    let client = seaweed.operator();
    let listed = client
        .list_with("db")
        .recursive(true)
        .await
        .expect("list bucket");
    let keys: Vec<String> = listed
        .into_iter()
        .map(|entry| entry.path().to_owned())
        .collect();
    assert_eq!(keys.len(), 1, "unexpected objects: {keys:?}");
    assert!(keys[0].starts_with("db/db-"), "{}", keys[0]);
    let ext = dvb::config::Compression::Zstd
        .extension()
        .expect("zstd has an extension");
    assert!(keys[0].ends_with(&format!(".{ext}")), "{}", keys[0]);

    // `dvb list` reports it with the timestamp parsed from the name.
    let output = dvb(tmp.path())
        .args(["list", "db"])
        .output()
        .expect("run dvb list");
    assert!(
        output.status.success(),
        "list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("db-"), "{stdout}");
    assert!(stdout.contains(".zst"), "{stdout}");

    // And the stored bytes are a readable zstd tar.
    let raw = client.read(&keys[0]).await.expect("read back");
    let tar_bytes = zstd::stream::decode_all(std::io::Cursor::new(raw.to_bytes()))
        .expect("decode stored archive");
    let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
    let names: Vec<String> = archive
        .entries()
        .expect("tar entries")
        .map(|entry| {
            entry
                .expect("valid entry")
                .path()
                .expect("path")
                .display()
                .to_string()
        })
        .collect();
    assert!(names.contains(&"pgdata/a.txt".to_owned()), "{names:?}");
    assert!(
        names.contains(&"pgdata/nested/b.bin".to_owned()),
        "{names:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with cargo test --test s3_minio -- --ignored"]
async fn uploads_an_archive_larger_than_one_multipart_part() {
    let seaweed = SeaweedS3::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = tmp.path().join("source/data");
    std::fs::create_dir_all(&source).expect("mkdir");

    // ~60 MiB across several files. The bytes come from a xorshift PRNG, so the
    // archive stays above 50 MiB and exceeds the 8 MiB write chunk: that is what
    // forces a multipart upload. A repeating pattern would compress away and
    // quietly skip the path under test.
    for index in 0..12_u64 {
        let bytes = pseudo_random_bytes(5_000_000, index);
        std::fs::write(source.join(format!("part-{index:02}.bin")), &bytes).expect("write");
    }

    seaweed.write_config(tmp.path(), &source, "big");
    let output = dvb(tmp.path())
        .args(["backup", "db"])
        .output()
        .expect("run dvb backup");
    assert!(
        output.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let client = seaweed.operator();
    let listed = client.list_with("big").recursive(true).await.expect("list");
    let keys: Vec<String> = listed
        .into_iter()
        .map(|entry| entry.path().to_owned())
        .collect();
    assert_eq!(keys.len(), 1, "unexpected objects: {keys:?}");

    let size = client.stat(&keys[0]).await.expect("stat").content_length();
    assert!(
        size > 50 * 1024 * 1024,
        "expected a >50MiB object, got {size} bytes"
    );

    // A multipart upload that lost or reordered a part would fail this read or
    // produce a truncated tar.
    let raw = client.read(&keys[0]).await.expect("read back");
    assert_eq!(raw.len() as u64, size, "read length differs from stat");
    let tar_bytes =
        zstd::stream::decode_all(std::io::Cursor::new(raw.to_bytes())).expect("decode archive");
    let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
    let count = archive.entries().expect("tar entries").count();
    assert_eq!(
        count, 13,
        "expected the root dir plus 12 parts, got {count}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with cargo test --test s3_minio -- --ignored"]
async fn check_reports_the_backend_as_working() {
    let seaweed = SeaweedS3::start().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = tmp.path().join("source/pgdata");
    std::fs::create_dir_all(&source).expect("mkdir");
    std::fs::write(source.join("a.txt"), b"x").expect("write");

    seaweed.write_config(tmp.path(), &source, "check");
    let output = dvb(tmp.path())
        .arg("check")
        .output()
        .expect("run dvb check");
    assert!(
        output.status.success(),
        "check failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("storage: ok"), "{stdout}");
    assert!(stdout.contains("all checks passed"), "{stdout}");

    // The probe object must not be left behind.
    let client = seaweed.operator();
    assert!(
        !client.exists(".dvb-check/probe").await.unwrap_or(false),
        "the check probe should have removed itself"
    );
}
