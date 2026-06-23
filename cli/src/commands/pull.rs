use std::path::Path;
use std::sync::Arc;

use oak_core::{
    reassemble_chunks, Blob, Branch, BranchStatus, ChangeType, ChunkInfo, CloseReason, Commit,
    FileChange, Hash, Manifest, MetadataKey, OakError, Result,
};
use oak_core::{Repository, SqliteRepository};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::materialize::{apply_manifest, ApplyOpts, DeleteScope, MissingBlobs};
use crate::output;
use crate::workdir_lock::WorkdirLock;

/// Default number of concurrent chunk downloads.
///
/// Symmetric to uploads: each chunk is a separate GET whose throughput is
/// per-connection limited, and the default `reqwest::Client` would multiplex
/// every concurrent GET onto a single HTTP/2 connection whose ~64 KB
/// flow-control window caps aggregate throughput. A repo with thousands of
/// small files clones as thousands of tiny per-chunk GETs, so concurrency
/// matters. Override with `OAK_DOWNLOAD_CONCURRENCY`.
const DEFAULT_CONCURRENT_DOWNLOADS: usize = 32;

/// Resolve the chunk-download concurrency, honoring `OAK_DOWNLOAD_CONCURRENCY`.
pub(crate) fn max_concurrent_downloads() -> usize {
    std::env::var("OAK_DOWNLOAD_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_CONCURRENT_DOWNLOADS)
}

/// HTTP/1.1 client dedicated to concurrent chunk downloads, so each download
/// gets its own pooled connection (and its own flow-control window) instead of
/// being multiplexed onto one throttled HTTP/2 connection — the download
/// counterpart of the upload client in `push.rs`.
pub(crate) fn download_client(concurrency: usize) -> reqwest::Client {
    crate::http::ensure_crypto_provider();
    reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .http1_only()
        .pool_max_idle_per_host(concurrency)
        .tcp_nodelay(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| crate::http::api_client())
}

/// Below this size a chunk is fetched via the CDN Worker's batch endpoint
/// rather than its own GET. Matches the upload-side small/large split
/// (`push.rs`) and the FastCDC minimum, so genuine CDC chunks (always larger)
/// keep their own cache-fronted GET while the many tiny self-chunk blobs batch.
pub(crate) const SMALL_CHUNK_THRESHOLD: u64 = 256 * 1024;

/// Chunks per batch-download request. Each cold chunk costs the Worker one R2
/// `BUCKET.get`, which counts against Cloudflare's per-invocation subrequest
/// limit (50 on the Workers free tier). At 64 the Worker exhausted its budget
/// mid-batch and silently dropped the overflow ("Too many subrequests by single
/// Worker invocation"), so a clone lost ~25% of every batch to the slow
/// per-chunk recovery path. Stay comfortably under the 50 cap (and the Worker's
/// `MAX_BATCH`) so a full batch fits one invocation; bump this up if the Worker
/// moves to the paid tier (1000-subrequest budget).
pub(crate) const BATCH_DOWNLOAD_SIZE: usize = 40;

/// A decoded chunk batch: `(hash, bytes)` pairs.
pub(crate) type ChunkBatch = Vec<(String, Vec<u8>)>;

/// Decode the length-framed body returned by the CDN Worker's batch endpoint:
/// `[u32 BE hash_len][hash utf8][u32 BE data_len][data]` per entry. Mirror of
/// the server/Worker encoder; tolerant of a partial trailing frame only by
/// erroring rather than panicking.
pub(crate) fn decode_chunk_batch(body: &[u8]) -> Result<ChunkBatch> {
    fn read_u32(body: &[u8], pos: usize) -> Result<usize> {
        let end = pos
            .checked_add(4)
            .filter(|&e| e <= body.len())
            .ok_or_else(|| OakError::Server("truncated chunk batch (length prefix)".into()))?;
        Ok(u32::from_be_bytes(body[pos..end].try_into().unwrap()) as usize)
    }

    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < body.len() {
        let hash_len = read_u32(body, pos)?;
        pos += 4;
        let hash_end = pos
            .checked_add(hash_len)
            .filter(|&e| e <= body.len())
            .ok_or_else(|| OakError::Server("truncated chunk batch (hash)".into()))?;
        let hash = String::from_utf8(body[pos..hash_end].to_vec())
            .map_err(|_| OakError::Server("invalid hash in chunk batch".into()))?;
        pos = hash_end;

        let data_len = read_u32(body, pos)?;
        pos += 4;
        let data_end = pos
            .checked_add(data_len)
            .filter(|&e| e <= body.len())
            .ok_or_else(|| OakError::Server("truncated chunk batch (data)".into()))?;
        out.push((hash, body[pos..data_end].to_vec()));
        pos = data_end;
    }
    Ok(out)
}

/// Fetch one chunk's raw (possibly zstd-compressed) bytes from its single-chunk
/// CDN GET URL, retrying a few times on transient failure. This is the fallback
/// path when a bulk batch request fails (e.g. the CDN Worker returns a 5xx /
/// Cloudflare error code 1101): rather than aborting the whole pull, each chunk
/// in the failed batch is fetched individually, which the Worker serves one R2
/// read at a time. Returns the raw bytes; the caller `chunk_decode`s them.
async fn fetch_chunk_via_url(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    const ATTEMPTS: usize = 3;
    let mut last_err = String::new();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(200 * attempt as u64)).await;
        }
        match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(b) => return Ok(b.to_vec()),
                Err(e) => last_err = e.to_string(),
            },
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                last_err = format!("{status}: {body}");
                // A 404 means the object is genuinely absent (not transient), so
                // retrying won't help — give up on this chunk immediately.
                if status == reqwest::StatusCode::NOT_FOUND {
                    break;
                }
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(OakError::Server(format!(
        "single-chunk fallback GET failed: {last_err}"
    )))
}

/// Commit the bulk-import transaction roughly every this many bytes of
/// chunk/blob content, so a large clone doesn't hold its entire dataset in
/// the WAL before the first commit. Flushing is cheap under
/// `synchronous=NORMAL`, so this only bounds transient WAL size; it doesn't
/// reintroduce a per-object fsync.
const BULK_FLUSH_BYTES: u64 = 64 * 1024 * 1024;

// Wire protocol types come from `oak_core::protocol` (the single source of
// truth shared with the hosted server and `oak serve`), aliased to the names
// this module has always used. `BlobData` is re-exported because `repo.rs`'s
// clone path imports it via `super::pull::BlobData` and shares
// `fetch_and_store_blobs`.
pub use oak_core::protocol::BlobData;
use oak_core::protocol::{
    BranchPullData as BranchData, BranchRenameData, ChunkDownloadResponse, CommitData,
    PullResponse, TreeData,
};

fn wire_to_core_tree(td: &TreeData) -> oak_core::Result<oak_core::Tree> {
    oak_core::protocol::tree_data_to_core(td).map_err(OakError::Database)
}

fn branch_from_pull_data(br_data: &BranchData) -> Result<Branch> {
    let status = BranchStatus::from_db_str(&br_data.status);
    let created_at = chrono::DateTime::parse_from_rfc3339(&br_data.created_at)
        .map_err(|e| OakError::Database(e.to_string()))?
        .with_timezone(&chrono::Utc);

    Ok(Branch {
        name: br_data.name.clone(),
        description: br_data.description.clone(),
        parent_branch: br_data.parent_branch.clone(),
        status,
        close_reason: br_data
            .close_reason
            .as_deref()
            .map(CloseReason::parse)
            .transpose()?,
        created_at,
    })
}

