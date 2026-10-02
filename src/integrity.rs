//! Archive integrity verification using SHA-256 sidecars.
//!
//! Backup creates a `<archive-name>.sha256` sidecar in storage with format:
//! `<hex>  <archive-name>\n` (compatible with `sha256sum -c`).
//! Restore computes SHA-256 over the compressed archive stream and verifies it
//! against the sidecar before executing any restore script.

use sha2::{Digest as _, Sha256};
use std::io::Read;

use crate::archive::ChunkSink;
use crate::error::Result;

/// A [`ChunkSink`] wrapper that computes SHA-256 over compressed archive chunks as they are written.
pub struct HashingSink<S> {
    inner: S,
    hasher: Sha256,
}

impl<S: ChunkSink> HashingSink<S> {
    /// Create a new hashing sink wrapping `inner`.
    #[must_use]
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    /// Complete the hash and return the inner sink along with the lowercase hex digest.
    #[must_use]
    pub fn finish_hex(self) -> (S, String) {
        let digest = self.hasher.finalize();
        (self.inner, hex::encode(digest))
    }
}

impl<S: ChunkSink> ChunkSink for HashingSink<S> {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        self.hasher.update(chunk);
        self.inner.write_chunk(chunk).await
    }
}

/// An [`std::io::Read`] wrapper that computes SHA-256 over bytes read.
pub struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> HashingReader<R> {
    /// Create a new hashing reader wrapping `inner`.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    /// Consume the reader and return the lowercase hex digest.
    #[must_use]
    pub fn finish_hex(self) -> String {
        let digest = self.hasher.finalize();
        hex::encode(digest)
    }

    /// Drain remaining bytes to EOF to ensure the entire stream is hashed.
    ///
    /// # Errors
    ///
    /// Returns any IO error encountered while draining.
    pub fn drain_to_end(&mut self) -> std::io::Result<()> {
        let mut buf = [0u8; 8192];
        loop {
            match self.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.hasher.update(&buf[..n]);
        }
        Ok(n)
    }
}

/// The remote storage path for the archive's `.sha256` sidecar.
#[must_use]
pub fn sidecar_path(archive_remote: &str) -> String {
    format!("{archive_remote}.sha256")
}

/// Strip the `.sha256` suffix from a storage path, returning the archive path if present.
#[must_use]
pub fn strip_sidecar_suffix(path: &str) -> Option<&str> {
    path.strip_suffix(".sha256")
}

/// Format the content of a `.sha256` file: `<hex>  <archive_name>\n` (two spaces between).
#[must_use]
pub fn format_sidecar(checksum: &str, archive_name: &str) -> String {
    format!("{checksum}  {archive_name}\n")
}

/// Parse sidecar content into `(checksum, archive_name)`.
///
/// Accepts standard `sha256sum` output with 64 hex characters followed by whitespace and filename.
#[must_use]
pub fn parse_sidecar(content: &str) -> Option<(String, String)> {
    let mut parts = content.split_whitespace();
    let hex_part = parts.next()?;
    let name_part = parts.next()?;
    if hex_part.len() != 64 || !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((hex_part.to_ascii_lowercase(), name_part.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_and_parse_sidecar() {
        let checksum = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let filename = "db-20261001T120000Z.tar.zst";
        let formatted = format_sidecar(checksum, filename);
        assert_eq!(
            formatted,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  db-20261001T120000Z.tar.zst\n"
        );

        let (parsed_hash, parsed_name) = parse_sidecar(&formatted).expect("valid sidecar");
        assert_eq!(parsed_hash, checksum);
        assert_eq!(parsed_name, filename);
    }

    #[test]
    fn parse_sidecar_rejects_malformed() {
        assert!(parse_sidecar("").is_none());
        assert!(parse_sidecar("too_short filename.tar.zst").is_none());
        assert!(
            parse_sidecar(
                "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz filename"
            )
            .is_none()
        );
        assert!(
            parse_sidecar("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
                .is_none()
        );
    }

    #[test]
    fn hashing_reader_computes_sha256() {
        let data = b"hello world integrity test";
        let mut reader = HashingReader::new(&data[..]);
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).expect("read");
        let hash = reader.finish_hex();

        let mut expected_hasher = Sha256::new();
        expected_hasher.update(data);
        let expected = hex::encode(expected_hasher.finalize());
        assert_eq!(hash, expected);
    }

    #[tokio::test]
    async fn hashing_sink_computes_sha256() {
        let mut output = Vec::new();
        let mut sink = HashingSink::new(&mut output);
        sink.write_chunk(b"chunk1 ").await.expect("chunk1");
        sink.write_chunk(b"chunk2").await.expect("chunk2");
        let (_, hash) = sink.finish_hex();

        let mut expected_hasher = Sha256::new();
        expected_hasher.update(b"chunk1 chunk2");
        let expected = hex::encode(expected_hasher.finalize());
        assert_eq!(hash, expected);
        assert_eq!(output, b"chunk1 chunk2");
    }

    #[test]
    fn sidecar_path_helpers() {
        assert_eq!(sidecar_path("db/bk.tar.zst"), "db/bk.tar.zst.sha256");
        assert_eq!(
            strip_sidecar_suffix("db/bk.tar.zst.sha256"),
            Some("db/bk.tar.zst")
        );
        assert_eq!(strip_sidecar_suffix("db/bk.tar.zst"), None);
    }
}
