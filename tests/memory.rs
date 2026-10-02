//! Memory boundedness of the streaming upload path.
//!
//! Two checks, no network and no Docker:
//!
//! 1. The largest chunk a sink ever receives is bounded by the pipe size. This
//!    is the structural guarantee: the archiver can only push what the duplex
//!    pipe accepts, so it cannot hand over a whole archive in one piece.
//! 2. Peak resident memory while uploading a 256 MiB archive stays far below the
//!    archive size. This is the empirical backstop for the structural claim.
//!
//! How memory is measured: `/proc/self/statm` field 2 (resident pages) times the
//! page size, sampled on a timer while the upload runs. The ceiling is loose by
//! design — it is there to catch a full-archive buffer, not to police allocator
//! behaviour.

use std::path::{Path, PathBuf};

use dvb::archive::{ArchiveOptions, ChunkSink, PIPE_BUFFER, Source};
use dvb::config::{Compression, FsConfig, JobConfig, StorageConfig};
use dvb::error::Result;
use dvb::storage::{self, WriterSink};

/// Source size for the resident-memory case.
const SOURCE_MIB: u64 = 256;

/// Resident memory ceiling for that case, in MiB.
const CEILING_MIB: u64 = 256;

/// Page size, used to convert `/proc/self/statm` pages into bytes.
///
/// 4096 on every platform this runs on; a wrong value would only scale the
/// reading, and the assertion is orders of magnitude away from the boundary.
const PAGE_SIZE: u64 = 4096;

/// Resident set size of this process in bytes.
fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * PAGE_SIZE)
}

/// Write `mib` of incompressible bytes into `dir/data`.
///
/// Incompressible on purpose: a repetitive pattern would be compressed away and
/// the upload would never reach the sizes under test.
fn build_source(dir: &Path, mib: u64) -> std::io::Result<()> {
    let root = dir.join("data");
    std::fs::create_dir_all(&root)?;

    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let per_file = 8 * 1024 * 1024_usize;
    let files = (mib * 1024 * 1024 / per_file as u64).max(1);

    for index in 0..files {
        let mut block = Vec::with_capacity(per_file);
        while block.len() < per_file {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            block.extend_from_slice(&state.to_le_bytes());
        }
        block.truncate(per_file);
        std::fs::write(root.join(format!("part-{index:04}.bin")), block)?;
    }
    Ok(())
}

/// A job archiving `source` into `root` with no compression.
fn job(source: PathBuf, root: PathBuf) -> JobConfig {
    JobConfig {
        name: "big".to_owned(),
        cron: None,
        source: vec![source],
        filename: "data-%Y%m%dT%H%M%SZ.tar".to_owned(),
        compression: Compression::None,
        retention_days: 14,
        min_keep: 1,
        stop_containers: Vec::new(),
        stop_label: None,
        stop_timeout_secs: 30,
        follow_symlinks: false,
        storage: StorageConfig::Fs(FsConfig {
            root,
            prefix: "big".to_owned(),
        }),
        pre: Vec::new(),
        post: Vec::new(),
        run_on_start: false,
    }
}

/// Forwards to the real writer while recording the largest chunk seen.
struct MeasuringSink<'a> {
    inner: &'a mut WriterSink,
    largest: &'a mut u64,
}

impl ChunkSink for MeasuringSink<'_> {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        *self.largest = (*self.largest).max(chunk.len() as u64);
        self.inner.write_chunk(chunk).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_chunk_larger_than_the_pipe_reaches_the_sink() {
    let tmp = tempfile::tempdir().expect("tempdir");
    build_source(tmp.path(), 64).expect("build source");

    let job = job(tmp.path().join("data"), tmp.path().join("remote"));
    let op = storage::operator(&job.storage).expect("operator");

    let writer = storage::new_writer(&op, "fs", &job.storage.remote_path("chunked.tar"))
        .await
        .expect("writer");
    let mut sink = WriterSink::new(writer, "fs");
    let mut largest = 0_u64;
    let stats = {
        let mut measuring = MeasuringSink {
            inner: &mut sink,
            largest: &mut largest,
        };
        dvb::archive::stream_to(
            vec![Source::from_path(tmp.path().join("data"))],
            &mut measuring,
            ArchiveOptions {
                compression: Compression::None,
                follow_symlinks: false,
            },
        )
        .await
        .expect("stream")
    };

    let uploaded = sink.close().await.expect("close").content_length();
    assert!(uploaded > 0, "nothing was uploaded");
    assert_eq!(
        stats.compressed_bytes, uploaded,
        "stats disagree with the object"
    );
    assert!(
        largest <= PIPE_BUFFER as u64,
        "the sink saw a {largest}-byte chunk, larger than the {PIPE_BUFFER} byte pipe"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploading_a_large_archive_does_not_buffer_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    build_source(tmp.path(), SOURCE_MIB).expect("build source");

    let job = job(tmp.path().join("data"), tmp.path().join("remote"));
    let op = storage::operator(&job.storage).expect("operator");

    let writer = storage::new_writer(&op, "fs", &job.storage.remote_path("large.tar"))
        .await
        .expect("writer");
    let mut sink = WriterSink::new(writer, "fs");

    let baseline = resident_bytes().unwrap_or(0);
    let mut peak = baseline;
    let mut samples = 0_u32;

    // Scoped so the borrow of `sink` ends before `close` consumes it.
    {
        let stream = dvb::archive::stream_to(
            vec![Source::from_path(tmp.path().join("data"))],
            &mut sink,
            ArchiveOptions {
                compression: Compression::None,
                follow_symlinks: false,
            },
        );
        tokio::pin!(stream);

        loop {
            tokio::select! {
                result = &mut stream => {
                    result.expect("stream");
                    break;
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(25)) => {
                    samples += 1;
                    if let Some(rss) = resident_bytes() {
                        peak = peak.max(rss);
                    }
                }
            }
        }
    }

    let uploaded = sink.close().await.expect("close").content_length();
    assert!(
        uploaded > SOURCE_MIB * 1024 * 1024 / 2,
        "expected a large upload, got {uploaded} bytes"
    );
    assert!(
        samples > 0,
        "the upload finished before RSS could be sampled; use a bigger source"
    );

    let peak_mib = peak / (1024 * 1024);
    assert!(
        peak_mib < CEILING_MIB,
        "peak RSS {peak_mib} MiB (baseline {} MiB) exceeds {CEILING_MIB} MiB while \
         uploading {uploaded} bytes: the archive is being buffered, not streamed",
        baseline / (1024 * 1024)
    );
}