fn commit_from_pull_data(commit_data: &CommitData) -> Result<Commit> {
    let files: Vec<FileChange> = commit_data
        .files
        .iter()
        .map(|f| FileChange {
            path: f.path.clone(),
            change_type: match f.change_type.as_str() {
                "added" => ChangeType::Added,
                "deleted" => ChangeType::Deleted,
                "renamed" => ChangeType::Renamed,
                _ => ChangeType::Modified,
            },
            old_blob_hash: f.old_blob_hash.clone().map(Hash),
            new_blob_hash: f.new_blob_hash.clone().map(Hash),
            old_path: f.old_path.clone(),
            old_mode: None,
            new_mode: None,
        })
        .collect();

    let timestamp = chrono::DateTime::parse_from_rfc3339(&commit_data.timestamp)
        .map_err(|e| OakError::Database(e.to_string()))?
        .with_timezone(&chrono::Utc);

    Ok(Commit {
        hash: Hash(commit_data.hash.clone()),
        branch_name: commit_data.branch_name.clone(),
        parent_hash: commit_data.parent_hash.clone().map(Hash),
        merge_parent_hash: commit_data.merge_parent_hash.clone().map(Hash),
        manifest_hash: Hash(commit_data.manifest_hash.clone()),
        author: commit_data.author.clone(),
        message: commit_data.message.clone(),
        timestamp,
        files,
    })
}

/// Helper to add auth header to a request builder
fn with_auth(builder: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    if let Some(key) = api_key {
        builder.header("authorization", format!("Bearer {key}"))
    } else {
        builder
    }
}

