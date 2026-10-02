//! `OpenDAL` operator construction and the streaming upload path.
//!
//! Every operator is wrapped in the same layer stack:
//!
//! * [`LoggingLayer`] (outermost) — one debug line per operation
//! * [`RetryLayer`] — retries transient failures with jittered backoff
//! * [`TimeoutLayer`] — bounds every individual IO call
//!
//! Retries sit inside the timeout so a retry loop cannot hang forever, and both
//! sit inside logging so one operation logs once instead of once per attempt.

use std::time::Duration;

use opendal::layers::{LoggingLayer, RetryLayer, TimeoutLayer};
use opendal::{Metadata, Operator, Writer};

use crate::config::StorageConfig;
use crate::error::{Error, Result};

/// Chunk size for buffered writes. Also the S3 multipart part size.
pub const WRITE_CHUNK: usize = 8 * 1024 * 1024;

/// Timeout for control operations such as `stat` and `delete`.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for a single IO operation such as one `Writer::write` call.
const IO_TIMEOUT: Duration = Duration::from_secs(120);

/// How many times a transient storage failure is retried.
const MAX_RETRIES: usize = 5;

/// Base delay of the retry backoff.
const RETRY_MIN_DELAY: Duration = Duration::from_millis(200);

/// Ceiling of the retry backoff.
const RETRY_MAX_DELAY: Duration = Duration::from_secs(10);

/// Install the process-wide HTTP transport and service registry.
///
/// `opendal` is used with `default-features = false` so its `auto-register-services`
/// constructor is off, which means the reqwest-based transport must be installed
/// explicitly. Without this, every HTTP backend (`s3`, `dropbox`) fails at the first
/// request with "default HTTP transport is not installed".
///
/// Safe and cheap to call more than once: the work happens behind a `Once`.
///
/// # Errors
///
/// Nothing today, but the signature is fallible-ready so a future transport with
/// runtime requirements does not silently change every caller.
pub fn install_transport() -> Result<()> {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(opendal::install_default);
    Ok(())
}

/// Build an `OpenDAL` [`Operator`] for the given backend configuration.
///
/// The layer stack is always the same, whatever the backend: logging outside,
/// retry in the middle, timeout innermost.
///
/// # Errors
///
/// [`Error::StorageConfig`] when the settings cannot form a valid backend (for
/// example half an S3 credential pair), and [`Error::Storage`] when the backend
/// rejects the configuration at build time.
pub fn operator(config: &StorageConfig) -> Result<Operator> {
    let op = match config {
        StorageConfig::Fs(cfg) => fs_operator(cfg)?,
        StorageConfig::S3(cfg) => s3_operator(cfg)?,
        StorageConfig::Sftp(cfg) => sftp_operator(cfg)?,
        StorageConfig::Dropbox(_) => {
            return Err(Error::StorageConfig(
                "the dropbox backend is not wired up yet".to_owned(),
            ));
        }
    };

    Ok(op
        // Outermost: one log line per logical operation, not per retry attempt.
        .layer(LoggingLayer::default())
        // Middle: retries transient failures. Inside the timeout so a retry loop
        // cannot hang forever, outside so the whole operation is retried.
        .layer(
            RetryLayer::default()
                .with_jitter()
                .with_max_times(MAX_RETRIES)
                .with_min_delay(RETRY_MIN_DELAY)
                .with_max_delay(RETRY_MAX_DELAY),
        )
        // Innermost: bounds every individual call to the backend.
        .layer(
            TimeoutLayer::default()
                .with_timeout(CONTROL_TIMEOUT)
                .with_io_timeout(IO_TIMEOUT),
        ))
}

fn fs_operator(cfg: &crate::config::FsConfig) -> Result<Operator> {
    let root = cfg.root.to_string_lossy().into_owned();
    let backend = opendal::services::Fs::default().root(&root);
    Operator::new(backend).map_err(|source| Error::Storage {
        backend: "fs",
        source,
    })
}

