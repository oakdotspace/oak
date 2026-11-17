//! The one chunk uploader: have/want negotiation, bounded concurrency and
//! incremental durable acknowledgement.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use oak_core::protocol::{ChunkCheckServerResponse, ChunkUploadInfo};
use oak_core::{OakError, Result};
use serde::Deserialize;
use tokio::task::JoinSet;

use super::{Claim, TransferSession};
use crate::output;

const CHUNK_BUSY_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);
const CHUNK_BUSY_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

/// Chunks negotiated per `chunks/check` request. With FastCDC's 4 MiB
/// maximum this keeps at most [`NEGOTIATION_PAGE_BYTES`] of chunk bytes
/// resident while a page is checked and uploaded.
const NEGOTIATION_PAGE_CHUNKS: usize = 32;
const NEGOTIATION_PAGE_BYTES: usize = 128 * 1024 * 1024;

/// Chunks below this size are sent in bulk through `chunks/batch` instead of
/// one presigned PUT each (== FastCDC's minimum chunk size).
const SMALL_CHUNK_THRESHOLD: usize = 256 * 1024;
/// Per bulk request, under the edge's request-body cap.
const BATCH_MAX_BYTES: usize = 12 * 1024 * 1024;
const BATCH_MAX_CHUNKS: usize = 256;

/// Direct (presigned) uploads become durable only once `chunks/uploaded`
/// acknowledges them. Finished uploads are acknowledged in groups: a group is
/// sent when [`CONFIRM_GROUP_CHUNKS`] uploads are waiting, when the oldest
/// waiting upload finished [`CONFIRM_LINGER`] ago, or when no upload of the
/// page is still running — whichever comes first — with at most
/// [`CONFIRMS_IN_FLIGHT`] requests open.
///
/// The bound on work an interruption can lose: uploads still in flight, plus
/// finished uploads that waited less than the linger (2 s) or whose
/// acknowledgement request had not returned — never more than one
/// negotiation page (32 chunks, at most 128 MiB) *per uploader*. Pages of one
/// uploader run one after another, but headless preload runs up to three
/// uploaders at once, so a push interrupted during preload fan-out can lose
/// up to three pages (96 chunks, 384 MiB). Peak memory is bounded the same
/// way: about one page of chunk bytes per concurrent uploader (≈3 × 128 MiB
/// under preload) plus encoded request bodies, independent of blob size.
const CONFIRM_GROUP_CHUNKS: usize = 16;
const CONFIRM_LINGER: std::time::Duration = std::time::Duration::from_secs(2);
const CONFIRMS_IN_FLIGHT: usize = 2;

/// `chunks/uploaded` is idempotent. The server answers 409 when a receipt it
/// is proving changed underneath it (another client confirming the same
/// chunk) and asks for a retry; that is retried this many times.
const CONFIRM_CONFLICT_RETRIES: usize = 4;

#[derive(Deserialize)]
struct ChunkObjectBusyResponse {
    error: String,
    retry_after_ms: u64,
}

/// The overall deadline and the cumulative busy-wait budget shared by every
/// chunk request of one upload.
pub(crate) struct ChunkBusyRetryState {
    overall_deadline: tokio::time::Instant,
    busy_wait_budget: std::time::Duration,
    busy_wait_reserved: tokio::sync::Mutex<std::time::Duration>,
}

impl ChunkBusyRetryState {
    /// The size-aware deadline and busy-retry budget for `declared_bytes` of
    /// chunk uploads.
    pub(crate) fn for_upload(declared_bytes: u64) -> Arc<Self> {
        Self::until(tokio::time::Instant::now() + super::size_aware_timeout(declared_bytes))
    }

    /// The standard busy-retry budget under a caller-owned deadline.
    pub(crate) fn until(overall_deadline: tokio::time::Instant) -> Arc<Self> {
        Arc::new(Self::new(overall_deadline, CHUNK_BUSY_RETRY_BUDGET))
    }

    pub(crate) fn new(
        overall_deadline: tokio::time::Instant,
        busy_wait_budget: std::time::Duration,
    ) -> Self {
        Self {
            overall_deadline,
            busy_wait_budget,
            busy_wait_reserved: tokio::sync::Mutex::new(std::time::Duration::ZERO),
        }
    }
}

fn chunk_request_remaining(
    retry_state: &ChunkBusyRetryState,
    context: &str,
) -> Result<std::time::Duration> {
    let remaining = retry_state
        .overall_deadline
        .saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(OakError::Server(format!(
            "{context} exceeded the size-aware push deadline; retry the push"
        )));
    }
    Ok(remaining)
}

pub(crate) async fn decode_chunk_response_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    retry_state: &ChunkBusyRetryState,
    context: &str,
) -> Result<T> {
    let remaining = chunk_request_remaining(retry_state, context)?;
    tokio::time::timeout(remaining, response.json())
        .await
        .map_err(|_| {
            OakError::Server(format!(
                "{context} response body exceeded the size-aware push deadline"
            ))
        })?
        .map_err(|error| OakError::Http(error.to_string()))
}

async fn chunk_error_text_with_deadline(
    response: reqwest::Response,
    retry_state: &ChunkBusyRetryState,
    context: &str,
) -> String {
    let Ok(remaining) = chunk_request_remaining(retry_state, context) else {
        return format!("{context} response body exceeded the size-aware push deadline");
    };
    match tokio::time::timeout(remaining, crate::http::error_text(response)).await {
        Ok(detail) => detail,
        Err(_) => format!("{context} response body exceeded the size-aware push deadline"),
    }
}

/// PUT one chunk to its presigned object-storage URL. The PUT is idempotent
/// (the key is the content hash), so transient failures are replayed inside
/// the transfer deadline.
pub(crate) async fn send_presigned_chunk_put(
    client: &reqwest::Client,
    url: &str,
    body: Vec<u8>,
    retry_state: &ChunkBusyRetryState,
) -> Result<reqwest::Response> {
    let remaining = chunk_request_remaining(retry_state, "presigned chunk upload")?;
    crate::http::send_idempotent_with_retry_until(
        client
            .put(url)
            .header("content-type", "application/octet-stream")
            .body(body),
        "presigned chunk upload",
        tokio::time::Instant::now() + remaining,
    )
    .await
}