/// Fetch every chunk a blob list references and materialize each blob into
/// local SQLite (writing both the `blobs` row and the `blob_chunks`
/// mapping). Shared between `oak pull` and `oak clone` so they go through
/// one wire-and-storage path.
///
/// Missing chunks are downloaded from R2 via presigned URLs (requested
/// from `/api/{owner}/{name}/chunks/download`). Chunks the server inlines
/// (no-R2 deployments) are stored directly. A blob with an empty `chunks`
/// list is rejected — the server should never advertise such a blob, and
/// silently writing empty content here was the bug that caused 256-files-
/// modified clones.
pub async fn fetch_and_store_blobs(
    repo: &SqliteRepository,
    blobs: &[BlobData],
    client: &reqwest::Client,
    remote: &str,
    owner: &str,
    name: &str,
    api_key: Option<&str>,
) -> Result<()> {
    // First, collect all missing chunk hashes across all blobs.
    let mut all_missing_hashes: Vec<String> = Vec::new();
    let mut all_missing_sizes: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();

    for blob_data in blobs {
        if blob_data.chunks.is_empty() {
            return Err(OakError::Server(format!(
                "Server returned blob {} with no chunk refs — its bytes are unreachable. \
                 The server may need to run migrate-blobs-to-r2.",
                blob_data.hash
            )));
        }
        for chunk_ref in &blob_data.chunks {
            let chunk_hash = Hash(chunk_ref.hash.clone());
            if !chunk_is_present_and_valid(repo, &chunk_hash)?
                && !all_missing_sizes.contains_key(&chunk_ref.hash)
            {
                all_missing_hashes.push(chunk_ref.hash.clone());
                all_missing_sizes.insert(chunk_ref.hash.clone(), chunk_ref.size);
            }
        }
    }

    // Batch every per-object write below into one relaxed-durability
    // transaction. Without it each store_chunk / store_blob /
    // store_blob_chunks is its own fsync'd commit (synchronous=FULL in WAL),
    // which is the dominant cost when cloning a repo with many files. The
    // guard rolls the batch back if any step below errors out. For the R2
    // path the transaction is open (but idle) across the network downloads —
    // safe here because clone/pull is the sole writer on this connection.
    let bulk = super::BulkTxn::begin(repo)?;
    let mut bytes_since_flush: u64 = 0;

    if !all_missing_hashes.is_empty() {
        let dl_resp = with_auth(
            client
                .post(format!("{remote}/api/{owner}/{name}/chunks/download"))
                .json(&serde_json::json!({ "hashes": all_missing_hashes })),
            api_key,
        )
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

        if !dl_resp.status().is_success() {
            return Err(OakError::Server(format!(
                "Failed to get chunk download info: {}",
                crate::http::error_text(dl_resp).await
            )));
        }

        let dl_result: ChunkDownloadResponse = dl_resp
            .json()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;

        let total_bytes: u64 = dl_result
            .chunks
            .iter()
            .map(|c| *all_missing_sizes.get(&c.hash).unwrap_or(&0) as u64)
            .sum();

        let pb = indicatif::ProgressBar::new(total_bytes);
        pb.set_style(
            indicatif::ProgressStyle::default_bar()
                .template(
                    "  Downloading [{bar:30.cyan/dim}] {bytes}/{total_bytes} ({bytes_per_sec})",
                )
                .unwrap()
                .progress_chars("━╸─"),
        );

        let concurrency = max_concurrent_downloads();
        let semaphore = Arc::new(Semaphore::new(concurrency));
        // Dedicated HTTP/1.1 client so concurrent chunk GETs each get their own
        // connection instead of being multiplexed (and throttled) onto one
        // shared HTTP/2 connection — see `download_client`.
        let dl_client = download_client(concurrency);
        let mut join_set: JoinSet<Result<(Hash, Vec<u8>, u64)>> = JoinSet::new();
        let chunk_count = dl_result.chunks.len();
        let inline_count = dl_result
            .chunks
            .iter()
            .filter(|c| c.content.is_some())
            .count();

        // When the server advertises a batch endpoint (chunk CDN configured),
        // route small chunks through it: one request per `BATCH_DOWNLOAD_SIZE`
        // chunks instead of one cold ~per-request-latency GET each. Large CDC
        // chunks keep their own cache-fronted GET (the RTT amortizes over MBs).
        let batch_url = dl_result.batch_url.clone();
        let all_missing_sizes = Arc::new(all_missing_sizes);
        let mut batch_hashes: Vec<String> = Vec::new();
        // Per-chunk single-GET URLs for the hashes routed to the batch, kept so
        // a failed batch request can fall back to fetching them individually.
        let mut batch_urls: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        output::vlog(&format!(
            "pull: downloading {chunk_count} chunk(s) ({inline_count} inline), concurrency={concurrency}, batch_endpoint={}",
            batch_url.is_some()
        ));

        for chunk_info in dl_result.chunks {
            let chunk_size = *all_missing_sizes.get(&chunk_info.hash).unwrap_or(&0) as u64;

            if let Some(content) = chunk_info.content {
                let chunk_hash = Hash(chunk_info.hash);
                // R2 chunks may be stored zstd-compressed; decode (raw passes
                // through) so local storage holds plaintext keyed by its hash.
                let content = oak_core::chunk_decode(content, &chunk_hash);
                repo.store_chunk(&chunk_hash, &content)?;
                pb.inc(chunk_size);
                bytes_since_flush += content.len() as u64;
                if bytes_since_flush >= BULK_FLUSH_BYTES {
                    bulk.flush()?;
                    bytes_since_flush = 0;
                }
            } else if batch_url.is_some() && chunk_size < SMALL_CHUNK_THRESHOLD {
                // Keep the chunk's own GET URL for the per-chunk fallback path.
                if let Some(durl) = chunk_info.download_url {
                    batch_urls.insert(chunk_info.hash.clone(), durl);
                }
                batch_hashes.push(chunk_info.hash);
            } else if let Some(download_url) = chunk_info.download_url {
                let client = dl_client.clone();
                let pb = pb.clone();
                let sem = semaphore.clone();
                let hash = chunk_info.hash;

                join_set.spawn(async move {
                    let _permit = sem
                        .acquire_owned()
                        .await
                        .map_err(|e| OakError::Http(e.to_string()))?;

                    let t_chunk = std::time::Instant::now();
                    let chunk_resp = client
                        .get(&download_url)
                        .send()
                        .await
                        .map_err(|e| OakError::Http(e.to_string()))?;

                    if !chunk_resp.status().is_success() {
                        return Err(OakError::Server(format!(
                            "Failed to download chunk from R2: {}",
                            crate::http::error_text(chunk_resp).await
                        )));
                    }

                    // The CDN sets cf-cache-status (HIT/MISS/…) — surfacing it
                    // tells us whether a slow clone is paying cold edge→R2
                    // fetches per chunk (the case the batch endpoint fixes).
                    let cache_status = chunk_resp
                        .headers()
                        .get("cf-cache-status")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("-")
                        .to_string();
                    let chunk_content = chunk_resp
                        .bytes()
                        .await
                        .map_err(|e| OakError::Http(e.to_string()))?;
                    let dt = t_chunk.elapsed().as_secs_f64();
                    output::vlog(&format!(
                        "pull: chunk {} ({} B) cf-cache={} in {:.3}s ({:.0} KiB/s)",
                        &hash[..hash.len().min(8)],
                        chunk_content.len(),
                        cache_status,
                        dt,
                        (chunk_content.len() as f64 / 1024.0) / dt.max(0.001),
                    ));
                    pb.inc(chunk_size);
                    Ok((Hash(hash), chunk_content.to_vec(), chunk_size))
                });
            } else {
                pb.finish_and_clear();
                return Err(OakError::Server(format!(
                    "No download URL or content for chunk: {}",
                    chunk_info.hash
                )));
            }
        }

        // Spawn the small-chunk batch requests now, BEFORE draining the
        // per-chunk GETs: the two pipelines are independent (disjoint chunk
        // sets, separate semaphores), and running them as sequential phases
        // added the whole batch phase — connection setup to the CDN host
        // included — onto the tail of the slowest per-chunk GET.
        let mut batch_set: JoinSet<Result<ChunkBatch>> = JoinSet::new();
        if let Some(batch_url) = batch_url {
            if !batch_hashes.is_empty() {
                output::vlog(&format!(
                    "pull: batching {} small chunk(s), {} per request",
                    batch_hashes.len(),
                    BATCH_DOWNLOAD_SIZE
                ));
                // A few concurrent bulk requests; each already carries up to
                // BATCH_DOWNLOAD_SIZE chunks, so modest fan-out saturates.
                let batch_sem = Arc::new(Semaphore::new(concurrency.min(8)));
                let batch_urls = Arc::new(batch_urls);
                for group in batch_hashes.chunks(BATCH_DOWNLOAD_SIZE) {
                    let group: Vec<String> = group.to_vec();
                    let client = dl_client.clone();
                    let pb = pb.clone();
                    let sem = batch_sem.clone();
                    let url = batch_url.clone();
                    let sizes = all_missing_sizes.clone();
                    let urls = batch_urls.clone();
                    batch_set.spawn(async move {
                        let _permit = sem
                            .acquire_owned()
                            .await
                            .map_err(|e| OakError::Http(e.to_string()))?;
                        // The batch URL already carries the capability token in
                        // `?t=`; the Worker authorizes on that, no API key.
                        let t = std::time::Instant::now();
                        let batch_result = async {
                            let resp = client
                                .post(&url)
                                .json(&serde_json::json!({ "hashes": group }))
                                .send()
                                .await
                                .map_err(|e| OakError::Http(e.to_string()))?;
                            if !resp.status().is_success() {
                                return Err(OakError::Server(format!(
                                    "Failed to download chunk batch: {}",
                                    crate::http::error_text(resp).await
                                )));
                            }
                            let body = resp
                                .bytes()
                                .await
                                .map_err(|e| OakError::Http(e.to_string()))?;
                            decode_chunk_batch(&body)
                        }
                        .await;

                        // The batch endpoint can return HTTP 200 with only a
                        // SUBSET of the requested hashes: each chunk is one R2
                        // read inside the Worker, and a read that throws or
                        // transiently fails is silently omitted from the framed
                        // body (the Worker tolerates it by design). A whole-batch
                        // failure (e.g. the CDN Worker threw — Cloudflare error
                        // 1101) yields no chunks at all. Either way, recover every
                        // hash the batch didn't return via its own cache-fronted
                        // single-chunk GET — the same path the Worker serves one
                        // R2 read at a time. Without this, omitted chunks slip
                        // through to blob reassembly and surface as the misleading
                        // "Missing chunk after download".
                        let mut decoded = match batch_result {
                            Ok(decoded) => {
                                output::vlog(&format!(
                                    "pull: batch of {} chunk(s) -> {} returned in {:.3}s",
                                    group.len(),
                                    decoded.len(),
                                    t.elapsed().as_secs_f64(),
                                ));
                                decoded
                            }
                            Err(e) => {
                                output::vlog(&format!(
                                    "pull: batch of {} chunk(s) failed ({e}); \
                                     falling back to per-chunk GETs",
                                    group.len(),
                                ));
                                ChunkBatch::new()
                            }
                        };

                        // Account progress for what the batch actually delivered,
                        // then refetch the rest one GET at a time.
                        let got: std::collections::HashSet<&str> =
                            decoded.iter().map(|(h, _)| h.as_str()).collect();
                        pb.inc(
                            group
                                .iter()
                                .filter(|h| got.contains(h.as_str()))
                                .map(|h| *sizes.get(h).unwrap_or(&0) as u64)
                                .sum::<u64>(),
                        );
                        let missing: Vec<&String> =
                            group.iter().filter(|h| !got.contains(h.as_str())).collect();
                        if !missing.is_empty() {
                            output::vlog(&format!(
                                "pull: batch omitted {} of {} chunk(s); \
                                 recovering via per-chunk GETs",
                                missing.len(),
                                group.len(),
                            ));
                            for hash in missing {
                                let Some(durl) = urls.get(hash) else {
                                    output::vlog(&format!(
                                        "pull: no single-chunk URL for {}, skipping",
                                        &hash[..hash.len().min(8)]
                                    ));
                                    continue;
                                };
                                match fetch_chunk_via_url(&client, durl).await {
                                    Ok(bytes) => {
                                        pb.inc(*sizes.get(hash).unwrap_or(&0) as u64);
                                        decoded.push((hash.clone(), bytes));
                                    }
                                    Err(e) => output::vlog(&format!(
                                        "pull: fallback GET for {} failed: {e}",
                                        &hash[..hash.len().min(8)]
                                    )),
                                }
                            }
                        }
                        Ok(decoded)
                    });
                }
            }
        }

        // Drain both pipelines concurrently, storing each chunk as it
        // lands. Stores stay on this task (SQLite is single-writer here),
        // so the shared flush counter only needs a `Cell`, not a lock.
        // `tokio::join!` waits for BOTH drains even when one errors — the
        // errors are checked afterwards so no spawned task is left running
        // against a rolled-back transaction.
        let bytes_cell = std::cell::Cell::new(bytes_since_flush);
        let store_chunk_flushing = |chunk_hash: Hash, data: Vec<u8>| -> Result<()> {
            let data = oak_core::chunk_decode(data, &chunk_hash);
            bytes_cell.set(bytes_cell.get() + data.len() as u64);
            repo.store_chunk(&chunk_hash, &data)?;
            if bytes_cell.get() >= BULK_FLUSH_BYTES {
                bulk.flush()?;
                bytes_cell.set(0);
            }
            Ok(())
        };
        let drain_large = async {
            while let Some(result) = join_set.join_next().await {
                let (chunk_hash, chunk_data, _) = result
                    .map_err(|e| OakError::Http(format!("Download task panicked: {e}")))??;
                store_chunk_flushing(chunk_hash, chunk_data)?;
            }
            Ok::<(), OakError>(())
        };
        let drain_batches = async {
            while let Some(res) = batch_set.join_next().await {
                let decoded =
                    res.map_err(|e| OakError::Http(format!("Batch task panicked: {e}")))??;
                for (hash, data) in decoded {
                    store_chunk_flushing(Hash(hash), data)?;
                }
            }
            Ok::<(), OakError>(())
        };
        let (large_res, batch_res) = tokio::join!(drain_large, drain_batches);
        large_res?;
        batch_res?;
        bytes_since_flush = bytes_cell.get();

        // Integrity gate: every chunk we set out to download must now be in
        // local storage before we reassemble blobs from it. The batch endpoint
        // can return a 200 with a partial set and the per-chunk recovery above
        // can itself fail; proceeding with gaps is what produced the misleading
        // "Server error: Missing chunk after download" at reassembly time. Fail
        // here instead, with the real cause and an actionable, retryable message.
        // Same-connection reads see the open bulk transaction's writes, so no
        // flush is needed for `has_chunk` to observe what we just stored.
        let mut still_missing: Vec<&String> = Vec::new();
        for h in &all_missing_hashes {
            if !chunk_is_present_and_valid(repo, &Hash(h.clone()))? {
                still_missing.push(h);
            }
        }
        if !still_missing.is_empty() {
            pb.finish_and_clear();
            let sample = &still_missing[0][..still_missing[0].len().min(12)];
            return Err(OakError::Server(format!(
                "{} of {} chunk(s) could not be downloaded (e.g. {sample}…). The chunk \
                 CDN batch endpoint returned an incomplete response and the per-chunk \
                 fallback also failed — this is usually transient, so retry the command. \
                 (Run with OAK_VERBOSE=1 to see which chunks failed.)",
                still_missing.len(),
                all_missing_hashes.len(),
            )));
        }

        pb.finish_and_clear();
        output::success(&format!("Downloaded {} chunk(s)", all_missing_hashes.len()));
    }

    // Reassemble every blob from its (now-local) chunks and write the
    // `blobs` row + `blob_chunks` mapping.
    let mut repaired = 0usize;
    for blob_data in blobs {
        let mut chunk_data_vec: Vec<Vec<u8>> = Vec::new();
        let mut chunk_infos: Vec<ChunkInfo> = Vec::new();
        for chunk_ref in &blob_data.chunks {
            let chunk_hash = Hash(chunk_ref.hash.clone());
            let data = repo.get_chunk(&chunk_hash)?.ok_or_else(|| {
                OakError::Server(format!("Missing chunk after download: {}", chunk_ref.hash))
            })?;
            chunk_infos.push(ChunkInfo {
                hash: chunk_hash,
                offset: chunk_ref.offset,
                length: chunk_ref.size,
            });
            chunk_data_vec.push(data);
        }

        let data_refs: Vec<&[u8]> = chunk_data_vec.iter().map(|d| d.as_slice()).collect();
        let content = reassemble_chunks(&data_refs);

        let expected = Hash(blob_data.hash.clone());
        let verified = verify_blob_content(content, &expected)?;
        if verified.repaired {
            repaired += 1;
        }
        let size = verified.content.len() as u64;
        repo.store_blob(&Blob {
            hash: expected.clone(),
            content: verified.content,
            size,
        })?;
        store_verified_blob_chunks(repo, &expected, chunk_infos, verified.repaired)?;

        bytes_since_flush += size;
        if bytes_since_flush >= BULK_FLUSH_BYTES {
            bulk.flush()?;
            bytes_since_flush = 0;
        }
    }
    warn_repaired_blobs(repaired);

    bulk.commit()?;
    Ok(())
}