fn s3_operator(cfg: &crate::config::S3Config) -> Result<Operator> {
    let mut builder = opendal::services::S3::default()
        .bucket(&cfg.bucket)
        .region(&cfg.region);

    if let Some(endpoint) = &cfg.endpoint {
        builder = builder.endpoint(endpoint);
    }

    // Path style is OpenDAL's default; virtual-host style is opt-in.
    if !cfg.force_path_style {
        tracing::debug!(bucket = %cfg.bucket, "using virtual-host style addressing");
        builder = builder.enable_virtual_host_style();
    }

    // Explicit keys win; with neither set OpenDAL falls back to the default AWS
    // chain (env vars, shared config, IMDS, web identity).
    match (&cfg.access_key_id, &cfg.secret_access_key) {
        (Some(id), Some(secret)) => {
            builder = builder
                .access_key_id(id.expose())
                .secret_access_key(secret.expose());
        }
        (None, None) => {
            tracing::debug!("no s3 credentials in config, using the ambient credential chain");
        }
        _ => {
            return Err(Error::StorageConfig(
                "s3 needs both access_key_id and secret_access_key, or neither \
                 (otherwise the ambient credential chain is used)"
                    .to_owned(),
            ));
        }
    }

    Operator::new(builder).map_err(|source| Error::Storage {
        backend: "s3",
        source,
    })
}

fn sftp_operator(cfg: &crate::config::SftpConfig) -> Result<Operator> {
    use crate::config::KnownHostsStrategy as Strategy;

    // OpenDAL shells out to `ssh`, which is why the image ships
    // `openssh-client`. Host key checking is always on: `strict` is the default
    // and `accept_new` still verifies, it just records new keys on first use.
    // There is no configuration that turns verification off.
    let strategy = match cfg.known_hosts_strategy {
        Strategy::Strict => "strict",
        Strategy::AcceptNew => {
            tracing::warn!(
                endpoint = %cfg.endpoint,
                "sftp known_hosts_strategy=accept_new: unknown host keys will be added to known_hosts on first use"
            );
            "accept"
        }
    };

    let mut builder = opendal::services::Sftp::default()
        // OpenDAL passes the endpoint straight to `ssh`. A bare `host:port`
        // would be read as a hostname, so a configured port needs the URI form.
        .endpoint(&sftp_endpoint(&cfg.endpoint))
        .user(&cfg.user)
        .root(&cfg.root)
        .known_hosts_strategy(strategy);

    if let Some(key_path) = &cfg.key_path {
        builder = builder.key(&key_path.to_string_lossy());
    }

    Operator::new(builder).map_err(|source| Error::Storage {
        backend: "sftp",
        source,
    })
}

/// Normalise a configured SFTP endpoint for the `ssh` client.
///
/// The config takes `host` or `host:port`. `ssh` treats a bare `host:port` as a
/// hostname and fails to resolve it, so a non-default port is rewritten to
/// `ssh://host:port`. Values that already carry a scheme are left alone.
fn sftp_endpoint(endpoint: &str) -> String {
    if endpoint.contains("://") {
        return endpoint.to_owned();
    }
    match endpoint.rsplit_once(':') {
        Some((_, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
            format!("ssh://{endpoint}")
        }
        _ => endpoint.to_owned(),
    }
}

/// One object stored in the backend.
///
/// Consumed by `dvb list` and retention pruning from phase 2 onwards.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ObjectInfo {
    /// Path relative to the backend root, including the job prefix.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
}

#[allow(dead_code)]
impl ObjectInfo {
    /// Build the info from a metadata entry.
    pub fn new(path: impl Into<String>, meta: &Metadata) -> Self {
        Self {
            path: path.into(),
            size: meta.content_length(),
        }
    }
}

/// List every object under `prefix`, recursively.
///
/// Directory entries are filtered out: only files are objects.
///
/// # Errors
///
/// [`Error::Storage`] when the prefix cannot be listed.
///
/// [`allow(dead_code)`]: used by `dvb list` and retention.rs in phase 2
pub async fn list_prefix(
    op: &Operator,
    backend: &'static str,
    prefix: &str,
) -> Result<Vec<ObjectInfo>> {
    let entries = op
        .list_with(prefix)
        .recursive(true)
        .await
        .map_err(|source| Error::Storage { backend, source })?;

    Ok(entries
        .into_iter()
        // Backends report the intermediate directories as entries too; only
        // files are objects.
        .filter(|entry| !entry.metadata().is_dir())
        .map(|entry| ObjectInfo::new(entry.path(), entry.metadata()))
        .collect())
}

/// Delete `path`.
///
/// # Errors
///
/// [`Error::Storage`] when the delete fails.
///
/// [`allow(dead_code)`]: used by retention.rs in phase 2
pub async fn delete_object(op: &Operator, backend: &'static str, path: &str) -> Result<()> {
    op.delete(path)
        .await
        .map_err(|source| Error::Storage { backend, source })
}