pub(crate) async fn send_chunk_request_with_busy_retry(
    request: reqwest::RequestBuilder,
    retry_state: Arc<ChunkBusyRetryState>,
    context: &str,
) -> Result<reqwest::Response> {
    send_chunk_request_retrying_conflicts(request, retry_state, context, 0).await
}

/// [`send_chunk_request_with_busy_retry`], additionally replaying the
/// request up to `other_conflict_retries` times when the server answers a
/// 409 that is not the typed `chunk_object_busy` fence. Only for requests
/// whose every 409 means "state changed underneath; retry".
async fn send_chunk_request_retrying_conflicts(
    request: reqwest::RequestBuilder,
    retry_state: Arc<ChunkBusyRetryState>,
    context: &str,
    mut other_conflict_retries: usize,
) -> Result<reqwest::Response> {
    let template = request.try_clone().ok_or_else(|| {
        OakError::InvalidArgument(format!(
            "{context} request body cannot be replayed safely; retry the push"
        ))
    })?;
    let mut backoff = std::time::Duration::from_millis(250);
    loop {
        let remaining = chunk_request_remaining(&retry_state, context)?;
        let attempt = template.try_clone().ok_or_else(|| {
            OakError::InvalidArgument(format!(
                "{context} request body cannot be replayed safely; retry the push"
            ))
        })?;
        let response = crate::http::send_idempotent_with_retry_until(
            attempt,
            context,
            tokio::time::Instant::now() + remaining,
        )
        .await?;
        if response.status() != reqwest::StatusCode::CONFLICT {
            return Ok(response);
        }

        let retry_after_header = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(std::time::Duration::from_secs)
            .unwrap_or_default();
        let remaining_after_headers = retry_state
            .overall_deadline
            .saturating_duration_since(tokio::time::Instant::now());
        if remaining_after_headers.is_zero() {
            return Err(OakError::Server(format!(
                "{context} exhausted the bounded push deadline while reading a chunk maintenance conflict"
            )));
        }
        let body = tokio::time::timeout(remaining_after_headers, response.bytes())
            .await
            .map_err(|_| OakError::Server(format!("{context} conflict response body timed out")))?
            .map_err(|error| OakError::Http(error.to_string()))?;
        let busy = serde_json::from_slice::<ChunkObjectBusyResponse>(&body)
            .ok()
            .filter(|busy| busy.error == "chunk_object_busy");
        let Some(busy) = busy else {
            if other_conflict_retries == 0 {
                return Err(OakError::Server(format!(
                    "{context} returned 409 Conflict: {}",
                    String::from_utf8_lossy(&body)
                )));
            }
            other_conflict_retries -= 1;
            let remaining = retry_state
                .overall_deadline
                .saturating_duration_since(tokio::time::Instant::now());
            if backoff >= remaining {
                return Err(OakError::Server(format!(
                    "{context} kept returning 409 Conflict until the push deadline; retry the push"
                )));
            }
            output::vlog(&format!(
                "push: {context} returned 409 Conflict; retrying in {} ms",
                backoff.as_millis()
            ));
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(CHUNK_BUSY_MAX_BACKOFF);
            continue;
        };

        let advertised =
            retry_after_header.max(std::time::Duration::from_millis(busy.retry_after_ms));
        let wait = advertised.max(backoff.min(CHUNK_BUSY_MAX_BACKOFF));
        let remaining_before_sleep = retry_state
            .overall_deadline
            .saturating_duration_since(tokio::time::Instant::now());
        let mut busy_reserved = retry_state.busy_wait_reserved.lock().await;
        let busy_remaining = retry_state.busy_wait_budget.saturating_sub(*busy_reserved);
        if wait > remaining_before_sleep || wait > busy_remaining {
            return Err(OakError::Server(format!(
                "{context} remained fenced by chunk object maintenance past its cumulative busy-wait budget; retry the push"
            )));
        }
        *busy_reserved += wait;
        drop(busy_reserved);
        tokio::time::sleep(wait).await;
        backoff = (backoff * 2).min(CHUNK_BUSY_MAX_BACKOFF);
    }
}

/// HTTP client dedicated to chunk uploads.
///
/// The default `reqwest::Client` negotiates HTTP/2 with R2/Cloudflare, which
/// multiplexes *every* concurrent upload over a single TCP connection. h2's
/// flow-control window (~64 KB) then caps aggregate throughput at
/// `window / RTT` no matter how many tasks we spawn — the bug that pinned
/// uploads to ~1.5 MiB/s. Forcing HTTP/1.1 gives each concurrent upload its
/// own pooled connection (each with its own window), so throughput scales with
/// `OAK_UPLOAD_CONCURRENCY` up to the real link ceiling. We keep up to
/// `concurrency` connections warm for reuse across the whole transfer.
pub(super) fn upload_client(concurrency: usize) -> reqwest::Client {
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

/// Encode chunks into the length-framed body the server's `/chunks/batch`
/// endpoint expects: `[u32 BE hash_len][hash][u32 BE data_len][data]` per
/// entry. Binary framing avoids the ~5x inflation of JSON number arrays.
fn encode_chunk_batch(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let cap: usize = entries.iter().map(|(h, d)| 8 + h.len() + d.len()).sum();
    let mut buf = Vec::with_capacity(cap);
    for (hash, data) in entries {
        buf.extend_from_slice(&(hash.len() as u32).to_be_bytes());
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
        buf.extend_from_slice(data);
    }
    buf
}

pub(crate) fn validate_chunk_check_missing(
    requested: &[String],
    returned: Vec<ChunkUploadInfo>,
    context: &str,
) -> Result<Vec<ChunkUploadInfo>> {
    let expected: HashSet<&str> = requested.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    for chunk in &returned {
        if !expected.contains(chunk.hash.as_str()) {
            return Err(OakError::InvalidArgument(format!(
                "{context} returned unrequested hash {}; no remote state was mutated",
                chunk.hash
            )));
        }
        if !seen.insert(chunk.hash.as_str()) {
            return Err(OakError::InvalidArgument(format!(
                "{context} returned duplicate hash {}; no remote state was mutated",
                chunk.hash
            )));
        }
    }
    Ok(returned)
}

fn with_auth(builder: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    match api_key {
        Some(key) => builder.header("authorization", format!("Bearer {key}")),
        None => builder,
    }
}

/// How one upload task left its chunks.
enum Uploaded {
    /// The server recorded the chunks itself (proxied PUT or bulk batch).
    Recorded { hashes: Vec<String>, bytes: u64 },
    /// The object is in storage; the server learns of it from
    /// `chunks/uploaded`.
    AwaitingConfirmation { hash: String, size: u64 },
    /// Not started: another upload of the page had already failed.
    Skipped,
}

/// The chunks one `upload` call owns in the session's chunk set. Whatever is
/// still open when the call ends — an error, or the future being dropped —
/// is released as failed so nothing waits on it forever.
struct Claims<'a> {
    session: &'a TransferSession,
    open: HashSet<String>,
}