/// A blob's verified plaintext, plus whether it had to be repaired from the
/// known server-side skew (see [`verify_blob_content`]).
pub(crate) struct VerifiedBlob {
    pub(crate) content: Vec<u8>,
    pub(crate) repaired: bool,
}

/// Gate every downloaded blob on `hash(content) == expected` before it is
/// stored. Without this, a server bug ships bytes that get written to the
/// local blob store under a hash they don't match — and from there straight
/// into the working tree, where the path-keyed stat cache hides the damage
/// until the next full scan (and an unattended commit then records the
/// garbage under its garbage hash).
///
/// One mismatch is repairable: a server whose `blob_chunks` rows were cut
/// from the zstd-compressed *at-rest* bytes instead of the plaintext (the
/// blob-store/chunk-CDN skew). The chunks themselves hash-verify — they're
/// honest chunks of the compressed frame — so the skew is only detectable
/// here, after reassembly: the result is a zstd frame that decompresses to
/// the true content. `chunk_decode` does exactly that probe (decompress,
/// check the hash, pass through unchanged otherwise). Anything else is
/// corruption and gets a hard error rather than a silent store.
pub(crate) fn verify_blob_content(content: Vec<u8>, expected: &Hash) -> Result<VerifiedBlob> {
    if &oak_core::hash_bytes(&content) == expected {
        return Ok(VerifiedBlob {
            content,
            repaired: false,
        });
    }
    let decoded = oak_core::chunk_decode(content, expected);
    if &oak_core::hash_bytes(&decoded) == expected {
        return Ok(VerifiedBlob {
            content: decoded,
            repaired: true,
        });
    }
    Err(OakError::Server(format!(
        "downloaded blob {expected} does not match its hash — refusing to store \
         corrupt content. Retry the command; if this persists, the server's \
         chunk data for this blob is corrupt.",
    )))
}

/// Return whether a local chunk row exists and still matches its content hash.
/// If a stale/corrupt row is found, delete it so the caller can refetch it.
pub(crate) fn chunk_is_present_and_valid(repo: &SqliteRepository, hash: &Hash) -> Result<bool> {
    let Some(content) = repo.get_chunk(hash)? else {
        return Ok(false);
    };
    if &oak_core::hash_bytes(&content) == hash {
        return Ok(true);
    }
    output::vlog(&format!(
        "discarding corrupt cached chunk {} before retry",
        hash.short()
    ));
    repo.delete_chunk(hash)?;
    Ok(false)
}

/// Persist a verified blob's chunk mapping. A repaired blob's advertised
/// chunk refs describe the compressed frame, not the plaintext we stored, so
/// recording them would re-poison push/serve (both re-advertise
/// `blob_chunks` as-is). Re-chunk the plaintext instead so the mapping
/// matches the stored bytes.
pub(crate) fn store_verified_blob_chunks(
    repo: &SqliteRepository,
    blob_hash: &Hash,
    advertised: Vec<ChunkInfo>,
    repaired: bool,
) -> Result<()> {
    if !repaired {
        return repo.store_blob_chunks(blob_hash, &advertised);
    }
    let blob = repo
        .get_blob(blob_hash)?
        .ok_or_else(|| OakError::BlobNotFound(blob_hash.to_string()))?;
    let mut infos = Vec::new();
    for (info, data) in oak_core::chunk_content(&blob.content) {
        repo.store_chunk(&info.hash, &data)?;
        infos.push(info);
    }
    repo.replace_blob_chunks(blob_hash, &infos)
}

/// One warning per download, not one per blob — a skewed server repo skews
/// every recently-pushed blob at once.
pub(crate) fn warn_repaired_blobs(repaired: usize) {
    if repaired > 0 {
        output::warning(&format!(
            "repaired {repaired} blob(s) whose server-side chunk data was \
             zstd-compressed at rest (blob-store/chunk skew); the server \
             should be checked for mis-chunked blobs"
        ));
    }
}

/// Orchestrator for `oak pull`: bring the local clone fully up to date.
///
/// Three phases:
/// 1. `--continue` / `--abort` — finish or abandon a parent-sync that
///    paused for conflict resolution. Returns early; no fetch happens.
/// 2. Fetch any new commits on the current branch from the server and
///    fast-forward the working tree (`fetch_current_branch`).
/// 3. Pull in the parent branch's new commits and 3-way-merge them into
///    the current branch (`sync::sync_from_parent`). This is what used to
///    be `oak sync`. On conflict, writes `.oak/SYNC_HEAD` and instructs
///    the user to run `oak pull --continue`.
///
/// `--force` discards local commits not on the remote during phase 2; it
/// does not affect phase 3.
///
/// When the remote answers with a redirect to a trusted Oak host (the old
/// origin has moved), the repo's stored remote is updated and the whole
/// pull retried once against the new origin — see `follow_remote_move`.
pub async fn run(
    path: &Path,
    remote_url: &str,
    force: bool,
    continue_after_conflict: bool,
    abort: bool,
) -> Result<()> {
    match run_once(path, remote_url, force, continue_after_conflict, abort).await {
        Err(OakError::RemoteMoved { origin }) if crate::http::is_trusted_origin(&origin) => {
            super::follow_remote_move(path, remote_url, &origin)?;
            run_once(path, &origin, force, continue_after_conflict, abort).await
        }
        result => result,
    }
}

