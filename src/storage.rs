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

/// Build an `OpenDAL` [`Operator`] for the given backend configuration.
///
/// Only the `fs` backend is wired up so far; the others are recognised by the
/// configuration parser but rejected here.
pub fn operator(config: &StorageConfig) -> Result<Operator> {
    let op = match config {
        StorageConfig::Fs(cfg) => fs_operator(cfg)?,
        StorageConfig::S3(_) => {
            return Err(Error::StorageConfig(
                "the s3 backend is not wired up yet".to_owned(),
            ));
        }
        StorageConfig::Sftp(_) => {
            return Err(Error::StorageConfig(
                "the sftp backend is not wired up yet".to_owned(),
            ));
        }
        StorageConfig::Dropbox(_) => {
            return Err(Error::StorageConfig(
                "the dropbox backend is not wired up yet".to_owned(),
            ));
        }
    };

    Ok(op
        .layer(LoggingLayer::default())
        .layer(
            RetryLayer::default()
                .with_jitter()
                .with_max_times(MAX_RETRIES)
                .with_min_delay(RETRY_MIN_DELAY)
                .with_max_delay(RETRY_MAX_DELAY),
        )
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
#[allow(dead_code)] // used by `dvb list` and retention.rs in phase 2
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
#[allow(dead_code)] // used by retention.rs in phase 2
pub async fn delete_object(op: &Operator, backend: &'static str, path: &str) -> Result<()> {
    op.delete(path)
        .await
        .map_err(|source| Error::Storage { backend, source })
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
#[allow(dead_code)] // used by the `dvb check` probe in phase 2
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
#[allow(dead_code)] // used by the `dvb check` probe in phase 2
pub async fn read_bytes(op: &Operator, backend: &'static str, path: &str) -> Result<Vec<u8>> {
    let buffer = op
        .read(path)
        .await
        .map_err(|source| Error::Storage { backend, source })?;
    Ok(buffer.to_vec())
}

/// Open a chunked writer for `path`.
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
    pub fn new(writer: Writer, backend: &'static str) -> Self {
        Self { writer, backend }
    }

    /// Commit the upload and return the resulting metadata.
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
    use crate::config::{FsConfig, S3Config, SecretString};

    fn fs_config(root: &std::path::Path) -> StorageConfig {
        StorageConfig::Fs(FsConfig {
            root: root.to_path_buf(),
            prefix: "backups".to_owned(),
        })
    }

    #[test]
    fn fs_operator_is_built_successfully() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let op = operator(&fs_config(tmp.path())).expect("operator");
        assert_eq!(op.info().scheme(), "fs");
    }

    #[test]
    fn unimplemented_backends_are_rejected_clearly() {
        let s3 = StorageConfig::S3(S3Config {
            bucket: "b".to_owned(),
            region: "us-east-1".to_owned(),
            endpoint: None,
            prefix: String::new(),
            access_key_id: Some(SecretString::new("id")),
            secret_access_key: Some(SecretString::new("secret")),
            force_path_style: true,
        });
        let err = operator(&s3).unwrap_err();
        assert!(
            err.to_string().contains("s3 backend is not wired up"),
            "{err}"
        );
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
