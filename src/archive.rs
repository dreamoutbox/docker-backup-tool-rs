//! Streaming tar + compression.
//!
//! The archive is produced on the blocking pool and handed to the async world
//! through a `tokio::io::duplex` pipe wrapped in `SyncIoBridge`, so neither the
//! compressor nor the storage client ever buffers the whole archive.
//!
//! Errors while reading sources are surfaced, never skipped: a backup that
//! silently misses files is worse than a failed one.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use std::future::Future;

use tokio::io::AsyncReadExt as _;
use tokio_util::io::SyncIoBridge;

use crate::config::Compression;
use crate::error::{Error, Result};

/// Size of the in-memory pipe between the blocking archiver and the uploader.
pub const PIPE_BUFFER: usize = 512 * 1024;

/// Options controlling how a source tree is turned into an archive.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArchiveOptions {
    /// Compression applied to the tar stream.
    pub compression: Compression,
    /// When true, symlinks are followed and their targets archived; when false,
    /// the symlink itself is stored.
    pub follow_symlinks: bool,
}

/// What a finished archive contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArchiveStats {
    /// Number of tar entries written.
    pub entries: u64,
    /// Size of the tar stream before compression, in bytes.
    pub uncompressed_bytes: u64,
    /// Compressed size that reached the sink, in bytes.
    pub compressed_bytes: u64,
}

/// A source path plus the tar entry root it is stored under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Filesystem path to archive.
    pub path: PathBuf,
    /// Name of the tar entry root, normally the source's basename.
    pub name: String,
}

impl Source {
    /// Build a source from a path, using its basename as the entry root.
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let name = path.file_name().map_or_else(
            || "root".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        Self { path, name }
    }
}

/// Tar + compress `sources` into `writer`, returning what was written.
///
/// This blocks; use [`stream_to`] to drive it from async code.
pub fn write_archive<W: Write>(
    sources: &[Source],
    writer: W,
    options: ArchiveOptions,
) -> Result<ArchiveStats> {
    // The byte counter is shared with the writer wrapper. `Rc` is fine here: the
    // whole archiver runs on one blocking thread.
    let counter = ByteCount::default();

    let entries = match options.compression {
        Compression::None => {
            let mut archive = tar::Builder::new(TarStats::new(writer, counter.clone()));
            let entries = archive_sources(sources, &mut archive, options)?;
            archive.into_inner().map_err(Error::archive_other)?;
            entries
        }
        Compression::Gzip => {
            let encoder = flate2::write::GzEncoder::new(
                TarStats::new(writer, counter.clone()),
                flate2::Compression::new(Compression::gzip_level()),
            );
            let mut archive = tar::Builder::new(encoder);
            let entries = archive_sources(sources, &mut archive, options)?;
            archive
                .into_inner()
                .map_err(Error::archive_other)?
                .finish()
                .map_err(Error::archive_other)?;
            entries
        }
        Compression::Zstd => {
            let encoder = zstd::stream::write::Encoder::new(
                TarStats::new(writer, counter.clone()),
                Compression::zstd_level(),
            )
            .map_err(Error::archive_other)?;
            let mut archive = tar::Builder::new(encoder);
            let entries = archive_sources(sources, &mut archive, options)?;
            archive
                .into_inner()
                .map_err(Error::archive_other)?
                .finish()
                .map_err(Error::archive_other)?;
            entries
        }
    };

    Ok(ArchiveStats {
        entries,
        uncompressed_bytes: counter.get(),
        compressed_bytes: 0,
    })
}

/// Walk `sources` into `archive`, surfacing every IO error. Returns the entry count.
fn archive_sources<W: Write>(
    sources: &[Source],
    archive: &mut tar::Builder<W>,
    options: ArchiveOptions,
) -> Result<u64> {
    // `Complete` writes uid/gid/uname/gname alongside mode and mtime, which is
    // what a faithful `tar` does.
    archive.mode(tar::HeaderMode::Complete);

    let mut entries = 0_u64;
    for source in sources {
        let meta =
            std::fs::symlink_metadata(&source.path).map_err(|e| Error::archive(&source.path, e))?;
        entries += add_node(archive, &source.path, &source.name, &meta, options)?;
    }

    archive.finish().map_err(Error::archive_other)?;
    Ok(entries)
}