async fn run_once(
    path: &Path,
    remote_url: &str,
    force: bool,
    continue_after_conflict: bool,
    abort: bool,
) -> Result<()> {
    if continue_after_conflict && abort {
        return Err(OakError::Io(std::io::Error::other(
            "--continue and --abort cannot be combined",
        )));
    }
    if continue_after_conflict {
        return super::sync::sync_continue(path);
    }
    if abort {
        return super::sync::sync_abort(path);
    }

    let branch_open_at_pull_start = current_open_branch_name(path)?;

    // Phase 2: fetch current branch
    fetch_current_branch(path, remote_url, force).await?;

    // Phase 3: sync parent into current branch. The branch may have no
    // parent (e.g. detached or a freshly-init'd repo with no remote yet);
    // surface those as success — there's nothing to sync.
    match super::sync::sync_from_parent_for_pull(path, branch_open_at_pull_start.as_deref()).await {
        Ok(Some(reconciled)) => super::merge::print_remote_merge_reconcile(&reconciled),
        Ok(None) => {}
        Err(OakError::BranchNotFound(msg)) if msg.contains("no parent to sync from") => {}
        Err(OakError::NoCommits) => {}
        Err(e) => return Err(e),
    }

    // Phase 4: with main's history freshly refreshed, sweep local rows for
    // branches whose work already landed on main (merged here, by another
    // client, or in the web UI) so they stop cluttering `oak switch`.
    let ctx = crate::resolve::resolve(path)?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    let pruned = super::merge::prune_merged_branches(&repo)?;
    super::merge::print_pruned_branches(&pruned);

    Ok(())
}

fn current_open_branch_name(path: &Path) -> Result<Option<String>> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    let Some(branch_name) = repo.get_current_branch_name()? else {
        return Ok(None);
    };
    let Some(branch) = repo.get_branch(&branch_name)? else {
        return Ok(None);
    };
    if branch.status == BranchStatus::Open {
        Ok(Some(branch_name))
    } else {
        Ok(None)
    }
}

/// Phase 2 of `oak pull`: fetch any new commits on the current branch from
/// the server and fast-forward the working tree. Was the entire body of
/// `oak pull` before `oak sync` was folded in.
pub async fn fetch_current_branch(path: &Path, remote_url: &str, force: bool) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let lock = crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir)?;
    let db_path = ctx.db_path()?;
    let work_tree = ctx.work_tree.clone();
    let repo = SqliteRepository::open(&db_path)?;

    // Save remote URL for future use
    repo.set_metadata(MetadataKey::RemoteUrl, remote_url)?;

    // Get local head
    let branch_name = repo.get_current_branch_name().ok().flatten();
    let local_head = if let Some(ref name) = branch_name {
        repo.get_branch_head(name)?
    } else {
        repo.get_head()?
    };

    // Get API key from env var, per-repo metadata, or global credentials
    let api_key = std::env::var("OAK_API_KEY")
        .ok()
        .or_else(|| repo.get_metadata(MetadataKey::ApiKey).ok().flatten())
        .or_else(|| super::credentials::get_token_for_server(remote_url));

    let (owner, repo_name) = super::read_repo_identity(&repo)?;

    pull_async(
        &lock,
        &repo,
        remote_url,
        &format!("{owner}/{repo_name}/pull"),
        branch_name.as_deref(),
        local_head.as_ref(),
        force,
        &work_tree,
        api_key.as_deref(),
    )
    .await
}

/// Stage pull response objects that are safe to leave unreferenced on retry.
///
/// This deliberately does not store branch rows, branch heads, current branch,
/// legacy HEAD, or rename metadata. Those are the publication surface callers
/// observe; they move only after content is present and the worktree update has
/// succeeded.
fn stage_pull_objects(
    repo: &SqliteRepository,
    pull_resp: &PullResponse,
) -> Result<std::collections::HashMap<String, Hash>> {
    repo.set_foreign_keys(false)?;
    let _fk_guard = super::FkGuard { repo };
    let bulk = super::BulkTxn::begin(repo)?;

    for tree_data in &pull_resp.trees {
        let tree = wire_to_core_tree(tree_data)?;
        repo.store_tree(&tree)?;
    }

    let mut branch_heads_seen: std::collections::HashMap<String, Hash> =
        std::collections::HashMap::new();
    for commit_data in &pull_resp.commits {
        let commit = commit_from_pull_data(commit_data)?;
        repo.store_commit(&commit)?;
        branch_heads_seen.insert(commit.branch_name.clone(), commit.hash.clone());

        output::item(&format!(
            "{}{}{} {}",
            output::colors::CYAN,
            &commit_data.hash[..12],
            output::colors::RESET,
            commit_data.message.as_deref().unwrap_or(""),
        ));
    }

    bulk.commit()?;
    Ok(branch_heads_seen)
}

fn apply_pull_renames(
    repo: &SqliteRepository,
    renames: &[BranchRenameData],
    last_rename_id: i64,
) -> Result<(usize, i64)> {
    let mut applied = 0usize;
    let mut highest_id = last_rename_id;
    for rn in renames {
        highest_id = highest_id.max(rn.id);
        if rn.old_name == rn.new_name {
            continue;
        }
        let has_old = repo.get_branch(&rn.old_name)?.is_some();
        if !has_old {
            continue;
        }
        if repo.get_branch(&rn.new_name)?.is_some() || repo.get_branch_head(&rn.new_name)?.is_some()
        {
            output::warning(&format!(
                "Skipping rename '{}' -> '{}': target name already exists locally",
                rn.old_name, rn.new_name
            ));
            continue;
        }
        repo.rename_branch(&rn.old_name, &rn.new_name)?;
        applied += 1;
    }
    Ok((applied, highest_id))
}

#[derive(Debug)]
struct PullRefPublication {
    orphaned_branch: Option<String>,
    renames_applied: usize,
}

struct PublishTxn<'a> {
    repo: &'a SqliteRepository,
    armed: bool,
}

impl<'a> PublishTxn<'a> {
    fn begin(repo: &'a SqliteRepository) -> Result<Self> {
        repo.write_txn_begin()?;
        Ok(Self { repo, armed: true })
    }

    fn commit(mut self) -> Result<()> {
        self.repo.write_txn_commit()?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for PublishTxn<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.repo.write_txn_rollback();
        }
    }
}

fn ensure_commit_present(repo: &SqliteRepository, hash: &Hash, context: &str) -> Result<()> {
    if repo.get_commit(hash)?.is_some() {
        return Ok(());
    }
    Err(OakError::Database(format!(
        "ref publication blocked: {context} points at missing commit {hash}"
    )))
}

#[allow(clippy::too_many_arguments)]
fn publish_pull_refs(
    repo: &SqliteRepository,
    pull_resp: &PullResponse,
    branch_name: Option<&str>,
    local_head: Option<&Hash>,
    force: bool,
    branch_heads_seen: &std::collections::HashMap<String, Hash>,
    new_head: Option<&Hash>,
    last_rename_id: i64,
) -> Result<PullRefPublication> {
    repo.set_foreign_keys(false)?;
    let _fk_guard = super::FkGuard { repo };
    let txn = PublishTxn::begin(repo)?;

    let (renames_applied, highest_rename_id) =
        apply_pull_renames(repo, &pull_resp.renames, last_rename_id)?;

    if let Some(br_data) = &pull_resp.branch {
        let br = branch_from_pull_data(br_data)?;
        repo.store_branch(&br)?;

        // Set as current if we don't have one.
        if repo.get_current_branch_name()?.is_none() {
            repo.set_current_branch(&br_data.name)?;
        }
    }

    // Store all branches referenced by commits. Sort parent-before-child so
    // the self-referential FK on branches(parent_branch) is satisfied when FK
    // checks are enabled by a future backend.
    let sorted_branches = super::sort_branches_topologically(
        pull_resp.branches.iter().collect::<Vec<_>>(),
        |b: &&BranchData| b.name.as_str(),
        |b: &&BranchData| b.parent_branch.as_deref(),
    );
    for br_data in sorted_branches {
        let status = BranchStatus::from_db_str(&br_data.status);
        // A branch that's already closed on the server and has no local row is
        // finished history (merged or abandoned elsewhere) — don't materialize
        // a tombstone for it.
        if status == BranchStatus::Closed && repo.get_branch(&br_data.name)?.is_none() {
            continue;
        }
        let br = branch_from_pull_data(br_data)?;
        repo.store_branch(&br)?;
    }

    // Set current branch from branches vec if not already set.
    if pull_resp.branch.is_none() && repo.get_current_branch_name()?.is_none() {
        if let Some(br_data) = pull_resp.branches.first() {
            repo.set_current_branch(&br_data.name)?;
        }
    }

    // `branch_heads` rows are learned from commit rows below, so a branch with
    // no commits of its own never gets one. Fall back to the head the server
    // reported for the requested branch — only when that commit is already in
    // local storage and there's no local head to overwrite.
    if let (Some(name), Some(head_str)) = (branch_name, pull_resp.head.as_deref()) {
        if !branch_heads_seen.contains_key(name) && repo.get_branch_head(name)?.is_none() {
            let head = Hash(head_str.to_string());
            if repo.get_commit(&head)?.is_some() {
                repo.set_branch_head(name, &head)?;
            }
        }
    }

    let mut orphaned_branch = None;
    if force {
        if let (Some(branch), Some(old_tip)) = (branch_name, local_head) {
            if let Some(new_branch_head) = branch_heads_seen.get(branch) {
                if new_branch_head != old_tip
                    && !super::sync::local_history_contains(repo, new_branch_head, old_tip)?
                {
                    orphaned_branch = Some(park_orphaned_branch_in_txn(repo, branch, old_tip)?);
                }
            }
        }
    }

    for (br_name, br_head) in branch_heads_seen {
        ensure_commit_present(repo, br_head, &format!("branch '{br_name}' head"))?;
        repo.set_branch_head(br_name, br_head)?;
    }

    if let Some(head) = new_head {
        ensure_commit_present(repo, head, "HEAD")?;
        repo.set_head(head)?;
    }

    if highest_rename_id > last_rename_id {
        repo.set_metadata(MetadataKey::LastRenameId, &highest_rename_id.to_string())?;
    }

    txn.commit()?;
    Ok(PullRefPublication {
        orphaned_branch,
        renames_applied,
    })
}