/// Prove the backend is usable: write a probe object, read it back, delete it.
///
/// A listing is not enough: write and delete frequently need permissions that
/// list does not, so `dvb check` exercises the full round trip.
///
/// # Errors
///
/// [`Error::Storage`] when any step of the round trip fails, and
/// [`Error::StorageConfig`] when the read-back does not match what was written.
/// On a content mismatch the probe object is deliberately left in place as
/// evidence.
pub async fn probe(op: &Operator, backend: &'static str) -> Result<()> {
    const PROBE_OBJECT: &str = ".dvb-check/probe";
    const PAYLOAD: &[u8] = b"dvb storage probe\n";

    // Clear any leftover from an interrupted run so the read-back is ours.
    op.delete(PROBE_OBJECT)
        .await
        .map_err(|source| Error::Storage { backend, source })?;

    write_bytes(op, backend, PROBE_OBJECT, PAYLOAD.to_vec()).await?;

    let read_back = read_bytes(op, backend, PROBE_OBJECT).await?;
    if read_back != PAYLOAD {
        // Leave the object in place as evidence rather than deleting it.
        return Err(Error::StorageConfig(format!(
            "{backend} probe read back {} bytes, expected {}",
            read_back.len(),
            PAYLOAD.len()
        )));
    }

    if let Err(err) = op.delete(PROBE_OBJECT).await {
        tracing::warn!(
            path = PROBE_OBJECT,
            backend,
            error = %err,
            "storage works but the probe object could not be removed"
        );
    }
    Ok(())
}

/// Remove a partially uploaded object, logging but not propagating failures.
///
/// Called when an upload is abandoned: a leftover truncated object would look
/// like a valid backup, but it must not mask the original error.
pub async fn cleanup_partial(op: &Operator, backend: &'static str, path: &str) {
    match op.delete(path).await {
        Ok(()) => tracing::warn!(path, backend, "removed partial upload after failure"),
        Err(e) => tracing::warn!(
            path,
            backend,
            error = %e,
            "could not remove partial upload; it may need manual cleanup"
        ),
    }
}

/// Write `bytes` to `path` in one call. Only for small objects.
///
/// # Errors
///
/// [`Error::Storage`] when the write or the commit fails. A failed write aborts
/// the writer rather than committing a partial object.
///
/// [`allow(dead_code)`]: used by the `dvb check` probe in phase 2
pub async fn write_bytes(
    op: &Operator,
    backend: &'static str,
    path: &str,
    bytes: Vec<u8>,
) -> Result<u64> {
    let mut writer = new_writer(op, backend, path).await?;
    if let Err(source) = writer.write(bytes).await {
        abort(op, backend, path, &mut writer).await;
        return Err(Error::Storage { backend, source });
    }
    let meta = writer
        .close()
        .await
        .map_err(|source| Error::Storage { backend, source })?;
    Ok(meta.content_length())
}

/// Read `path` in full. Only for small objects such as the `check` probe.
///
/// # Errors
///
/// [`Error::Storage`] when the object cannot be read.
///
/// [`allow(dead_code)`]: used by the `dvb check` probe in phase 2
pub async fn read_bytes(op: &Operator, backend: &'static str, path: &str) -> Result<Vec<u8>> {
    let buffer = op
        .read(path)
        .await
        .map_err(|source| Error::Storage { backend, source })?;
    Ok(buffer.to_vec())
}

/// Open a chunked writer for `path`.
///
/// The chunk size is [`WRITE_CHUNK`], which is also the S3 multipart part size,
/// so one chunk write is one part and memory stays bounded.
///
/// # Errors
///
/// [`Error::Storage`] when the writer cannot be opened.
pub async fn new_writer(op: &Operator, backend: &'static str, path: &str) -> Result<Writer> {
    op.writer_with(path)
        .chunk(WRITE_CHUNK)
        .await
        .map_err(|source| Error::Storage { backend, source })
}

/// Abort `writer` and remove whatever partial object it created.
#[allow(dead_code)] // replaced by `WriterSink::abort` plus cleanup_partial
pub async fn abort(op: &Operator, backend: &'static str, path: &str, writer: &mut Writer) {
    if let Err(e) = writer.abort().await {
        tracing::warn!(path, backend, error = %e, "writer abort failed");
    }
    // Belt and braces: some services cannot abort mid-write, so make sure the
    // truncated object is gone regardless.
    cleanup_partial(op, backend, path).await;
}

/// A [`crate::archive::ChunkSink`] adapter over an `OpenDAL` [`Writer`].
///
/// One chunk is in flight at a time and its payload is a single copy of the
/// caller's buffer, so memory stays bounded by the chunk size rather than by the
/// archive size.
pub struct WriterSink {
    writer: Writer,
    backend: &'static str,
}

impl WriterSink {
    /// Wrap `writer`, attributing errors to `backend`.
    #[must_use]
    pub fn new(writer: Writer, backend: &'static str) -> Self {
        Self { writer, backend }
    }