/// Recursively add `path` to the archive under `name`. Returns the entries added.
fn add_node<W: Write>(
    archive: &mut tar::Builder<W>,
    path: &Path,
    name: &str,
    meta: &std::fs::Metadata,
    options: ArchiveOptions,
) -> Result<u64> {
    let is_symlink = meta.file_type().is_symlink();

    if is_symlink && !options.follow_symlinks {
        let target = std::fs::read_link(path).map_err(|e| Error::archive(path, e))?;
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        header.set_mtime(metadata_mtime(meta));
        header
            .set_link_name(target)
            .map_err(|e| Error::archive(path, io::Error::other(e)))?;
        header.set_cksum();
        archive
            .append_data(&mut header, name, io::empty())
            .map_err(|e| Error::archive(path, e))?;
        return Ok(1);
    }

    // Following a symlink (or reading a plain file) needs the target's metadata.
    let target_meta = if is_symlink {
        std::fs::metadata(path).map_err(|e| Error::archive(path, e))?
    } else {
        meta.clone()
    };

    if target_meta.is_dir() {
        let mut header = tar::Header::new_gnu();
        header.set_metadata(&target_meta);
        header.set_entry_type(tar::EntryType::Directory);
        header.set_cksum();
        archive
            .append_data(&mut header, name, io::empty())
            .map_err(|e| Error::archive(path, e))?;

        let mut entries = std::fs::read_dir(path)
            .map_err(|e| Error::archive(path, e))?
            .collect::<io::Result<Vec<_>>>()
            .map_err(|e| Error::archive(path, e))?;
        // Deterministic ordering keeps archives byte-reproducible.
        entries.sort_by_key(std::fs::DirEntry::file_name);

        let mut count = 1_u64;
        for entry in entries {
            let entry_path = entry.path();
            let entry_meta = entry
                .metadata()
                .map_err(|e| Error::archive(&entry_path, e))?;
            let entry_name = format!("{name}/{}", entry.file_name().to_string_lossy());
            count += add_node(archive, &entry_path, &entry_name, &entry_meta, options)?;
        }
        return Ok(count);
    }

    // Regular file: streamed through, so memory stays flat regardless of size.
    let mut file = File::open(path).map_err(|e| Error::archive(path, e))?;
    let mut header = tar::Header::new_gnu();
    header.set_metadata(&target_meta);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    archive
        .append_data(&mut header, name, &mut file)
        .map_err(|e| Error::archive(path, e))?;
    Ok(1)
}

