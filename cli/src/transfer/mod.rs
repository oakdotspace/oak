//! Content transfer: one chunk mapping, one uploader.
//!
//! Every path that sends blob content to a server — ordinary push, headless
//! blob preload, phase-one bootstrap and staged publication — goes through
//! this module:
//!
//! - [`map_blob`] / [`map_blob_reader`] are the only way the CLI turns blob
//!   bytes into a chunk mapping. They are thin wire-typed wrappers over
//!   [`oak_core::blob_chunk_mapping`], so a blob's mapping is a pure function
//!   of its bytes whichever transport carries it.
//! - [`ChunkUploader`] is the only chunk upload loop: have/want negotiation
//!   (`chunks/check`), bounded-concurrency uploads, and incremental durable
//!   acknowledgement (`chunks/uploaded` as uploads finish), so an interrupted
//!   transfer resumes with only the chunks the server never acknowledged.
//! - [`TransferSession`] carries the bounded upload resources, the observed
//!   counts a receipt reports, and progress output for one operation.

mod uploader;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use oak_core::protocol::ChunkRefData;
use oak_core::{OakError, Result};

pub(crate) use uploader::{ChunkBusyRetryState, ChunkUploader};

/// Default number of concurrent chunk uploads.
///
/// Each upload to R2 runs on its own connection and tends to be throttled
/// per-connection (bandwidth-delay-product limited over a high-RTT path), so
/// aggregate throughput scales roughly linearly with concurrency until the
/// link's real ceiling — a single 1 MB-chunk stream leaves a fast uplink idle.
/// Override with `OAK_UPLOAD_CONCURRENCY` for very fast or very slow links.
const DEFAULT_CONCURRENT_TRANSFERS: usize = 32;

/// How often a long transfer reports progress on stderr.
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Resolve the chunk-upload concurrency, honoring `OAK_UPLOAD_CONCURRENCY`.
fn max_concurrent_transfers() -> usize {
    std::env::var("OAK_UPLOAD_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_CONCURRENT_TRANSFERS)
}

/// The size-aware ceiling for transferring `declared_bytes`.
pub(crate) fn size_aware_timeout(declared_bytes: u64) -> std::time::Duration {
    // One MiB/s is deliberately conservative enough for a congested agent or
    // busy server, while the 23-hour ceiling leaves an hour to abort or
    // finalize before the staged session's 24-hour expiry.
    let transfer_seconds = declared_bytes.div_ceil(1024 * 1024);
    std::time::Duration::from_secs((15 * 60 + transfer_seconds).clamp(60 * 60, 23 * 60 * 60))
}

fn chunk_ref(info: &oak_core::ChunkInfo) -> ChunkRefData {
    ChunkRefData {
        hash: info.hash.to_string(),
        offset: info.offset,
        size: info.length,
    }
}

/// The chunk mapping of `size` bytes read from `reader`, with each chunk's
/// bytes. See [`oak_core::blob_chunk_mapping`] for the mapping itself.
pub(crate) fn map_blob_reader<R: std::io::Read>(
    size: u64,
    reader: R,
) -> impl Iterator<Item = Result<(ChunkRefData, Vec<u8>)>> {
    oak_core::blob_chunk_mapping(size, reader).map(|chunk| {
        let (info, bytes) = chunk.map_err(OakError::Io)?;
        Ok((chunk_ref(&info), bytes))
    })
}

/// The chunk mapping of an in-memory blob.
pub(crate) fn map_blob(content: &[u8]) -> Result<Vec<(ChunkRefData, Vec<u8>)>> {
    map_blob_reader(content.len() as u64, content).collect()
}

/// Whether a blob of `size` bytes is carried as content-defined chunks (and
/// so can never travel inline or as one whole-blob chunk).
pub(crate) fn is_content_defined(size: u64) -> bool {
    size >= oak_core::LARGE_FILE_THRESHOLD
}

/// What one operation observed while transferring content. Every number is
/// set from a server acknowledgement, never from intent.
///
/// The counts cover the chunks of blobs the server reported missing. A blob
/// small enough to travel inline in a publication request is its own single
/// chunk and is counted once that request is acknowledged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct TransferCounts {
    /// Unique chunks this operation needed the server to hold (a chunk
    /// shared by several blobs or batches counts once).
    pub chunks_required: u64,
    /// Of those, chunks the server already held (nothing was sent).
    pub chunks_already_present: u64,
    /// Chunks uploaded and acknowledged by the server.
    pub chunks_uploaded: u64,
    /// Content bytes (before transport compression) of the uploaded chunks.
    pub bytes_uploaded: u64,
}