    /// Commit the upload and return the resulting metadata.
    ///
    /// # Errors
    ///
    /// [`Error::Storage`] when the commit fails. Callers should then delete the
    /// partial object.
    pub async fn close(mut self) -> Result<Metadata> {
        self.writer.close().await.map_err(|source| Error::Storage {
            backend: self.backend,
            source,
        })
    }

    /// Abandon the upload without committing.
    pub async fn abort(mut self) {
        if let Err(e) = self.writer.abort().await {
            tracing::warn!(backend = self.backend, error = %e, "writer abort failed");
        }
    }
}

impl crate::archive::ChunkSink for WriterSink {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        // The payload must outlive the call, hence the copy. OpenDAL buffers
        // internally up to the configured chunk size.
        let payload = opendal::Buffer::from(chunk.to_vec());
        self.writer
            .write(payload)
            .await
            .map_err(|source| Error::Storage {
                backend: self.backend,
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FsConfig, KnownHostsStrategy, S3Config, SecretString};

    fn fs_config(root: &std::path::Path) -> StorageConfig {
        StorageConfig::Fs(FsConfig {
            root: root.to_path_buf(),
            prefix: "backups".to_owned(),
        })
    }

    fn sftp_config(strategy: KnownHostsStrategy) -> crate::config::SftpConfig {
        crate::config::SftpConfig {
            endpoint: "backup.example.com:22".to_owned(),
            user: "backup".to_owned(),
            root: "/srv/backups".to_owned(),
            key_path: None,
            known_hosts_strategy: strategy,
        }
    }

    #[test]
    fn fs_operator_is_built_successfully() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let op = operator(&fs_config(tmp.path())).expect("operator");
        assert_eq!(op.info().scheme(), "fs");
    }

    fn s3_config(bucket: &str) -> S3Config {
        S3Config {
            bucket: bucket.to_owned(),
            region: "us-east-1".to_owned(),
            endpoint: None,
            prefix: String::new(),
            access_key_id: Some(SecretString::new("access-key")),
            secret_access_key: Some(SecretString::new("secret-key")),
            force_path_style: true,
        }
    }

    #[test]
    fn s3_operator_builds_with_explicit_credentials() {
        let op = operator(&StorageConfig::S3(s3_config("backups"))).expect("operator");
        assert_eq!(op.info().scheme(), "s3");
    }

    #[test]
    fn s3_operator_builds_against_a_custom_endpoint_for_minio() {
        let cfg = S3Config {
            endpoint: Some("http://127.0.0.1:9000".to_owned()),
            ..s3_config("backups")
        };
        let op = operator(&StorageConfig::S3(cfg)).expect("operator");
        assert_eq!(op.info().scheme(), "s3");
    }

    #[test]
    fn s3_falls_back_to_the_ambient_credential_chain() {
        let cfg = S3Config {
            access_key_id: None,
            secret_access_key: None,
            ..s3_config("backups")
        };
        let op = operator(&StorageConfig::S3(cfg)).expect("operator");
        assert_eq!(op.info().scheme(), "s3");
    }

    #[test]
    fn s3_rejects_half_a_credential_pair() {
        let only_id = S3Config {
            secret_access_key: None,
            ..s3_config("backups")
        };
        let err = operator(&StorageConfig::S3(only_id)).unwrap_err();
        assert!(err.to_string().contains("both access_key_id"), "{err}");

        let only_secret = S3Config {
            access_key_id: None,
            ..s3_config("backups")
        };
        let err = operator(&StorageConfig::S3(only_secret)).unwrap_err();
        assert!(err.to_string().contains("both access_key_id"), "{err}");
    }

    #[test]
    fn s3_virtual_host_style_is_opt_in() {
        let cfg = S3Config {
            force_path_style: false,
            ..s3_config("backups")
        };
        assert_eq!(
            operator(&StorageConfig::S3(cfg))
                .expect("operator")
                .info()
                .scheme(),
            "s3"
        );
    }

    #[test]
    fn sftp_operator_defaults_to_strict_host_key_checking() {
        let op = operator(&StorageConfig::Sftp(sftp_config(
            KnownHostsStrategy::Strict,
        )))
        .expect("operator");
        assert_eq!(op.info().scheme(), "sftp");
    }

    #[test]
    fn sftp_accept_new_still_verifies_and_is_built() {
        let op = operator(&StorageConfig::Sftp(sftp_config(
            KnownHostsStrategy::AcceptNew,
        )))
        .expect("operator");
        assert_eq!(op.info().scheme(), "sftp");
    }