fn metadata_mtime(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Shared byte counter, so the tar wrapper can report how much it wrote without
/// borrowing the caller's `ArchiveStats` (the compressor layer sits in between).
#[derive(Debug, Clone, Default)]
struct ByteCount(std::rc::Rc<std::cell::Cell<u64>>);

impl ByteCount {
    fn get(&self) -> u64 {
        self.0.get()
    }
}

/// Counts the bytes the tar layer writes so `ArchiveStats` is accurate without
/// reaching into the tar crate's internals.
struct TarStats<W> {
    inner: W,
    counter: ByteCount,
}

impl<W: Write> TarStats<W> {
    fn new(inner: W, counter: ByteCount) -> Self {
        Self { inner, counter }
    }
}

impl<W: Write> Write for TarStats<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.counter.0.set(self.counter.0.get() + written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Tar + compress `sources` on the blocking pool, piping the bytes into `sink`.
/// A consumer of compressed archive chunks.
///
/// Implemented by the storage writer. One chunk is outstanding at a time, which
/// is what keeps memory bounded regardless of archive size.
pub trait ChunkSink {
    /// Accept one chunk of the compressed stream.
    fn write_chunk(&mut self, chunk: &[u8]) -> impl Future<Output = Result<()>>;
}

impl<S: ChunkSink + ?Sized> ChunkSink for &mut S {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        (**self).write_chunk(chunk).await
    }
}

impl ChunkSink for Vec<u8> {
    fn write_chunk(&mut self, chunk: &[u8]) -> impl std::future::Future<Output = Result<()>> {
        self.extend_from_slice(chunk);
        std::future::ready(Ok(()))
    }
}

/// Tar + compress `sources` on the blocking pool, piping the bytes into `sink`.
///
/// Returns the tar stats plus the number of compressed bytes handed to `sink`.
/// Read errors on the source side surface as [`Error::Archive`]; a sink error is
/// wrapped the same way around its underlying cause.
pub async fn stream_to<S: ChunkSink>(
    sources: Vec<Source>,
    mut sink: S,
    options: ArchiveOptions,
) -> Result<ArchiveStats> {
    let (pipe_writer, mut pipe_reader) = tokio::io::duplex(PIPE_BUFFER);
    let bridge = SyncIoBridge::new(pipe_writer);

    let archiver = tokio::task::spawn_blocking(move || write_archive(&sources, bridge, options));

    let mut compressed = 0_u64;
    let mut buf = vec![0_u8; PIPE_BUFFER];
    let mut sink_error = None;
    loop {
        let read = match pipe_reader.read(&mut buf).await {
            Ok(read) => read,
            Err(e) => {
                sink_error = Some(e);
                break;
            }
        };
        if read == 0 {
            break;
        }
        if let Err(err) = sink.write_chunk(&buf[..read]).await {
            sink_error = Some(err.into_io());
            break;
        }
        compressed += read as u64;
    }

    // Surface source-side errors first: a truncated pipe also shows up as a
    // short read, and the archiver's error is the real cause.
    let mut stats = archiver.await.map_err(|e| {
        Error::archive_other(io::Error::other(format!("archiver task failed: {e}")))
    })??;

    if let Some(e) = sink_error {
        return Err(Error::archive_other(e));
    }

    stats.compressed_bytes = compressed;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Read as _;

    fn build_tree(root: &Path) {
        std::fs::create_dir_all(root.join("nested/deep")).unwrap_or_else(|e| panic!("mkdir: {e}"));
        std::fs::write(root.join("a.txt"), b"hello").unwrap_or_else(|e| panic!("write: {e}"));
        std::fs::write(root.join("nested/b.bin"), [0_u8, 1, 2, 3])
            .unwrap_or_else(|e| panic!("write: {e}"));
        std::fs::write(root.join("nested/deep/c.txt"), b"deep")
            .unwrap_or_else(|e| panic!("write: {e}"));
    }

    /// Read a tar stream into `name -> bytes` and `name -> link target` maps.
    fn read_tar(data: &[u8]) -> (BTreeMap<String, Vec<u8>>, BTreeMap<String, String>) {
        let mut files = BTreeMap::new();
        let mut links = BTreeMap::new();
        let mut archive = tar::Archive::new(io::Cursor::new(data));
        for entry in archive.entries().expect("entries") {
            let mut entry = entry.expect("valid entry");
            let path = entry
                .path()
                .unwrap_or_else(|e| panic!("path: {e}"))
                .display()
                .to_string();
            let kind = entry.header().entry_type();
            if kind.is_symlink() {
                let target = entry
                    .link_name()
                    .unwrap_or_else(|e| panic!("link: {e}"))
                    .unwrap_or_default();
                links.insert(path, target.display().to_string());
            } else if kind.is_dir() {
                files.insert(format!("{path}/"), Vec::new());
            } else {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf).expect("read entry");
                files.insert(path, buf);
            }
        }
        (files, links)
    }

    fn plain() -> ArchiveOptions {
        ArchiveOptions {
            compression: Compression::None,
            follow_symlinks: false,
        }
    }

    #[test]
    fn none_compression_round_trips_a_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("data");
        build_tree(&root);

        let mut out = Vec::new();
        let stats = write_archive(&[Source::from_path(&root)], &mut out, plain()).expect("archive");

        let (files, _) = read_tar(&out);
        assert_eq!(
            files.get("data/a.txt").map(Vec::as_slice),
            Some(&b"hello"[..])
        );
        assert_eq!(
            files.get("data/nested/b.bin").map(Vec::as_slice),
            Some(&[0, 1, 2, 3][..])
        );
        assert_eq!(
            files.get("data/nested/deep/c.txt").map(Vec::as_slice),
            Some(&b"deep"[..])
        );
        assert!(files.contains_key("data/nested/"));
        // a.txt, b.bin, c.txt plus three directory entries.
        assert_eq!(stats.entries, 6);
        assert!(stats.uncompressed_bytes >= out.len() as u64);
    }

    #[test]
    fn entries_are_relative_to_the_source_basename() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("pgdata");
        build_tree(&root);

        let mut out = Vec::new();
        write_archive(&[Source::from_path(&root)], &mut out, plain()).expect("archive");

        let (files, links) = read_tar(&out);
        for name in files.keys().chain(links.keys()) {
            assert!(name.starts_with("pgdata/"), "unexpected entry {name}");
            assert!(!name.starts_with('/'), "absolute entry {name}");
            assert!(
                !name.split('/').any(|part| part == ".."),
                "escaping entry {name}"
            );
        }
    }

    #[test]
    fn multiple_sources_share_one_archive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let one = tmp.path().join("one");
        let two = tmp.path().join("two");
        build_tree(&one);
        std::fs::create_dir_all(&two).unwrap_or_else(|e| panic!("mkdir: {e}"));
        std::fs::write(two.join("x.txt"), b"x").unwrap_or_else(|e| panic!("write: {e}"));

        let mut out = Vec::new();
        write_archive(
            &[Source::from_path(&one), Source::from_path(&two)],
            &mut out,
            plain(),
        )
        .expect("archive");

        let (files, _) = read_tar(&out);
        assert!(files.contains_key("one/a.txt"));
        assert!(files.contains_key("two/x.txt"));
    }

    #[test]
    fn symlinks_are_stored_as_symlinks_by_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("data");
        build_tree(&root);
        std::os::unix::fs::symlink("a.txt", root.join("link"))
            .unwrap_or_else(|e| panic!("symlink: {e}"));

        let mut out = Vec::new();
        write_archive(&[Source::from_path(&root)], &mut out, plain()).expect("archive");

        let (files, links) = read_tar(&out);
        assert_eq!(links.get("data/link").map(String::as_str), Some("a.txt"));
        assert!(files.contains_key("data/a.txt"));
    }

    #[test]
    fn follow_symlinks_stores_the_target_contents() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("data");
        build_tree(&root);
        std::os::unix::fs::symlink("a.txt", root.join("link"))
            .unwrap_or_else(|e| panic!("symlink: {e}"));

        let mut out = Vec::new();
        write_archive(
            &[Source::from_path(&root)],
            &mut out,
            ArchiveOptions {
                compression: Compression::None,
                follow_symlinks: true,
            },
        )
        .expect("archive");

        let (files, links) = read_tar(&out);
        assert!(
            links.is_empty(),
            "expected no symlink entries, got {links:?}"
        );
        assert_eq!(
            files.get("data/link").map(Vec::as_slice),
            Some(&b"hello"[..])
        );
    }

    #[test]
    fn permissions_and_mtime_are_preserved() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("data");
        std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        let file = root.join("script.sh");
        std::fs::write(&file, b"#!/bin/sh\n").unwrap_or_else(|e| panic!("write: {e}"));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o750))
            .unwrap_or_else(|e| panic!("chmod: {e}"));

        let mut out = Vec::new();
        write_archive(&[Source::from_path(&root)], &mut out, plain()).expect("archive");

        let mut archive = tar::Archive::new(io::Cursor::new(out));
        let mut entries = archive.entries().expect("entries");
        let dir = entries.next().expect("dir entry").expect("valid entry");
        assert!(dir.header().entry_type().is_dir());
        let file = entries.next().expect("file entry").expect("valid entry");
        assert_eq!(file.header().mode().unwrap_or(0) & 0o777, 0o750);
        assert!(file.header().mtime().unwrap_or(0) > 0);
    }

    #[test]
    fn zstd_and_gzip_are_readable_by_their_decoders() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("data");
        build_tree(&root);

        let mut zst = Vec::new();
        write_archive(
            &[Source::from_path(&root)],
            &mut zst,
            ArchiveOptions {
                compression: Compression::Zstd,
                follow_symlinks: false,
            },
        )
        .expect("zstd archive");
        let decoded = zstd::stream::decode_all(io::Cursor::new(&zst)).expect("zstd decode");
        let (files, _) = read_tar(&decoded);
        assert_eq!(
            files.get("data/a.txt").map(Vec::as_slice),
            Some(&b"hello"[..])
        );

        let mut gz = Vec::new();
        write_archive(
            &[Source::from_path(&root)],
            &mut gz,
            ArchiveOptions {
                compression: Compression::Gzip,
                follow_symlinks: false,
            },
        )
        .expect("gzip archive");
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(io::Cursor::new(&gz))
            .read_to_end(&mut decoded)
            .expect("gzip decode");
        let (files, _) = read_tar(&decoded);
        assert_eq!(
            files.get("data/a.txt").map(Vec::as_slice),
            Some(&b"hello"[..])
        );
    }

    #[test]
    fn compression_actually_shrinks_repetitive_data() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("data");
        std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        std::fs::write(root.join("big.txt"), vec![7_u8; 512 * 1024])
            .unwrap_or_else(|e| panic!("write: {e}"));

        let mut plain_out = Vec::new();
        write_archive(&[Source::from_path(&root)], &mut plain_out, plain()).expect("plain");
        let mut zst = Vec::new();
        write_archive(
            &[Source::from_path(&root)],
            &mut zst,
            ArchiveOptions {
                compression: Compression::Zstd,
                follow_symlinks: false,
            },
        )
        .expect("zstd");

        assert!(
            zst.len() < plain_out.len() / 10,
            "zst={} plain={}",
            zst.len(),
            plain_out.len()
        );
    }

    #[test]
    fn a_missing_source_fails_the_archive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist");

        let mut out = Vec::new();
        let err = write_archive(
            &[Source::from_path(&missing)],
            &mut out,
            ArchiveOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Archive { .. }), "got {err:?}");
    }

    #[test]
    fn a_broken_symlink_target_fails_the_archive_when_following() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("data");
        std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        std::fs::write(root.join("a.txt"), b"fine").unwrap_or_else(|e| panic!("write: {e}"));
        // A symlink whose target does not exist: reading it fails once we follow it.
        std::os::unix::fs::symlink("gone.txt", root.join("dangling"))
            .unwrap_or_else(|e| panic!("symlink: {e}"));

        let mut out = Vec::new();
        let err = write_archive(
            &[Source::from_path(&root)],
            &mut out,
            ArchiveOptions {
                compression: Compression::None,
                follow_symlinks: true,
            },
        )
        .unwrap_err();

        assert!(matches!(err, Error::Archive { .. }), "got {err:?}");
        assert!(
            err.to_string().contains("dangling"),
            "error should name the offending entry: {err}"
        );
    }

    #[test]
    fn dangling_symlinks_are_stored_as_links_when_not_following() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("data");
        std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        std::os::unix::fs::symlink("gone.txt", root.join("dangling"))
            .unwrap_or_else(|e| panic!("symlink: {e}"));

        let mut out = Vec::new();
        write_archive(&[Source::from_path(&root)], &mut out, plain()).expect("archive");
        let (_, links) = read_tar(&out);
        assert_eq!(
            links.get("data/dangling").map(String::as_str),
            Some("gone.txt")
        );
    }

    #[test]
    fn directories_with_many_entries_are_handled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("data");
        std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        for i in 0..64 {
            std::fs::write(root.join(format!("f{i:03}.txt")), b"x")
                .unwrap_or_else(|e| panic!("write: {e}"));
        }

        let mut out = Vec::new();
        let stats = write_archive(&[Source::from_path(&root)], &mut out, plain()).expect("archive");
        let (files, _) = read_tar(&out);
        assert_eq!(files.len(), 65, "root dir plus 64 files");
        assert_eq!(stats.entries, 65);
    }

    #[tokio::test]
    async fn stream_to_pipes_through_to_the_sink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("data");
        build_tree(&root);

        let mut sink: Vec<u8> = Vec::new();
        let stats = stream_to(vec![Source::from_path(&root)], &mut sink, plain())
            .await
            .expect("stream");

        assert!(stats.compressed_bytes > 0);
        let (files, _) = read_tar(&sink);
        assert!(files.contains_key("data/a.txt"));
    }

    #[tokio::test]
    async fn stream_to_propagates_source_errors() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("nope");

        let mut sink: Vec<u8> = Vec::new();
        let err = stream_to(
            vec![Source::from_path(&missing)],
            &mut sink,
            ArchiveOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::Archive { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn stream_to_streams_8_mib_through_a_small_pipe() {
        // 8 MiB through a 512 KiB pipe: buffering the whole archive would need
        // 8 MiB up front, which the pipe makes impossible.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("data");
        std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        std::fs::write(root.join("big.bin"), vec![3_u8; 8 * 1024 * 1024])
            .unwrap_or_else(|e| panic!("write: {e}"));

        let mut sink = CountingSink::default();
        let stats = stream_to(
            vec![Source::from_path(&root)],
            &mut sink,
            ArchiveOptions::default(),
        )
        .await
        .expect("stream");
        assert!(stats.compressed_bytes > 0);
        assert_eq!(sink.total, stats.compressed_bytes);
    }

    #[tokio::test]
    async fn stream_to_surfaces_sink_failures() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("data");
        std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        std::fs::write(root.join("a.txt"), b"hello").unwrap_or_else(|e| panic!("write: {e}"));

        let err = stream_to(
            vec![Source::from_path(&root)],
            FailingSink,
            ArchiveOptions::default(),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, Error::Archive { .. }), "got {err:?}");
        assert!(err.to_string().contains("sink refused"), "{err}");
    }

    /// Counts bytes instead of storing them, to prove nothing is buffered whole.
    #[derive(Default)]
    struct CountingSink {
        total: u64,
    }

    impl ChunkSink for CountingSink {
        fn write_chunk(&mut self, chunk: &[u8]) -> impl std::future::Future<Output = Result<()>> {
            self.total += chunk.len() as u64;
            std::future::ready(Ok(()))
        }
    }

    /// A sink that always fails, to check the error path.
    struct FailingSink;

    impl ChunkSink for FailingSink {
        fn write_chunk(&mut self, _chunk: &[u8]) -> impl std::future::Future<Output = Result<()>> {
            std::future::ready(Err(Error::archive_other(io::Error::other("sink refused"))))
        }
    }
}