struct Progress {
    started: std::time::Instant,
    last: std::time::Instant,
    /// The last line printed, without its elapsed-time suffix: an unchanged
    /// line is never printed twice.
    last_line: Option<String>,
}

/// Where one chunk stands within a transfer session.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    /// One uploader of the session is negotiating or uploading it.
    InFlight,
    /// The server holds it: reported present, or uploaded and acknowledged.
    Done,
    /// The uploader that owned it failed; it may be claimed again.
    Failed,
}

/// Outcome of asking the session for a chunk.
pub(super) enum Claim {
    /// The caller owns the chunk and must negotiate/upload it.
    Mine,
    /// The server already holds it (settled earlier in this session).
    Done,
    /// Another uploader of this session owns it; wait for it to settle.
    Elsewhere,
}

/// Shared state of one content-transfer operation (one `oak push`): the
/// bounded upload resources, the have/want/in-flight chunk set, the
/// observed counts and the progress line.
///
/// The chunk set is what makes "a chunk is negotiated, uploaded and
/// acknowledged at most once per operation" true even when several uploaders
/// (concurrent preload batches) need the same chunk.
pub(crate) struct TransferSession {
    required: AtomicU64,
    already_present: AtomicU64,
    uploaded: AtomicU64,
    bytes: AtomicU64,
    concurrency: usize,
    upload_slots: Arc<tokio::sync::Semaphore>,
    /// HTTP/1.1 pool for chunk bytes.
    upload_client: reqwest::Client,
    /// Own HTTP/1.1 connections for `chunks/check` and `chunks/uploaded`,
    /// so an acknowledgement never queues behind large request bodies
    /// (preload requests) on the shared HTTP/2 API connection.
    control_client: reqwest::Client,
    chunks: Mutex<std::collections::HashMap<String, ChunkState>>,
    chunk_settled: tokio::sync::Notify,
    progress: Mutex<Progress>,
}

impl TransferSession {
    pub(crate) fn new() -> Arc<Self> {
        let concurrency = max_concurrent_transfers();
        let now = std::time::Instant::now();
        Arc::new(Self {
            required: AtomicU64::new(0),
            already_present: AtomicU64::new(0),
            uploaded: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            concurrency,
            upload_slots: Arc::new(tokio::sync::Semaphore::new(concurrency)),
            upload_client: uploader::upload_client(concurrency),
            control_client: crate::http::dedicated_http1_api_client(4),
            chunks: Mutex::new(std::collections::HashMap::new()),
            chunk_settled: tokio::sync::Notify::new(),
            progress: Mutex::new(Progress {
                started: now,
                last: now,
                last_line: None,
            }),
        })
    }

    pub(crate) fn counts(&self) -> TransferCounts {
        TransferCounts {
            chunks_required: self.required.load(Ordering::Relaxed),
            chunks_already_present: self.already_present.load(Ordering::Relaxed),
            chunks_uploaded: self.uploaded.load(Ordering::Relaxed),
            bytes_uploaded: self.bytes.load(Ordering::Relaxed),
        }
    }

    /// Ask for `hash`: the first caller owns it, later callers learn that it
    /// is settled or owned elsewhere.
    fn claim(&self, hash: &str) -> Claim {
        let mut chunks = self.chunks.lock().expect("transfer chunk set poisoned");
        match chunks.get(hash) {
            Some(ChunkState::Done) => Claim::Done,
            Some(ChunkState::InFlight) => Claim::Elsewhere,
            Some(ChunkState::Failed) | None => {
                chunks.insert(hash.to_string(), ChunkState::InFlight);
                Claim::Mine
            }
        }
    }

    /// The owner of `hash` reports that the server holds it (`true`) or that
    /// its upload failed.
    fn settle(&self, hash: &str, held: bool) {
        let state = if held {
            ChunkState::Done
        } else {
            ChunkState::Failed
        };
        self.chunks
            .lock()
            .expect("transfer chunk set poisoned")
            .insert(hash.to_string(), state);
        self.chunk_settled.notify_waiters();
    }