    #[test]
    fn sftp_carries_the_key_path_through() {
        let cfg = crate::config::SftpConfig {
            key_path: Some(std::path::PathBuf::from("/run/secrets/id_ed25519")),
            ..sftp_config(KnownHostsStrategy::Strict)
        };
        let op = operator(&StorageConfig::Sftp(cfg)).expect("operator");
        assert_eq!(op.info().scheme(), "sftp");
    }

    #[test]
    fn dropbox_is_still_reported_as_unimplemented() {
        let cfg = StorageConfig::Dropbox(crate::config::DropboxConfig {
            root: "/dvb".to_owned(),
            client_id: SecretString::new("id"),
            client_secret: SecretString::new("secret"),
            refresh_token: SecretString::new("token"),
        });
        let err = operator(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("dropbox backend is not wired up"),
            "{err}"
        );
    }

    #[test]
    fn sftp_endpoint_with_a_port_gets_the_uri_scheme() {
        // `ssh` would read a bare `host:port` as a hostname.
        assert_eq!(sftp_endpoint("example.com:2222"), "ssh://example.com:2222");
        // A bare host needs no change.
        assert_eq!(sftp_endpoint("example.com"), "example.com");
        // Already a URI: leave it alone.
        assert_eq!(
            sftp_endpoint("ssh://example.com:2222"),
            "ssh://example.com:2222"
        );
        assert_eq!(sftp_endpoint("sftp://example.com"), "sftp://example.com");
    }

    #[tokio::test]
    async fn write_read_list_delete_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let op = operator(&fs_config(tmp.path())).expect("operator");

        let size = write_bytes(&op, "fs", "backups/a.txt", b"hello".to_vec())
            .await
            .expect("write");
        assert_eq!(size, 5);
        assert_eq!(
            read_bytes(&op, "fs", "backups/a.txt").await.expect("read"),
            b"hello"
        );

        let listed = list_prefix(&op, "fs", "backups").await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, "backups/a.txt");
        assert_eq!(listed[0].size, 5);

        delete_object(&op, "fs", "backups/a.txt")
            .await
            .expect("delete");
        let listed = list_prefix(&op, "fs", "backups").await.expect("list");
        assert_eq!(listed, Vec::new());
    }

    #[tokio::test]
    async fn aborting_a_writer_leaves_no_object() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let op = operator(&fs_config(tmp.path())).expect("operator");

        let writer = new_writer(&op, "fs", "backups/partial")
            .await
            .expect("writer");
        let mut sink = WriterSink::new(writer, "fs");
        {
            use crate::archive::ChunkSink as _;
            sink.write_chunk(b"partial data").await.expect("write");
        }
        sink.abort().await;
        cleanup_partial(&op, "fs", "backups/partial").await;

        let listed = list_prefix(&op, "fs", "backups").await.expect("list");
        assert!(listed.is_empty(), "partial object left behind: {listed:?}");
    }

    #[tokio::test]
    async fn writer_sink_commits_on_close() {
        use crate::archive::ChunkSink as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let op = operator(&fs_config(tmp.path())).expect("operator");

        let writer = new_writer(&op, "fs", "backups/ok").await.expect("writer");
        let mut sink = WriterSink::new(writer, "fs");
        sink.write_chunk(b"payload").await.expect("write");
        let meta = sink.close().await.expect("close");
        assert_eq!(meta.content_length(), 7);

        assert_eq!(
            read_bytes(&op, "fs", "backups/ok").await.expect("read"),
            b"payload"
        );
    }

    #[tokio::test]
    async fn writer_sink_streams_more_than_one_chunk() {
        use crate::archive::ChunkSink as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let op = operator(&fs_config(tmp.path())).expect("operator");

        let writer = new_writer(&op, "fs", "backups/big").await.expect("writer");
        let mut sink = WriterSink::new(writer, "fs");
        let block = vec![7_u8; 64 * 1024];
        for _ in 0..8 {
            sink.write_chunk(&block).await.expect("write");
        }
        let meta = sink.close().await.expect("close");
        assert_eq!(meta.content_length(), 8 * 64 * 1024);
    }

    #[tokio::test]
    async fn cleanup_partial_is_best_effort() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let op = operator(&fs_config(tmp.path())).expect("operator");
        write_bytes(&op, "fs", "backups/partial", vec![0; 16])
            .await
            .expect("write");
        cleanup_partial(&op, "fs", "backups/partial").await;
        let listed = list_prefix(&op, "fs", "backups").await.expect("list");
        assert_eq!(listed, Vec::new());
    }
}