/// Async pull implementation.
/// `endpoint_pull_path` is e.g. "repos/my-repo/pull".
#[allow(clippy::too_many_arguments)]
pub async fn pull_async(
    lock: &WorkdirLock,
    repo: &SqliteRepository,
    remote: &str,
    endpoint_pull_path: &str,
    branch_name: Option<&str>,
    local_head: Option<&Hash>,
    force: bool,
    work_tree: &Path,
    api_key: Option<&str>,
) -> Result<()> {
    let client = crate::http::api_client();

    // Build pull URL
    let mut url = format!("{remote}/api/{endpoint_pull_path}");
    let separator = if url.contains('?') { "&" } else { "?" };
    let mut params = Vec::new();
    if let Some(head) = local_head {
        params.push(format!("since={head}"));
    }
    if force {
        params.push("force=true".to_string());
    }
    if let Some(name) = branch_name {
        params.push(format!("branch_name={name}"));
    }

    // Tell the server which rename events we've already applied so it
    // only returns ones we still need to replay.
    let last_rename_id: i64 = repo
        .get_metadata(MetadataKey::LastRenameId)?
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if last_rename_id > 0 {
        params.push(format!("since_rename_id={last_rename_id}"));
    }

    if !params.is_empty() {
        url = format!("{}{}{}", url, separator, params.join("&"));
    }

    let resp = with_auth(client.get(&url), api_key)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if resp.status().as_u16() == 404 {
        return Err(OakError::RemoteRepoNotFound(endpoint_pull_path.to_string()));
    }

    if resp.status().as_u16() == 409 {
        // The server doesn't know our `since` commit: this branch's local
        // history isn't anchored on the remote's. Converge by snapshot
        // re-parenting instead of dead-ending (Invariant 2).
        return reparent_after_divergence(
            lock,
            repo,
            remote,
            branch_name,
            local_head,
            work_tree,
            api_key,
        )
        .await;
    }

    if force && local_head.is_some() {
        output::warning(
            "Force pull: local-only commits will be parked on an orphaned branch, then re-synced from remote",
        );
    }

    if !resp.status().is_success() {
        return Err(crate::http::server_error(resp).await);
    }

    let pull_resp: PullResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if pull_resp.commits.is_empty() {
        let published = publish_pull_refs(
            repo,
            &pull_resp,
            branch_name,
            local_head,
            force,
            &std::collections::HashMap::new(),
            local_head,
            last_rename_id,
        )?;
        if published.renames_applied > 0 {
            output::info(&format!(
                "Applied {} branch rename(s)",
                published.renames_applied
            ));
        }
        output::info("Already up to date");
        return Ok(());
    }

    output::info(&format!("Pulling {} commit(s)...", pull_resp.commits.len()));

    // Fetch chunks from R2 (or inline from server) and materialize every
    // blob into local SQLite. Shared with `oak clone`.
    let (owner_pull, repo_pull) = super::read_repo_identity(repo)?;
    fetch_and_store_blobs(
        repo,
        &pull_resp.blobs,
        &client,
        remote,
        &owner_pull,
        &repo_pull,
        api_key,
    )
    .await?;

    let branch_heads_seen = stage_pull_objects(repo, &pull_resp)?;

    // Determine the head to advance the current branch / global head to.
    // Prefer the head of the current branch from this payload; if the
    // payload had no commits on the current branch, fall back to the
    // local head (no change).
    let new_head: Option<Hash> = branch_name
        .and_then(|n| branch_heads_seen.get(n).cloned())
        .or_else(|| local_head.cloned());

    // Refuse to materialize over uncommitted work BEFORE moving any pointers,
    // so a refused pull is a clean, retryable no-op. `update_working_dir`
    // (below) rewrites every manifest entry and deletes upstream-removed
    // tracked files, silently overwriting uncommitted edits — even edits to
    // files that didn't change upstream. Any pull that reaches the
    // materialize step (`new_head.is_some()`) touches the tree, so the guard
    // keys off that, not just on the current branch advancing. `merge`/`switch`
    // refuse a dirty tree the same way; the 409-divergence path has its own
    // clean probe. `--force` opts into the overwrite (and parked any
    // local-only commits just above).
    if new_head.is_some() && !force {
        let clean = super::commit::worktree_is_clean_without_storing_blobs(repo, work_tree)?;
        if !clean {
            return Err(OakError::DirtyWorkingTree(
                "working tree has uncommitted changes — run `oak commit` to keep them \
                 (or `oak reset` to discard them) before pulling, or `oak pull --force` \
                 to overwrite them"
                    .to_string(),
            ));
        }
    }

    // Update working directory to match new head, applying upstream
    // deletions relative to the pre-pull head. Keep refs on the old state
    // until this succeeds, so a crash before publication can be retried
    // without exposing a head that the worktree did not reach.
    if let Some(ref head) = new_head {
        update_working_dir(lock, work_tree, repo, local_head, head)?;
    }

    let published = publish_pull_refs(
        repo,
        &pull_resp,
        branch_name,
        local_head,
        force,
        &branch_heads_seen,
        new_head.as_ref(),
        last_rename_id,
    )?;
    if published.renames_applied > 0 {
        output::info(&format!(
            "Applied {} branch rename(s)",
            published.renames_applied
        ));
    }
    if let Some(orphan) = published.orphaned_branch {
        output::info(&format!(
            "Parked local commits as '{orphan}' (recover with `oak switch {orphan}`)"
        ));
    }

    output::success(&format!("Pulled {} commit(s)", pull_resp.commits.len()));

    Ok(())
}