impl Claims<'_> {
    fn claim(&mut self, hash: &str) -> Claim {
        let claim = self.session.claim(hash);
        if matches!(claim, Claim::Mine) {
            self.open.insert(hash.to_string());
        }
        claim
    }

    /// The server holds `hash`.
    fn held(&mut self, hash: &str) {
        if self.open.remove(hash) {
            self.session.settle(hash, true);
        }
    }
}

impl Drop for Claims<'_> {
    fn drop(&mut self) {
        for hash in self.open.drain() {
            self.session.settle(&hash, false);
        }
    }
}

/// Uploads the chunks a server is missing. One per destination repository
/// and deadline; every transport shares it.
pub(crate) struct ChunkUploader<'a> {
    pub(crate) remote: &'a str,
    pub(crate) endpoint_path: &'a str,
    pub(crate) api_key: Option<&'a str>,
    pub(crate) retry: Arc<ChunkBusyRetryState>,
    pub(crate) session: Arc<TransferSession>,
}

impl ChunkUploader<'_> {
    /// Make sure the server holds every chunk in `chunks` (`(hash, bytes)`,
    /// produced lazily): negotiate in bounded pages, upload only what is
    /// missing, acknowledge as uploads finish. Within the session a chunk is
    /// negotiated and uploaded once: a hash repeated here, or already owned
    /// by another uploader of the session, is waited for, not sent again.
    pub(crate) async fn upload<I>(&self, chunks: I) -> Result<()>
    where
        I: IntoIterator<Item = Result<(String, Vec<u8>)>>,
    {
        let mut claims = Claims {
            session: &self.session,
            open: HashSet::new(),
        };
        let mut elsewhere: Vec<String> = Vec::new();
        let mut page: HashMap<String, Vec<u8>> = HashMap::new();
        let mut page_bytes = 0usize;
        for chunk in chunks {
            let (hash, data) = chunk?;
            match claims.claim(&hash) {
                Claim::Done => continue,
                Claim::Elsewhere => {
                    elsewhere.push(hash);
                    continue;
                }
                Claim::Mine => {}
            }
            page_bytes += data.len();
            page.insert(hash, data);
            if page.len() >= NEGOTIATION_PAGE_CHUNKS || page_bytes >= NEGOTIATION_PAGE_BYTES {
                self.upload_page(std::mem::take(&mut page), &mut claims)
                    .await?;
                page_bytes = 0;
            }
        }
        if !page.is_empty() {
            self.upload_page(page, &mut claims).await?;
        }
        self.session.wait_for(&elsewhere).await
    }

    /// `/chunks/check`, then presigned direct-to-storage PUTs for large
    /// chunks (server-proxied when the server has no object storage),
    /// length-framed `/chunks/batch` for small ones, and `/chunks/uploaded`
    /// acknowledgements for the presigned uploads as they finish.
    async fn upload_page(
        &self,
        mut page: HashMap<String, Vec<u8>>,
        claims: &mut Claims<'_>,
    ) -> Result<()> {
        let hashes: Vec<String> = page.keys().cloned().collect();
        // Sizes parallel to the hashes — sent so the server can enforce the
        // organization storage quota before issuing presigned upload URLs.
        let sizes: Vec<u64> = hashes
            .iter()
            .map(|hash| page.get(hash).map_or(0, |data| data.len() as u64))
            .collect();
        let api = format!("{}/api/{}", self.remote, self.endpoint_path);

        output::vlog(&format!(
            "push: POST {api}/chunks/check ({} hashes)",
            hashes.len()
        ));
        let t_check = std::time::Instant::now();
        let check_request = with_auth(
            self.session
                .control_client
                .post(format!("{api}/chunks/check"))
                .json(&serde_json::json!({
                    "hashes": &hashes,
                    "sizes": sizes,
                    "chunk_batch_protocol": oak_core::protocol::CHUNK_BATCH_PROTOCOL,
                })),
            self.api_key,
        );
        let check_resp =
            send_chunk_request_with_busy_retry(check_request, self.retry.clone(), "chunk check")
                .await?;
        output::vlog(&format!(
            "push: chunks/check returned {} in {:.3}s",
            check_resp.status(),
            t_check.elapsed().as_secs_f64()
        ));
        if !check_resp.status().is_success() {
            let detail =
                chunk_error_text_with_deadline(check_resp, &self.retry, "chunk check").await;
            return Err(OakError::Server(format!(
                "Failed to check chunks: {detail}"
            )));
        }
        let check: ChunkCheckServerResponse =
            decode_chunk_response_json(check_resp, &self.retry, "chunk check").await?;
        let missing = validate_chunk_check_missing(&hashes, check.missing, "chunk check response")?;
        self.session
            .record_negotiated(hashes.len() as u64, (hashes.len() - missing.len()) as u64);
        {
            let wanted: HashSet<&str> = missing.iter().map(|info| info.hash.as_str()).collect();
            for hash in hashes.iter().filter(|hash| !wanted.contains(hash.as_str())) {
                claims.held(hash);
            }
        }
        if missing.is_empty() {
            output::vlog("push: all chunks of this page already on server");
            return Ok(());
        }

        // Partition missing chunks by size. Tiny objects (below the FastCDC
        // floor) would each pay a fixed round trip as a presigned PUT; they
        // ship in bulk to `/chunks/batch` instead, which records them
        // server-side. Large chunks keep the presigned-direct path, where the
        // per-request round trip amortizes over megabytes.
        let mut small_batches: Vec<Vec<(String, Vec<u8>)>> = Vec::new();
        let mut current: Vec<(String, Vec<u8>)> = Vec::new();
        let mut current_bytes = 0usize;
        let mut large: Vec<(String, Option<String>, Vec<u8>)> = Vec::new();
        for info in missing {
            let Some(data) = page.remove(&info.hash) else {
                continue;
            };
            if data.len() >= SMALL_CHUNK_THRESHOLD {
                large.push((info.hash, info.upload_url, data));
                continue;
            }
            // 8 bytes of framing per entry: two u32 length prefixes.
            let entry_bytes = 8 + info.hash.len() + data.len();
            if !current.is_empty()
                && (current_bytes + entry_bytes > BATCH_MAX_BYTES
                    || current.len() == BATCH_MAX_CHUNKS)
            {
                small_batches.push(std::mem::take(&mut current));
                current_bytes = 0;
            }
            current_bytes += entry_bytes;
            current.push((info.hash, data));
        }
        if !current.is_empty() {
            small_batches.push(current);
        }
        drop(page);

        output::vlog(&format!(
            "push: uploading {} large chunk(s) + {} small-chunk batch(es) (concurrency={})",
            large.len(),
            small_batches.len(),
            self.session.concurrency,
        ));
        let t_upload = std::time::Instant::now();

        // Set on the first failure: uploads still waiting for a slot do not
        // start, while those already in flight finish and are acknowledged,
        // so a retry skips everything that reached the server.
        let stop = Arc::new(AtomicBool::new(false));
        let mut uploads: JoinSet<Result<Uploaded>> = JoinSet::new();
        for (hash, upload_url, data) in large {
            uploads.spawn(upload_large_chunk(
                self.session.clone(),
                stop.clone(),
                self.retry.clone(),
                api.clone(),
                self.api_key.map(str::to_string),
                hash,
                upload_url,
                data,
            ));
        }
        for batch in small_batches {
            uploads.spawn(upload_small_batch(
                self.session.clone(),
                stop.clone(),
                self.retry.clone(),
                api.clone(),
                self.api_key.map(str::to_string),
                batch,
            ));
        }

        // Acknowledge presigned uploads in groups while the page uploads
        // (see `CONFIRM_GROUP_CHUNKS`), not once after the whole page.
        let mut confirms: JoinSet<Result<Vec<(String, u64)>>> = JoinSet::new();
        let mut unconfirmed: Vec<(String, u64)> = Vec::new();
        let mut oldest_unconfirmed: Option<tokio::time::Instant> = None;
        let mut failure: Option<OakError> = None;
        loop {
            while !unconfirmed.is_empty()
                && confirms.len() < CONFIRMS_IN_FLIGHT
                && (unconfirmed.len() >= CONFIRM_GROUP_CHUNKS
                    || uploads.is_empty()
                    || oldest_unconfirmed.is_some_and(|since| since.elapsed() >= CONFIRM_LINGER))
            {
                let take = unconfirmed.len().min(CONFIRM_GROUP_CHUNKS);
                let group: Vec<(String, u64)> = unconfirmed.drain(..take).collect();
                oldest_unconfirmed = (!unconfirmed.is_empty()).then(tokio::time::Instant::now);
                confirms.spawn(confirm_uploaded(
                    self.session.control_client.clone(),
                    self.retry.clone(),
                    api.clone(),
                    self.api_key.map(str::to_string),
                    group,
                ));
            }
            let linger_due = oldest_unconfirmed.map(|since| since + CONFIRM_LINGER);
            tokio::select! {
                Some(joined) = uploads.join_next() => match joined {
                    Ok(Ok(Uploaded::Recorded { hashes, bytes })) => {
                        for hash in &hashes {
                            claims.held(hash);
                        }
                        self.session.record_uploaded(hashes.len() as u64, bytes);
                    }
                    Ok(Ok(Uploaded::AwaitingConfirmation { hash, size })) => {
                        unconfirmed.push((hash, size));
                        oldest_unconfirmed.get_or_insert_with(tokio::time::Instant::now);
                    }
                    Ok(Ok(Uploaded::Skipped)) => {}
                    Ok(Err(error)) => {
                        failure.get_or_insert(error);
                        stop.store(true, Ordering::Relaxed);
                    }
                    Err(join_error) => {
                        failure.get_or_insert(OakError::Http(format!(
                            "chunk upload task failed: {join_error}"
                        )));
                        stop.store(true, Ordering::Relaxed);
                    }
                },
                Some(joined) = confirms.join_next() => match joined {
                    Ok(Ok(group)) => {
                        for (hash, _) in &group {
                            claims.held(hash);
                        }
                        self.session.record_uploaded(
                            group.len() as u64,
                            group.iter().map(|(_, size)| *size).sum(),
                        );
                    }
                    Ok(Err(error)) => {
                        failure.get_or_insert(error);
                        stop.store(true, Ordering::Relaxed);
                    }
                    Err(join_error) => {
                        failure.get_or_insert(OakError::Http(format!(
                            "chunk confirmation task failed: {join_error}"
                        )));
                        stop.store(true, Ordering::Relaxed);
                    }
                },
                _ = async {
                    match linger_due {
                        Some(due) => tokio::time::sleep_until(due).await,
                        None => std::future::pending().await,
                    }
                }, if linger_due.is_some() && confirms.len() < CONFIRMS_IN_FLIGHT => {}
                else => break,
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        output::vlog(&format!(
            "push: chunk uploads done in {:.3}s",
            t_upload.elapsed().as_secs_f64()
        ));
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_large_chunk(
    session: Arc<TransferSession>,
    stop: Arc<AtomicBool>,
    retry: Arc<ChunkBusyRetryState>,
    api: String,
    api_key: Option<String>,
    hash: String,
    upload_url: Option<String>,
    data: Vec<u8>,
) -> Result<Uploaded> {
    let _permit = session
        .upload_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|error| OakError::Http(error.to_string()))?;
    if stop.load(Ordering::Relaxed) {
        return Ok(Uploaded::Skipped);
    }
    let size = data.len() as u64;
    let started = std::time::Instant::now();
    let short = &hash[..hash.len().min(8)];
    let rate = |seconds: f64| (size as f64 / 1024.0) / seconds.max(0.001);

    if let Some(url) = upload_url {
        // Direct to object storage via a presigned URL. The server never
        // sees these bytes (so it cannot compress them for us); compress
        // here. Download paths detect-and-decode.
        let body = oak_core::chunk_encode(&data);
        drop(data);
        let response = send_presigned_chunk_put(&session.upload_client, &url, body, &retry).await?;
        if !response.status().is_success() {
            let detail =
                chunk_error_text_with_deadline(response, &retry, "presigned chunk upload").await;
            return Err(OakError::Server(format!(
                "Failed to upload chunk to R2: {detail}"
            )));
        }
        let seconds = started.elapsed().as_secs_f64();
        output::vlog(&format!(
            "push: chunk {short} ({size} B) -> R2 in {seconds:.3}s ({:.0} KiB/s)",
            rate(seconds)
        ));
        return Ok(Uploaded::AwaitingConfirmation { hash, size });
    }

    // No object storage — upload through the server, which verifies and
    // records the chunk before it answers.
    let request = with_auth(
        session
            .upload_client
            .put(format!("{api}/chunks/{hash}"))
            .header("content-type", "application/octet-stream")
            .body(data),
        api_key.as_deref(),
    );
    let response =
        send_chunk_request_with_busy_retry(request, retry.clone(), "server chunk upload").await?;
    if !response.status().is_success() {
        let detail = chunk_error_text_with_deadline(response, &retry, "server chunk upload").await;
        return Err(OakError::Server(format!(
            "Failed to upload chunk: {detail}"
        )));
    }
    let seconds = started.elapsed().as_secs_f64();
    output::vlog(&format!(
        "push: chunk {short} ({size} B) -> server in {seconds:.3}s ({:.0} KiB/s)",
        rate(seconds)
    ));
    Ok(Uploaded::Recorded {
        hashes: vec![hash],
        bytes: size,
    })
}

async fn upload_small_batch(
    session: Arc<TransferSession>,
    stop: Arc<AtomicBool>,
    retry: Arc<ChunkBusyRetryState>,
    api: String,
    api_key: Option<String>,
    batch: Vec<(String, Vec<u8>)>,
) -> Result<Uploaded> {
    let _permit = session
        .upload_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(|error| OakError::Http(error.to_string()))?;
    if stop.load(Ordering::Relaxed) {
        return Ok(Uploaded::Skipped);
    }
    let chunks = batch.len() as u64;
    let bytes: u64 = batch.iter().map(|(_, data)| data.len() as u64).sum();
    let body = encode_chunk_batch(&batch);
    let hashes: Vec<String> = batch.into_iter().map(|(hash, _)| hash).collect();
    let started = std::time::Instant::now();
    let request = with_auth(
        session
            .upload_client
            .post(format!("{api}/chunks/batch"))
            .header("content-type", "application/octet-stream")
            .body(body),
        api_key.as_deref(),
    );
    let response =
        send_chunk_request_with_busy_retry(request, retry.clone(), "chunk batch upload").await?;
    if !response.status().is_success() {
        let detail = chunk_error_text_with_deadline(response, &retry, "chunk batch upload").await;
        return Err(OakError::Server(format!(
            "Failed to upload chunk batch: {detail}"
        )));
    }
    output::vlog(&format!(
        "push: batch of {chunks} chunk(s) ({bytes} B) -> server in {:.3}s",
        started.elapsed().as_secs_f64(),
    ));
    Ok(Uploaded::Recorded { hashes, bytes })
}

/// Tell the server which presigned uploads finished so it verifies and
/// records them. Returns the acknowledged group.
async fn confirm_uploaded(
    client: reqwest::Client,
    retry: Arc<ChunkBusyRetryState>,
    api: String,
    api_key: Option<String>,
    group: Vec<(String, u64)>,
) -> Result<Vec<(String, u64)>> {
    let entries: Vec<serde_json::Value> = group
        .iter()
        .map(|(hash, size)| serde_json::json!({ "hash": hash, "size": size }))
        .collect();
    let started = std::time::Instant::now();
    let request = with_auth(
        client
            .post(format!("{api}/chunks/uploaded"))
            .json(&serde_json::json!({ "hashes": entries })),
        api_key.as_deref(),
    );
    let response = send_chunk_request_retrying_conflicts(
        request,
        retry.clone(),
        "chunk upload confirmation",
        CONFIRM_CONFLICT_RETRIES,
    )
    .await?;
    if !response.status().is_success() {
        let detail =
            chunk_error_text_with_deadline(response, &retry, "chunk upload confirmation").await;
        return Err(OakError::Server(format!(
            "Failed to confirm chunk uploads: {detail}"
        )));
    }
    output::vlog(&format!(
        "push: confirmed {} uploaded chunk(s) in {:.3}s",
        group.len(),
        started.elapsed().as_secs_f64()
    ));
    Ok(group)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn chunk(seed: u8, len: usize) -> (String, Vec<u8>) {
        let data: Vec<u8> = (0..len)
            .map(|i| (i as u8).wrapping_mul(31) ^ seed)
            .collect();
        (oak_core::hash_bytes(&data).to_string(), data)
    }

    /// A stand-in for the hosted chunk API with object storage: a chunk is
    /// "present" only once `chunks/uploaded` confirmed it, exactly the rule
    /// that made an interrupted push re-send everything.
    #[derive(Clone, Default)]
    struct FakeChunkServer {
        state: Arc<Mutex<FakeState>>,
    }

    #[derive(Default)]
    struct FakeState {
        base: String,
        confirmed: HashSet<String>,
        stored: HashSet<String>,
        puts: Vec<String>,
        confirm_groups: Vec<Vec<String>>,
        batch_requests: usize,
        fail_put: Option<String>,
        /// Delay every PUT by this long; `slow_put` delays one chunk more.
        put_delay_ms: u64,
        slow_put: Option<(String, u64)>,
        /// Answer this many confirmations with the server's non-busy 409.
        confirm_conflicts: usize,
        /// Objects in storage when each confirmation arrived.
        stored_at_confirm: Vec<usize>,
    }

    struct Check(FakeChunkServer);
    impl Respond for Check {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let state = self.0.state.lock().unwrap();
            let missing: Vec<serde_json::Value> = body["hashes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|hash| hash.as_str().unwrap().to_string())
                .filter(|hash| !state.confirmed.contains(hash))
                .map(|hash| {
                    serde_json::json!({
                        "hash": hash,
                        "upload_url": format!("{}/r2/{hash}", state.base),
                    })
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "missing": missing }))
        }
    }

    struct Put(FakeChunkServer);
    impl Respond for Put {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let hash = request.url.path().rsplit('/').next().unwrap().to_string();
            let mut state = self.0.state.lock().unwrap();
            if state.fail_put.as_deref() == Some(hash.as_str()) {
                return ResponseTemplate::new(403).set_body_string("denied");
            }
            state.puts.push(hash.clone());
            let delay = match &state.slow_put {
                Some((slow, extra)) if *slow == hash => state.put_delay_ms + extra,
                _ => state.put_delay_ms,
            };
            state.stored.insert(hash);
            ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(delay))
        }
    }

    struct Confirm(FakeChunkServer);
    impl Respond for Confirm {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let group: Vec<String> = body["hashes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["hash"].as_str().unwrap().to_string())
                .collect();
            let mut state = self.0.state.lock().unwrap();
            if state.confirm_conflicts > 0 {
                state.confirm_conflicts -= 1;
                return ResponseTemplate::new(409).set_body_json(serde_json::json!({
                    "error": "chunk receipt changed during direct-upload proof; retry"
                }));
            }
            let stored = state.stored.len();
            state.stored_at_confirm.push(stored);
            if group.iter().any(|hash| !state.stored.contains(hash)) {
                return ResponseTemplate::new(400).set_body_string("chunk object missing");
            }
            state.confirmed.extend(group.iter().cloned());
            state.confirm_groups.push(group);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true }))
        }
    }

    struct Batch(FakeChunkServer);
    impl Respond for Batch {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let mut state = self.0.state.lock().unwrap();
            state.batch_requests += 1;
            let mut body = request.body.as_slice();
            while !body.is_empty() {
                let hash_len = u32::from_be_bytes(body[..4].try_into().unwrap()) as usize;
                let hash = String::from_utf8(body[4..4 + hash_len].to_vec()).unwrap();
                body = &body[4 + hash_len..];
                let data_len = u32::from_be_bytes(body[..4].try_into().unwrap()) as usize;
                assert_eq!(
                    oak_core::hash_bytes(&body[4..4 + data_len]).to_string(),
                    hash
                );
                body = &body[4 + data_len..];
                // The batch endpoint verifies and records in one step.
                state.stored.insert(hash.clone());
                state.confirmed.insert(hash);
            }
            ResponseTemplate::new(200)
        }
    }

    async fn fake_server() -> (MockServer, FakeChunkServer) {
        let server = MockServer::start().await;
        let fake = FakeChunkServer::default();
        fake.state.lock().unwrap().base = server.uri();
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/chunks/check"))
            .respond_with(Check(fake.clone()))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex("^/r2/[0-9a-f]{64}$"))
            .respond_with(Put(fake.clone()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/chunks/uploaded"))
            .respond_with(Confirm(fake.clone()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/chunks/batch"))
            .respond_with(Batch(fake.clone()))
            .mount(&server)
            .await;
        (server, fake)
    }

    async fn upload(
        server: &MockServer,
        chunks: &[(String, Vec<u8>)],
    ) -> (Result<()>, crate::transfer::TransferCounts) {
        let session = TransferSession::new();
        let result = upload_in(server, &session, chunks).await;
        (result, session.counts())
    }

    async fn upload_in(
        server: &MockServer,
        session: &Arc<TransferSession>,
        chunks: &[(String, Vec<u8>)],
    ) -> Result<()> {
        let remote = server.uri();
        let uploader = ChunkUploader {
            remote: &remote,
            endpoint_path: "oak/oak",
            api_key: Some("secret"),
            retry: ChunkBusyRetryState::for_upload(0),
            session: session.clone(),
        };
        uploader.upload(chunks.iter().cloned().map(Ok)).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uploads_only_what_the_server_lacks_and_acknowledges_incrementally() {
        let (server, fake) = fake_server().await;
        // 40 large chunks (two negotiation pages), 5 small ones, one repeat.
        let mut chunks: Vec<(String, Vec<u8>)> = (0..40)
            .map(|i| chunk(i, SMALL_CHUNK_THRESHOLD + i as usize))
            .collect();
        chunks.extend((0..5).map(|i| chunk(100 + i, 1000 + i as usize)));
        chunks.push(chunks[0].clone());
        // The server already holds three of the large chunks.
        for (hash, _) in &chunks[..3] {
            let mut state = fake.state.lock().unwrap();
            state.stored.insert(hash.clone());
            state.confirmed.insert(hash.clone());
        }

        let (result, counts) = upload(&server, &chunks).await;
        result.unwrap();

        let state = fake.state.lock().unwrap();
        assert_eq!(
            counts.chunks_required, 45,
            "a repeated hash is negotiated once"
        );
        assert_eq!(counts.chunks_already_present, 3);
        assert_eq!(counts.chunks_uploaded, 42);
        let expected_bytes: u64 = chunks[3..45]
            .iter()
            .map(|(_, data)| data.len() as u64)
            .sum();
        assert_eq!(counts.bytes_uploaded, expected_bytes);
        assert_eq!(state.puts.len(), 37, "present chunks are never re-sent");
        assert!(chunks[..3]
            .iter()
            .all(|(hash, _)| !state.puts.contains(hash)));
        assert_eq!(
            state.batch_requests, 1,
            "small chunks travel in one bulk request"
        );
        // Every presigned upload was acknowledged exactly once, in groups no
        // larger than the bound — a handful of requests, not one per chunk.
        let confirmed: usize = state.confirm_groups.iter().map(Vec::len).sum();
        assert_eq!(confirmed, 37);
        assert!(state
            .confirm_groups
            .iter()
            .all(|group| !group.is_empty() && group.len() <= CONFIRM_GROUP_CHUNKS));
        assert!(
            (37_usize.div_ceil(CONFIRM_GROUP_CHUNKS)..=6).contains(&state.confirm_groups.len()),
            "{} confirmation requests for 37 chunks in 2 pages",
            state.confirm_groups.len()
        );
        assert!(chunks
            .iter()
            .all(|(hash, _)| state.confirmed.contains(hash)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_interrupted_upload_resumes_with_only_unacknowledged_chunks() {
        let (server, fake) = fake_server().await;
        let chunks: Vec<(String, Vec<u8>)> = (0..24)
            .map(|i| chunk(i, SMALL_CHUNK_THRESHOLD + 64 + i as usize))
            .collect();
        // Object storage refuses one chunk: the transfer fails part-way.
        let poisoned = chunks[17].0.clone();
        fake.state.lock().unwrap().fail_put = Some(poisoned.clone());

        let (result, first) = upload(&server, &chunks).await;
        let error = result.unwrap_err();
        assert!(error.to_string().contains("Failed to upload chunk to R2"));

        // Whatever finished uploading before the failure was acknowledged —
        // nothing that reached storage was left unconfirmed — and the counts
        // report exactly the acknowledged chunks.
        let (acknowledged, first_puts) = {
            let state = fake.state.lock().unwrap();
            assert!(state.puts.iter().all(|hash| state.confirmed.contains(hash)));
            assert!(!state.confirmed.contains(&poisoned));
            (state.confirmed.clone(), state.puts.len())
        };
        assert_eq!(first.chunks_uploaded as usize, acknowledged.len());
        assert_eq!(first.chunks_required, 24);

        // The retry sends only what was never acknowledged.
        fake.state.lock().unwrap().fail_put = None;
        let (result, second) = upload(&server, &chunks).await;
        result.unwrap();
        let state = fake.state.lock().unwrap();
        assert_eq!(second.chunks_required, 24);
        assert_eq!(second.chunks_already_present as usize, acknowledged.len());
        assert_eq!(second.chunks_uploaded as usize, 24 - acknowledged.len());
        let retry_puts = &state.puts[first_puts..];
        assert_eq!(retry_puts.len(), 24 - acknowledged.len());
        assert!(retry_puts.iter().all(|hash| !acknowledged.contains(hash)));
        assert_eq!(state.confirmed.len(), 24);
    }

    /// Item: a chunk needed by several uploaders of one session (concurrent
    /// preload batches of near-identical files) is negotiated, uploaded and
    /// acknowledged once, and counted once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_uploaders_of_one_session_send_a_shared_chunk_once() {
        let (server, fake) = fake_server().await;
        fake.state.lock().unwrap().put_delay_ms = 40;
        let shared: Vec<(String, Vec<u8>)> = (0..20)
            .map(|i| chunk(i, SMALL_CHUNK_THRESHOLD + 10 + i as usize))
            .collect();
        let batches: Vec<Vec<(String, Vec<u8>)>> = (0..4u8)
            .map(|batch| {
                let mut chunks = shared.clone();
                chunks.push(chunk(200 + batch, SMALL_CHUNK_THRESHOLD + 500));
                // Different orders, so every batch meets chunks owned elsewhere.
                chunks.rotate_left(batch as usize * 5);
                chunks
            })
            .collect();

        let session = TransferSession::new();
        let results = futures_util::future::join_all(
            batches
                .iter()
                .map(|batch| upload_in(&server, &session, batch)),
        )
        .await;
        for result in results {
            result.unwrap();
        }

        let state = fake.state.lock().unwrap();
        assert_eq!(state.puts.len(), 24, "20 shared + 4 distinct chunks");
        let unique: HashSet<&String> = state.puts.iter().collect();
        assert_eq!(unique.len(), 24, "no chunk is uploaded twice");
        let confirmed: usize = state.confirm_groups.iter().map(Vec::len).sum();
        assert_eq!(confirmed, 24, "no chunk is acknowledged twice");
        assert_eq!(
            session.counts(),
            crate::transfer::TransferCounts {
                chunks_required: 24,
                chunks_already_present: 0,
                chunks_uploaded: 24,
                bytes_uploaded: state
                    .puts
                    .iter()
                    .map(|hash| {
                        batches
                            .iter()
                            .flatten()
                            .find(|(candidate, _)| candidate == hash)
                            .unwrap()
                            .1
                            .len() as u64
                    })
                    .sum(),
            },
            "the receipt counts unique chunks"
        );
    }

    /// An uploader waiting on a chunk another uploader owns fails with it
    /// instead of publishing a mapping whose chunk never arrived.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_shared_chunk_fails_every_uploader_that_needed_it() {
        let (server, fake) = fake_server().await;
        let shared = chunk(1, SMALL_CHUNK_THRESHOLD + 1);
        {
            let mut state = fake.state.lock().unwrap();
            state.put_delay_ms = 50;
            state.fail_put = Some(shared.0.clone());
        }
        let first = vec![shared.clone(), chunk(2, SMALL_CHUNK_THRESHOLD + 2)];
        let second = vec![chunk(3, SMALL_CHUNK_THRESHOLD + 3), shared.clone()];
        let session = TransferSession::new();
        let (a, b) = tokio::join!(
            upload_in(&server, &session, &first),
            upload_in(&server, &session, &second)
        );
        assert!(a.is_err() || b.is_err());
        assert!(
            a.is_err() && b.is_err(),
            "both uploaders needed the failed chunk: {a:?} / {b:?}"
        );
        // The failed chunk may be claimed again by a retry in the session.
        fake.state.lock().unwrap().fail_put = None;
        upload_in(&server, &session, &first).await.unwrap();
        assert!(fake.state.lock().unwrap().confirmed.contains(&shared.0));
    }

    /// Acknowledgement does not wait for the slowest upload of a page: what
    /// finished is confirmed once the linger elapses.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finished_uploads_are_acknowledged_while_a_slow_one_is_still_running() {
        let (server, fake) = fake_server().await;
        let chunks: Vec<(String, Vec<u8>)> = (0..6)
            .map(|i| chunk(i, SMALL_CHUNK_THRESHOLD + 7 + i as usize))
            .collect();
        let linger_ms = CONFIRM_LINGER.as_millis() as u64;
        fake.state.lock().unwrap().slow_put = Some((chunks[5].0.clone(), linger_ms + 1500));

        let (result, counts) = upload(&server, &chunks).await;
        result.unwrap();

        let state = fake.state.lock().unwrap();
        assert_eq!(counts.chunks_uploaded, 6);
        assert_eq!(state.confirm_groups.len(), 2, "{:?}", state.confirm_groups);
        assert_eq!(state.confirm_groups[0].len(), 5);
        assert!(!state.confirm_groups[0].contains(&chunks[5].0));
        assert_eq!(state.confirm_groups[1], vec![chunks[5].0.clone()]);
    }

    /// `chunks/uploaded` is idempotent; the server's "receipt changed;
    /// retry" 409 (another client confirming the same chunk) is retried.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_non_busy_conflict_on_confirmation_is_retried() {
        let (server, fake) = fake_server().await;
        let chunks: Vec<(String, Vec<u8>)> = (0..3)
            .map(|i| chunk(i, SMALL_CHUNK_THRESHOLD + 3 + i as usize))
            .collect();
        fake.state.lock().unwrap().confirm_conflicts = 2;
        let (result, counts) = upload(&server, &chunks).await;
        result.unwrap();
        assert_eq!(counts.chunks_uploaded, 3);
        assert_eq!(fake.state.lock().unwrap().confirmed.len(), 3);

        // A conflict that never clears is an error, not an endless loop.
        let more: Vec<(String, Vec<u8>)> = (10..12)
            .map(|i| chunk(i, SMALL_CHUNK_THRESHOLD + 3 + i as usize))
            .collect();
        fake.state.lock().unwrap().confirm_conflicts = CONFIRM_CONFLICT_RETRIES + 1;
        let (result, counts) = upload(&server, &more).await;
        let error = result.unwrap_err();
        assert!(error.to_string().contains("409 Conflict"), "{error}");
        assert_eq!(
            counts.chunks_uploaded, 0,
            "nothing unacknowledged is counted"
        );
    }

    #[test]
    fn chunk_check_missing_must_be_a_unique_subset_of_the_request_page() {
        let requested = vec!["aa".repeat(32), "bb".repeat(32)];
        let upload = |hash: String| ChunkUploadInfo {
            hash,
            upload_url: None,
        };

        let unknown =
            validate_chunk_check_missing(&requested, vec![upload("cc".repeat(32))], "chunk check")
                .err()
                .unwrap();
        assert!(unknown.to_string().contains("unrequested hash"));

        let duplicate = validate_chunk_check_missing(
            &requested,
            vec![upload(requested[0].clone()), upload(requested[0].clone())],
            "chunk check",
        )
        .err()
        .unwrap();
        assert!(duplicate.to_string().contains("duplicate hash"));

        let mixed = validate_chunk_check_missing(
            &requested,
            vec![upload(requested[0].clone()), upload("cc".repeat(32))],
            "chunk check",
        )
        .err()
        .unwrap();
        assert!(mixed.to_string().contains("unrequested hash"));

        let valid = validate_chunk_check_missing(
            &requested,
            vec![upload(requested[1].clone())],
            "chunk check",
        )
        .unwrap();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].hash, requested[1]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chunk_check_replays_exact_payload_after_typed_object_busy_conflict() {
        use wiremock::matchers::{body_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = serde_json::json!({
            "hashes": ["a".repeat(64)],
            "sizes": [3],
            "chunk_batch_protocol": oak_core::protocol::CHUNK_BATCH_PROTOCOL,
        });
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/chunks/check"))
            .and(header("authorization", "Bearer secret"))
            .and(body_json(body.clone()))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "0")
                    .set_body_string("admission busy"),
            )
            .with_priority(1)
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/chunks/check"))
            .and(header("authorization", "Bearer secret"))
            .and(body_json(body.clone()))
            .respond_with(
                ResponseTemplate::new(409)
                    .insert_header("Retry-After", "1")
                    .set_body_json(serde_json::json!({
                        "error": "chunk_object_busy",
                        "retry_after_ms": 250
                    })),
            )
            .with_priority(2)
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/oak/oak/chunks/check"))
            .and(header("authorization", "Bearer secret"))
            .and(body_json(body.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "missing": []
            })))
            .with_priority(3)
            .expect(1)
            .mount(&server)
            .await;

        let response = send_chunk_request_with_busy_retry(
            crate::http::api_client()
                .post(format!("{}/api/oak/oak/chunks/check", server.uri()))
                .header("authorization", "Bearer secret")
                .json(&body),
            std::sync::Arc::new(ChunkBusyRetryState::new(
                tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                std::time::Duration::from_secs(3),
            )),
            "chunk check",
        )
        .await
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chunk_busy_retry_never_sleeps_past_remaining_overall_deadline() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/busy"))
            .respond_with(
                ResponseTemplate::new(409)
                    .insert_header("Retry-After", "1")
                    .set_body_json(serde_json::json!({
                        "error": "chunk_object_busy",
                        "retry_after_ms": 250
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let started = std::time::Instant::now();
        let error = send_chunk_request_with_busy_retry(
            crate::http::api_client().post(format!("{}/busy", server.uri())),
            std::sync::Arc::new(ChunkBusyRetryState::new(
                tokio::time::Instant::now() + std::time::Duration::from_millis(100),
                std::time::Duration::from_secs(60),
            )),
            "chunk upload",
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("busy-wait budget"));
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_never_busy_chunk_request_does_not_consume_busy_wait_budget() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/slow"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(100)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let state = std::sync::Arc::new(ChunkBusyRetryState::new(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            std::time::Duration::ZERO,
        ));
        let response = send_chunk_request_with_busy_retry(
            crate::http::api_client().put(format!("{}/slow", server.uri())),
            state.clone(),
            "chunk upload",
        )
        .await
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            *state.busy_wait_reserved.lock().await,
            std::time::Duration::ZERO
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn successful_chunk_check_body_is_bounded_by_the_overall_deadline() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1024\r\n\r\n{",
                )
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        });
        let response = crate::http::api_client()
            .get(format!("http://{address}/withheld-body"))
            .send()
            .await
            .unwrap();
        let state = ChunkBusyRetryState::new(
            tokio::time::Instant::now() + std::time::Duration::from_millis(40),
            std::time::Duration::ZERO,
        );
        let error =
            decode_chunk_response_json::<serde_json::Value>(response, &state, "chunk check")
                .await
                .unwrap_err();
        assert!(error.to_string().contains("response body exceeded"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_presigned_put_uses_transfer_deadline_not_busy_budget() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/presigned"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(100)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let state = ChunkBusyRetryState::new(
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            std::time::Duration::ZERO,
        );
        let response = send_presigned_chunk_put(
            &crate::http::api_client(),
            &format!("{}/presigned", server.uri()),
            vec![7; 1024],
            &state,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            *state.busy_wait_reserved.lock().await,
            std::time::Duration::ZERO
        );
    }
}