    /// Wait until every chunk in `hashes` (owned by other uploaders of this
    /// session) is held by the server; fail as soon as one of them failed.
    async fn wait_for(&self, hashes: &[String]) -> Result<()> {
        loop {
            let settled = self.chunk_settled.notified();
            tokio::pin!(settled);
            // Register before inspecting, so a settle in between is not lost.
            settled.as_mut().enable();
            {
                let chunks = self.chunks.lock().expect("transfer chunk set poisoned");
                let mut pending = false;
                for hash in hashes {
                    match chunks.get(hash.as_str()) {
                        Some(ChunkState::Done) => {}
                        Some(ChunkState::InFlight) => pending = true,
                        Some(ChunkState::Failed) | None => {
                            return Err(OakError::Server(format!(
                                "the upload of chunk {hash}, shared with another part of this push, failed; retry the push"
                            )));
                        }
                    }
                }
                if !pending {
                    return Ok(());
                }
            }
            settled.await;
        }
    }

    fn record_negotiated(&self, required: u64, already_present: u64) {
        self.required.fetch_add(required, Ordering::Relaxed);
        self.already_present
            .fetch_add(already_present, Ordering::Relaxed);
    }

    fn record_uploaded(&self, chunks: u64, bytes: u64) {
        self.uploaded.fetch_add(chunks, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        self.report_progress(false);
    }

    /// Blobs that travelled inline in an acknowledged publication or preload
    /// request: each is its own single chunk, required and uploaded.
    pub(crate) fn record_inline_blobs(&self, blobs: u64, bytes: u64) {
        self.required.fetch_add(blobs, Ordering::Relaxed);
        self.record_uploaded(blobs, bytes);
    }

    /// Print `line` as progress unless it is not yet due (`force` overrides
    /// the interval) or is the line printed last. Returns whether the line
    /// was handed to the output layer.
    fn emit_progress(&self, line: String, force: bool) -> bool {
        let mut progress = self
            .progress
            .lock()
            .expect("transfer progress lock poisoned");
        let now = std::time::Instant::now();
        if (!force && now.duration_since(progress.last) < PROGRESS_INTERVAL)
            || progress.last_line.as_deref() == Some(line.as_str())
        {
            return false;
        }
        progress.last = now;
        let elapsed = now.duration_since(progress.started).as_secs_f64();
        progress.last_line = Some(line.clone());
        drop(progress);
        crate::output::progress_line(&format!("  {line} ({elapsed:.0}s)"));
        true
    }

    /// The periodic chunk progress line.
    fn report_progress(&self, force: bool) -> bool {
        self.emit_progress(self.summary(), force)
    }

    /// One closing progress line for a transfer that already reported
    /// progress, so its last line states the final counts — unless that is
    /// what the last line already said.
    pub(crate) fn finish_progress(&self) {
        let reported = self
            .progress
            .lock()
            .expect("transfer progress lock poisoned")
            .last_line
            .is_some();
        if reported {
            self.report_progress(true);
        }
    }

    /// Human summary of the observed counts.
    pub(crate) fn summary(&self) -> String {
        let counts = self.counts();
        format!(
            "Uploaded {} chunk(s), {}; {} of {} already on server",
            counts.chunks_uploaded,
            human_bytes(counts.bytes_uploaded),
            counts.chunks_already_present,
            counts.chunks_required,
        )
    }

    /// A phase-level progress line (for example a finished preload request),
    /// rate-limited like the chunk progress.
    pub(crate) fn report_phase(&self, message: &str) {
        self.emit_progress(message.to_string(), false);
    }
}

fn human_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random(len: usize, mut state: u32) -> Vec<u8> {
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state & 0xFF) as u8
            })
            .collect()
    }

    #[test]
    fn the_wire_mapping_is_the_core_mapping() {
        for len in [
            0usize,
            1,
            300 * 1024,
            oak_core::LARGE_FILE_THRESHOLD as usize - 1,
            oak_core::LARGE_FILE_THRESHOLD as usize,
            oak_core::LARGE_FILE_THRESHOLD as usize + 3 * 1024 * 1024,
        ] {
            let content = pseudo_random(len, 0x1234_5678);
            let wire = map_blob(&content).unwrap();
            let core: Vec<_> = oak_core::blob_chunk_mapping(len as u64, content.as_slice())
                .collect::<std::io::Result<_>>()
                .unwrap();
            assert_eq!(wire.len(), core.len());
            let mut rebuilt = Vec::new();
            for ((chunk, bytes), (info, _)) in wire.iter().zip(&core) {
                assert_eq!(chunk.hash, info.hash.to_string());
                assert_eq!(chunk.offset, info.offset);
                assert_eq!(chunk.size, info.length);
                rebuilt.extend_from_slice(bytes);
            }
            assert_eq!(rebuilt, content);
            assert_eq!(
                is_content_defined(len as u64),
                len as u64 >= oak_core::LARGE_FILE_THRESHOLD
            );
            if is_content_defined(len as u64) {
                assert!(wire.len() > 1, "a content-defined blob is never one chunk");
            } else {
                assert!(wire.len() <= 1, "a small blob is its own single chunk");
            }
        }
    }

    /// Every non-comment source line under `root` (recursively), as
    /// `(path relative to root, line number, code)`.
    fn source_lines(root: &std::path::Path) -> Vec<(String, usize, String)> {
        let mut lines = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                    continue;
                }
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let text = std::fs::read_to_string(&path).unwrap();
                for (index, line) in text.lines().enumerate() {
                    // A comment starts at the first `//` that is not part
                    // of a URL scheme inside a string (`https://…`).
                    let comment = line
                        .match_indices("//")
                        .map(|(at, _)| at)
                        .find(|at| !line[..*at].ends_with(':'));
                    let code = &line[..comment.unwrap_or(line.len())];
                    if !code.trim().is_empty() {
                        lines.push((relative.clone(), index + 1, code.to_string()));
                    }
                }
            }
        }
        lines
    }

    /// Whether `code` mentions `identifier` as a whole identifier — a call,
    /// a path segment, a `use` (with or without `as`), or a bare function
    /// value alike.
    fn mentions_identifier(code: &str, identifier: &str) -> bool {
        let is_ident = |c: char| c.is_alphanumeric() || c == '_';
        code.match_indices(identifier).any(|(at, _)| {
            !code[..at].chars().next_back().is_some_and(is_ident)
                && !code[at + identifier.len()..]
                    .chars()
                    .next()
                    .is_some_and(is_ident)
        })
    }

    /// Mechanical enforcement of "one chunk mapping": no source of the CLI
    /// outside this module, and no source of `oak_core` outside
    /// `chunking.rs` (and its re-export), may so much as name a chunking
    /// primitive. The primitives stay public in `oak_core` because the
    /// hosted server calls them, so visibility cannot enforce this; naming
    /// one anywhere else — in a call, a `use … as`, or as a function value —
    /// is how a transport-specific chunk shape would come back.
    #[test]
    fn no_source_names_a_chunking_primitive_outside_the_one_mapping() {
        const PRIMITIVES: [&str; 7] = [
            "chunk_content",
            "stream_chunk_content",
            "blob_chunk_mapping",
            "BlobChunkMapping",
            "FastCDC",
            "StreamCDC",
            "fastcdc",
        ];
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offenders = Vec::new();
        for (path, line, code) in source_lines(&manifest.join("src")) {
            if path.starts_with("transfer/") {
                continue;
            }
            if PRIMITIVES
                .iter()
                .any(|name| mentions_identifier(&code, name))
            {
                offenders.push(format!("cli/src/{path}:{line}: {}", code.trim()));
            }
        }
        let mut in_reexport = false;
        for (path, line, code) in source_lines(&manifest.join("../core/src")) {
            if path == "chunking.rs" {
                continue;
            }
            // The one place the names may appear: `pub use chunking::{…};`.
            if path == "lib.rs" && code.trim_start().starts_with("pub use chunking::") {
                in_reexport = true;
            }
            let exempt = in_reexport;
            if in_reexport && code.contains(';') {
                in_reexport = false;
            }
            // The re-export may name the primitives, never rename one: an
            // alias (`stream_chunk_content as split_stream`) would give the
            // primitive a name this scan does not know.
            if exempt && reexport_renames(&code) {
                offenders.push(format!(
                    "core/src/{path}:{line}: the chunking re-export renames a primitive: {}",
                    code.trim()
                ));
            }
            if !exempt
                && PRIMITIVES
                    .iter()
                    .any(|name| mentions_identifier(&code, name))
            {
                offenders.push(format!("core/src/{path}:{line}: {}", code.trim()));
            }
        }
        assert!(
            offenders.is_empty(),
            "blob bytes must be chunked through crate::transfer::map_blob / map_blob_reader only:\n{}",
            offenders.join("\n")
        );
    }

    /// Whether a line of a `use` list renames an item (`x as y`).
    fn reexport_renames(code: &str) -> bool {
        mentions_identifier(code, "as")
    }

    #[test]
    fn a_renaming_chunking_reexport_is_seen() {
        assert!(reexport_renames(
            "    blob_chunk_mapping, chunk_content, stream_chunk_content as split_stream,"
        ));
        assert!(reexport_renames(
            "pub use chunking::{chunk_content as cc, reassemble_chunks};"
        ));
        assert!(!reexport_renames(
            "    blob_chunk_mapping, chunk_content, reassemble_chunks, stream_chunk_content, BlobChunkMapping,"
        ));
        assert!(!reexport_renames("    ChunkInfo, BaseAssets,"));
    }

    #[test]
    fn the_identifier_scan_sees_aliases_and_function_values() {
        for code in [
            "let chunks = oak_core::chunk_content(&data);",
            "use oak_core::chunk_content as cc;",
            "let f = oak_core::chunk_content;",
            "use oak_core::{hash_bytes, stream_chunk_content};",
            "use fastcdc::v2020::FastCDC;",
        ] {
            assert!(
                ["chunk_content", "stream_chunk_content", "FastCDC"]
                    .iter()
                    .any(|name| mentions_identifier(code, name)),
                "missed: {code}"
            );
        }
        for code in [
            "let rechunk_content = 1;",
            "let chunk_contents = 2;",
            "stream_chunk_content_v2()",
        ] {
            assert!(!mentions_identifier(code, "chunk_content"), "{code}");
            assert!(!mentions_identifier(code, "stream_chunk_content"), "{code}");
        }
    }

    /// Mechanical enforcement of "one uploader": the chunk negotiation,
    /// upload and acknowledgement endpoints may be spoken only by this
    /// module. A second call site anywhere else in the CLI is a second
    /// upload loop.
    #[test]
    fn only_the_transfer_module_speaks_the_chunk_upload_endpoints() {
        const ENDPOINTS: [&str; 4] = [
            "chunks/check",
            "chunks/uploaded",
            "chunks/batch",
            // `PUT …/chunks/{hash}` (server-proxied upload).
            "/chunks/{",
        ];
        // (file, reason it may contain these paths)
        const ALLOWED: [(&str, &str); 1] = [(
            "commands/pull.rs",
            "chunk download: the CDN object path `ws/{org}/chunks/{hash}` is a GET, not an upload",
        )];
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let offenders: Vec<String> = source_lines(&src)
            .into_iter()
            .filter(|(path, _, code)| {
                !path.starts_with("transfer/")
                    && !ALLOWED.iter().any(|(allowed, _)| path == allowed)
                    && ENDPOINTS.iter().any(|endpoint| code.contains(endpoint))
            })
            .map(|(path, line, code)| format!("cli/src/{path}:{line}: {}", code.trim()))
            .collect();
        assert!(
            offenders.is_empty(),
            "chunk upload endpoints may only be called through crate::transfer::ChunkUploader:\n{}",
            offenders.join("\n")
        );
        // The allowlist must not rot: every entry still needs its exemption.
        for (allowed, reason) in ALLOWED {
            assert!(
                source_lines(&src)
                    .iter()
                    .any(|(path, _, code)| path == allowed
                        && ENDPOINTS.iter().any(|endpoint| code.contains(endpoint))),
                "{allowed} no longer needs its exemption ({reason})"
            );
        }
    }

    #[test]
    fn an_unchanged_progress_line_is_never_printed_twice() {
        let session = TransferSession::new();
        session.record_negotiated(4, 1);
        assert!(session.report_progress(true));
        // Same counts: neither a forced report nor the closing line repeats it.
        assert!(!session.report_progress(true));
        session.finish_progress();
        assert!(!session.report_progress(true));
        // New counts print again; a non-forced line inside the interval waits.
        session.uploaded.fetch_add(1, Ordering::Relaxed);
        assert!(!session.report_progress(false));
        assert!(session.report_progress(true));
    }

    #[test]
    fn counts_report_only_what_was_recorded() {
        let session = TransferSession::new();
        assert_eq!(session.counts(), TransferCounts::default());
        session.record_negotiated(5, 2);
        session.record_uploaded(3, 300);
        session.record_inline_blobs(2, 20);
        assert_eq!(
            session.counts(),
            TransferCounts {
                chunks_required: 7,
                chunks_already_present: 2,
                chunks_uploaded: 5,
                bytes_uploaded: 320,
            }
        );
    }
}