/// Converge a diverged branch after the server rejected our `since` (HTTP
/// 409): snapshot re-parenting per Invariant 2. The branch's changes are
/// overlaid onto the server's current snapshot as one new commit; overlapping
/// edits route into the EXISTING `oak pull --continue` / `--abort` conflict
/// flow. Local commits are never discarded — the old tip stays reachable via
/// the new commit's `merge_parent_hash`.
async fn reparent_after_divergence(
    lock: &WorkdirLock,
    repo: &SqliteRepository,
    remote: &str,
    branch_name: Option<&str>,
    local_head: Option<&Hash>,
    work_tree: &Path,
    api_key: Option<&str>,
) -> Result<()> {
    let (Some(branch), Some(_old_tip)) = (branch_name, local_head) else {
        return Err(OakError::LocalCommitsNotInRemoteHistory);
    };
    // Only re-parent the checked-out branch: every mutation below (heads,
    // conflict write-out, worktree reset) assumes it.
    if repo.get_current_branch_name()?.as_deref() != Some(branch) {
        return Err(OakError::LocalCommitsNotInRemoteHistory);
    }
    let (owner, repo_name) = super::read_repo_identity(repo)?;

    // Refresh canonical main first: stores the server's main head verbatim,
    // backfills ancestry for the LCA finder, and repoints anything still
    // anchored on a synthetic main duplicate (Invariant 1). Best-effort —
    // the re-parent itself fetches what it needs.
    if let Err(e) = super::sync::fetch_parent_from_server(repo, oak_core::DEFAULT_BRANCH).await {
        output::vlog(&format!("pull: main refresh before re-parent failed: {e}"));
    }

    // Probe dirtiness BEFORE moving any pointers — the probe compares the
    // working tree against the current head.
    let worktree_clean = super::commit::worktree_is_clean_without_storing_blobs(repo, work_tree)?;

    match super::sync::prepare_reparent(repo, remote, &owner, &repo_name, branch, api_key).await? {
        super::sync::ReparentCheck::AlreadyAnchored => {
            output::info("Already up to date (local commits not pushed yet)");
            Ok(())
        }
        super::sync::ReparentCheck::Ready(plan) => {
            if plan.conflict_count() > 0 {
                let merged = Manifest::new(plan.merged_entries.clone());
                super::sync::write_sync_conflict_state(
                    lock,
                    repo,
                    work_tree,
                    branch,
                    &plan.seed_branch_name,
                    Some(&plan.seed),
                    Some(&plan.old_tip),
                    &merged,
                    &plan.conflict_paths,
                    &plan.binary_conflict_paths,
                )?;
                return Err(OakError::MergeConflict(plan.conflict_count()));
            }
            let result = super::sync::complete_reparent(
                Some(lock),
                repo,
                work_tree,
                branch,
                &plan,
                worktree_clean,
            )?;
            output::success(&format!(
                "Re-parented '{branch}' onto {}@{} (local commits kept)",
                plan.seed_branch_name,
                plan.seed.short()
            ));
            output::item(&format!("commit {}", result.commit.short()));
            if !result.reset_worktree {
                output::warning("Preserved uncommitted changes in the working tree.");
            }
            Ok(())
        }
    }
}

/// Create the keepsafe branch row for `oak pull --force`: an open branch
/// named `<branch>.orphaned-<timestamp>` whose head is the local tip being
/// replaced, so the parked commits stay reachable.
#[allow(dead_code)]
fn park_orphaned_branch(repo: &SqliteRepository, branch: &str, tip: &Hash) -> Result<String> {
    let txn = PublishTxn::begin(repo)?;
    let orphan = park_orphaned_branch_in_txn(repo, branch, tip)?;
    txn.commit()?;
    Ok(orphan)
}

fn park_orphaned_branch_in_txn(
    repo: &SqliteRepository,
    branch: &str,
    tip: &Hash,
) -> Result<String> {
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let mut orphan = format!("{branch}.orphaned-{ts}");
    let mut n = 2;
    while repo.get_branch(&orphan)?.is_some() {
        orphan = format!("{branch}.orphaned-{ts}-{n}");
        n += 1;
    }
    let row = Branch::new(
        orphan.clone(),
        Some(format!(
            "local commits parked by `oak pull --force` from '{branch}'"
        )),
        Some(oak_core::DEFAULT_BRANCH.to_string()),
    );
    repo.store_branch(&row)?;
    repo.set_branch_head(&orphan, tip)?;
    Ok(orphan)
}

/// Update working directory to match the given commit, treating the pull as a
/// fast-forward from `old_head`: every manifest entry of `head` is written,
/// and any path tracked at `old_head` but absent from `head` — a deletion
/// that happened upstream — is removed from disk (and from the stat cache).
/// The user's untracked files are preserved. Without the deletion pass, a
/// file deleted upstream stayed on disk and, because Oak commits are
/// whole-worktree snapshots, silently resurrected on the next `oak commit`.
pub(crate) fn update_working_dir(
    lock: &WorkdirLock,
    path: &Path,
    repo: &SqliteRepository,
    old_head: Option<&Hash>,
    head: &Hash,
) -> Result<()> {
    let commit = repo
        .get_commit(head)?
        .ok_or_else(|| OakError::CommitNotFound(head.to_string()))?;

    let manifest = repo
        .get_manifest(&commit.manifest_hash)?
        .ok_or_else(|| OakError::ManifestNotFound(commit.manifest_hash.to_string()))?;

    // The pre-pull head's manifest drives the deletion diff. Tolerant
    // lookups: a missing old commit/manifest just means no tracked
    // deletions to apply (e.g. a fresh clone).
    let old_manifest = match old_head {
        Some(h) => match repo.get_commit(h)? {
            Some(c) => repo
                .get_manifest(&c.manifest_hash)?
                .unwrap_or_else(Manifest::empty),
            None => Manifest::empty(),
        },
        None => Manifest::empty(),
    };

    let allow_partial = std::env::var("OAK_ALLOW_PARTIAL_CLONE")
        .ok()
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));

    // Same rationale as `clone_repo`'s `write_working_directory`: a silent
    // skip on a missing blob produces a partial working tree that
    // masquerades as "everything modified" in `oak status`. Surface the gap
    // as a hard error instead — unless the operator opts into a partial pull
    // with OAK_ALLOW_PARTIAL_CLONE=1 to recover from a server in a broken
    // state. Checked up front so the error fires before any file is touched.
    if !allow_partial {
        for entry in &manifest.entries {
            if !repo.has_blob(&entry.blob_hash)? {
                return Err(OakError::Server(format!(
                    "pull is missing blob {} for '{}'. The server didn't ship \
                     this blob's bytes — check `blob_chunks` / R2 state for \
                     this hash on the server. Set OAK_ALLOW_PARTIAL_CLONE=1 \
                     to skip missing files instead.",
                    entry.blob_hash, entry.path,
                )));
            }
        }
    }

    let report = apply_manifest(
        lock,
        path,
        repo,
        &manifest,
        ApplyOpts {
            delete: DeleteScope::TrackedRemoved { old: &old_manifest },
            missing_blobs: if allow_partial {
                MissingBlobs::Skip
            } else {
                MissingBlobs::Error
            },
            ..ApplyOpts::default()
        },
    )?;

    if !report.skipped.is_empty() {
        output::warning(&format!(
            "OAK_ALLOW_PARTIAL_CLONE: skipped {} missing file(s):",
            report.skipped.len()
        ));
        for p in &report.skipped {
            output::warning(&format!("  - {p}"));
        }
    }

    Ok(())
}

#[cfg(test)]
mod verify_blob_content_tests {
    use super::verify_blob_content;
    use oak_core::{hash_bytes, Hash};

    #[test]
    fn matching_content_passes_through() {
        let content = b"hello world".to_vec();
        let expected = hash_bytes(&content);
        let v = verify_blob_content(content.clone(), &expected).unwrap();
        assert!(!v.repaired);
        assert_eq!(v.content, content);
    }

    /// The blob-store/chunk skew: the server chunked the zstd-compressed
    /// at-rest bytes, so reassembly yields a zstd frame of the true content
    /// under the plaintext blob hash. Must decompress and verify.
    #[test]
    fn zstd_frame_of_true_content_is_repaired() {
        let plaintext = "the real file content\n".repeat(50).into_bytes();
        let expected = hash_bytes(&plaintext);
        let frame = zstd::encode_all(&plaintext[..], 3).unwrap();
        assert_ne!(hash_bytes(&frame), expected);
        let v = verify_blob_content(frame, &expected).unwrap();
        assert!(v.repaired);
        assert_eq!(v.content, plaintext);
    }

    #[test]
    fn corrupt_content_is_rejected_not_stored() {
        let expected = hash_bytes(b"what the server promised");
        assert!(verify_blob_content(b"something else entirely".to_vec(), &expected).is_err());
        // A valid zstd frame of the WRONG plaintext must also be rejected.
        let frame = zstd::encode_all(&b"wrong plaintext"[..], 3).unwrap();
        assert!(verify_blob_content(frame, &expected).is_err());
        // And garbage that merely starts with the zstd magic.
        let mut garbage = vec![0x28, 0xB5, 0x2F, 0xFD];
        garbage.extend_from_slice(b"not a real frame");
        assert!(verify_blob_content(garbage, &Hash(expected.0.clone())).is_err());
    }
}

#[cfg(test)]
mod pull_ingest_publish_tests {
    use super::{
        publish_pull_refs, stage_pull_objects, BranchData, BranchRenameData, CommitData,
        PullResponse,
    };
    use oak_core::protocol::{BlobData, FileChangeData};
    use oak_core::{Branch, Hash, MetadataKey, Repository, SqliteRepository};
    use tempfile::TempDir;

    fn repo() -> (TempDir, SqliteRepository) {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        (temp, repo)
    }

    fn branch(name: &str) -> BranchData {
        BranchData {
            name: name.to_string(),
            description: Some(format!("{name} branch")),
            parent_branch: Some("main".to_string()),
            status: "open".to_string(),
            created_at: "2026-06-16T00:00:00Z".to_string(),
            close_reason: None,
        }
    }

    fn commit(branch_name: &str, hash: &str) -> CommitData {
        CommitData {
            hash: hash.to_string(),
            branch_name: branch_name.to_string(),
            parent_hash: None,
            merge_parent_hash: None,
            manifest_hash: "f".repeat(64),
            author: "test".to_string(),
            message: Some("test commit".to_string()),
            timestamp: "2026-06-16T00:00:00Z".to_string(),
            files: vec![FileChangeData {
                path: "src/lib.rs".to_string(),
                change_type: "modified".to_string(),
                old_blob_hash: None,
                new_blob_hash: Some("b".repeat(64)),
                old_path: None,
            }],
        }
    }

    fn response_for(branch_name: &str, commit_hash: &str) -> PullResponse {
        PullResponse {
            head: Some(commit_hash.to_string()),
            branch: None,
            branches: vec![branch(branch_name)],
            commits: vec![commit(branch_name, commit_hash)],
            blobs: Vec::<BlobData>::new(),
            trees: Vec::new(),
            renames: Vec::new(),
        }
    }

    #[test]
    fn staging_pull_objects_does_not_publish_branch_refs() {
        let (_temp, repo) = repo();
        repo.store_branch(&Branch::new("main".to_string(), None, None))
            .unwrap();
        repo.set_current_branch("main").unwrap();

        let commit_hash = "a".repeat(64);
        let pull = response_for("feature", &commit_hash);

        let heads = stage_pull_objects(&repo, &pull).unwrap();

        assert_eq!(heads.get("feature"), Some(&Hash(commit_hash.clone())));
        assert!(repo.get_commit(&Hash(commit_hash)).unwrap().is_some());
        assert!(repo.get_branch("feature").unwrap().is_none());
        assert!(repo.get_branch_head("feature").unwrap().is_none());
        assert_eq!(
            repo.get_current_branch_name().unwrap().as_deref(),
            Some("main")
        );
    }

    #[test]
    fn publish_refs_rolls_back_branch_rows_on_error() {
        let (_temp, repo) = repo();
        let mut pull = PullResponse {
            head: None,
            branch: Some(branch("feature")),
            branches: vec![branch("broken")],
            commits: Vec::new(),
            blobs: Vec::<BlobData>::new(),
            trees: Vec::new(),
            renames: Vec::new(),
        };
        pull.branches[0].created_at = "not a timestamp".to_string();

        let err = publish_pull_refs(
            &repo,
            &pull,
            Some("feature"),
            None,
            false,
            &std::collections::HashMap::new(),
            None,
            0,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("input contains invalid characters")
                || err.to_string().contains("premature end of input")
                || err.to_string().contains("too short"),
            "unexpected error: {err}"
        );
        assert!(repo.get_branch("feature").unwrap().is_none());
        assert!(repo.get_branch("broken").unwrap().is_none());
        assert!(repo.get_current_branch_name().unwrap().is_none());
    }

    #[test]
    fn publish_refs_rejects_missing_commit_head_and_rolls_back() {
        let (_temp, repo) = repo();
        let missing_hash = Hash("d".repeat(64));
        let pull = PullResponse {
            head: Some(missing_hash.to_string()),
            branch: Some(branch("feature")),
            branches: vec![branch("feature")],
            commits: Vec::new(),
            blobs: Vec::<BlobData>::new(),
            trees: Vec::new(),
            renames: Vec::new(),
        };
        let branch_heads_seen =
            std::collections::HashMap::from([("feature".to_string(), missing_hash.clone())]);

        let err = publish_pull_refs(
            &repo,
            &pull,
            Some("feature"),
            None,
            false,
            &branch_heads_seen,
            Some(&missing_hash),
            0,
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("missing commit"),
            "unexpected error: {err}"
        );
        assert!(repo.get_branch("feature").unwrap().is_none());
        assert!(repo.get_branch_head("feature").unwrap().is_none());
        assert!(repo.get_head().unwrap().is_none());
    }

    #[test]
    fn publish_refs_lands_rename_heads_current_head_and_watermark_together() {
        let (_temp, repo) = repo();
        repo.store_branch(&Branch::new("old".to_string(), None, None))
            .unwrap();
        repo.set_current_branch("old").unwrap();

        let commit_hash = "c".repeat(64);
        let mut pull = response_for("new", &commit_hash);
        pull.renames = vec![BranchRenameData {
            id: 42,
            old_name: "old".to_string(),
            new_name: "new".to_string(),
            renamed_at: "2026-06-16T00:00:00Z".to_string(),
        }];

        let heads = stage_pull_objects(&repo, &pull).unwrap();
        let published = publish_pull_refs(
            &repo,
            &pull,
            Some("new"),
            None,
            false,
            &heads,
            Some(&Hash(commit_hash.clone())),
            0,
        )
        .unwrap();

        assert_eq!(published.renames_applied, 1);
        assert!(repo.get_branch("old").unwrap().is_none());
        assert!(repo.get_branch("new").unwrap().is_some());
        assert_eq!(
            repo.get_current_branch_name().unwrap().as_deref(),
            Some("new")
        );
        assert_eq!(
            repo.get_branch_head("new").unwrap(),
            Some(Hash(commit_hash.clone()))
        );
        assert_eq!(repo.get_head().unwrap(), Some(Hash(commit_hash)));
        assert_eq!(
            repo.get_metadata(MetadataKey::LastRenameId)
                .unwrap()
                .as_deref(),
            Some("42")
        );
    }
}

#[cfg(test)]
mod chunk_batch_decode_tests {
    use super::decode_chunk_batch;

    /// Build a frame the way the CDN Worker (and server) do:
    /// `[u32 BE hash_len][hash][u32 BE data_len][data]` per entry.
    fn frame(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (hash, data) in entries {
            buf.extend_from_slice(&(hash.len() as u32).to_be_bytes());
            buf.extend_from_slice(hash.as_bytes());
            buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
            buf.extend_from_slice(data);
        }
        buf
    }

    #[test]
    fn decodes_multiple_entries() {
        let entries: &[(&str, &[u8])] = &[
            ("aaaa", b"hello"),
            ("bbbb", b""),
            ("cccc", &[0u8, 1, 2, 255]),
        ];
        let decoded = decode_chunk_batch(&frame(entries)).expect("valid frame decodes");
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0], ("aaaa".to_string(), b"hello".to_vec()));
        assert_eq!(decoded[1], ("bbbb".to_string(), Vec::<u8>::new()));
        assert_eq!(decoded[2], ("cccc".to_string(), vec![0, 1, 2, 255]));
    }

    #[test]
    fn empty_body_decodes_to_empty() {
        assert!(decode_chunk_batch(&[]).unwrap().is_empty());
    }

    #[test]
    fn truncated_frames_error_not_panic() {
        let mut body = frame(&[("aaaa", b"hello")]);
        body.truncate(body.len() - 2);
        assert!(decode_chunk_batch(&body).is_err());
        assert!(decode_chunk_batch(&[0, 0, 0, 8]).is_err());
        assert!(decode_chunk_batch(&[0, 0]).is_err());
    }
}
