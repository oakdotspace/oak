//! Lazy blob fetch for on-demand hydration.
//!
//! A mount (or a scoped `--team`/`--project` clone) only materializes the
//! blobs it needs up front — `oak pull` fetches blobs matching the active
//! scope's filter, so blobs outside that scope are absent locally until
//! something references them.
//!
//! This module provides [`ensure_blobs_local`], which fills those gaps on
//! demand using two server endpoints:
//!
//! 1. `POST /api/:owner/:name/blobs/info` — returns blob metadata (size +
//!    chunk list) for a batch of blob hashes.
//! 2. `POST /api/:owner/:name/chunks/download` — returns presigned URLs or
//!    inline content for a batch of chunk hashes.
//!
//! Chunks are fetched concurrently; blobs are then reassembled and stored.
//! The whole operation is a no-op if every requested blob is already local.
//!
//! Requires commits and manifests for the dep pins to already be present
//! locally (pulled as part of a normal `oak pull`). A dedicated "fetch a
//! commit by hash" endpoint is out of scope for this helper.

use std::sync::Arc;

use futures_util::StreamExt;
use oak_core::protocol::{CommitData, TreeData};
use oak_core::{reassemble_chunks, Blob, ChunkInfo, Hash, Manifest, OakError, Result};
use oak_core::{Repository, SqliteRepository};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// Process-wide HTTP client. A `reqwest::Client` owns a connection pool with
/// keep-alive; creating a fresh one per fetch (as this module used to) forced a
/// new TLS handshake to the remote on every cold blob read — the dominant cost
/// of `oak mount` lazy hydration. Sharing one client lets sequential reads reuse
/// the warmed TLS connection. Cheap to clone (it's `Arc` inside).
fn shared_client() -> reqwest::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(crate::http::api_client).clone()
}

use super::pull::{download_client, max_concurrent_downloads};

#[derive(Serialize)]
struct BlobInfoRequest<'a> {
    hashes: &'a [String],
}

#[derive(Deserialize, Clone)]
struct ChunkRef {
    hash: String,
    offset: u64,
    size: u32,
}

#[derive(Deserialize)]
struct BlobData {
    hash: String,
    #[serde(default)]
    content: Vec<u8>,
    /// Wire field; the stored size is recomputed from the verified content
    /// (`verify_blob_content`), so nothing reads it anymore.
    #[allow(dead_code)]
    size: u64,
    #[serde(default)]
    chunks: Vec<ChunkRef>,
}

#[derive(Deserialize)]
struct LegacyPullBranch {
    name: String,
}

#[derive(Deserialize)]
struct LegacyPullBlobInfo {
    branch: Option<LegacyPullBranch>,
    #[serde(default)]
    blobs: Vec<BlobData>,
}

const LEGACY_PULL_PROOF_MAX_BYTES: usize = 64 * 1024 * 1024;
const LEGACY_PULL_PROOF_MAX_BLOBS: usize = oak_core::protocol::STAGED_MAX_BLOBS;

fn legacy_pull_envelope_error(reason: impl std::fmt::Display) -> OakError {
    OakError::Server(format!(
        "legacy blob hydration {reason}; upgrade the server to one with /blobs/info and retry"
    ))
}

async fn legacy_pull_blob_info(
    client: &reqwest::Client,
    remote_url: &str,
    owner: &str,
    repo_name: &str,
    branch_name: &str,
    api_key: Option<&str>,
) -> Result<Vec<BlobData>> {
    let mut url = reqwest::Url::parse(&format!("{remote_url}/api/{owner}/{repo_name}/pull"))
        .map_err(|error| OakError::InvalidArgument(error.to_string()))?;
    url.query_pairs_mut()
        .append_pair("branch_name", branch_name);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let response = crate::http::send_idempotent_with_retry_until(
        with_auth(client.get(url), api_key),
        "legacy blob hydration pull",
        deadline,
    )
    .await?;
    if !response.status().is_success() {
        return Err(OakError::Server(format!(
            "legacy blob hydration pull failed: {}",
            crate::http::error_text(response).await
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > LEGACY_PULL_PROOF_MAX_BYTES as u64)
    {
        return Err(legacy_pull_envelope_error(format!(
            "pull exceeds the {} MiB compatibility envelope",
            LEGACY_PULL_PROOF_MAX_BYTES / (1024 * 1024)
        )));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(legacy_pull_envelope_error(
                "pull exceeded its 60 second compatibility deadline",
            ));
        }
        let frame = tokio::time::timeout(remaining, stream.next())
            .await
            .map_err(|_| {
                legacy_pull_envelope_error("pull exceeded its 60 second compatibility deadline")
            })?;
        let Some(frame) = frame else { break };
        let frame = frame.map_err(|error| OakError::Http(error.to_string()))?;
        if body.len().saturating_add(frame.len()) > LEGACY_PULL_PROOF_MAX_BYTES {
            return Err(legacy_pull_envelope_error(format!(
                "pull exceeds the {} MiB compatibility envelope",
                LEGACY_PULL_PROOF_MAX_BYTES / (1024 * 1024)
            )));
        }
        body.extend_from_slice(&frame);
    }
    let pull: LegacyPullBlobInfo =
        serde_json::from_slice(&body).map_err(|error| OakError::Http(error.to_string()))?;
    if pull.branch.is_none() {
        return Err(legacy_pull_envelope_error(format!(
            "pull returned branch:null for unpublished branch {branch_name}; deploy the fixed server first because v0.102.1 cannot expose repo-wide blob mappings through a branch that does not exist"
        )));
    }
    if pull.branch.as_ref().map(|branch| branch.name.as_str()) != Some(branch_name) {
        return Err(legacy_pull_envelope_error(format!(
            "pull returned a different branch than {branch_name}"
        )));
    }
    if pull.blobs.len() > LEGACY_PULL_PROOF_MAX_BLOBS {
        return Err(legacy_pull_envelope_error(format!(
            "pull returned {} blobs (safe limit {LEGACY_PULL_PROOF_MAX_BLOBS})",
            pull.blobs.len()
        )));
    }
    Ok(pull.blobs)
}

#[derive(Deserialize)]
struct BlobInfoResponse {
    blobs: Vec<BlobData>,
    /// Requested hashes the server withheld under path-based permissions
    /// (as opposed to hashes it doesn't know). Older servers omit this.
    #[serde(default)]
    restricted: Vec<String>,
}

#[derive(Serialize)]
struct ChunkDownloadRequest<'a> {
    hashes: &'a [String],
    chunk_batch_protocol: &'static str,
}

#[derive(Deserialize)]
struct ChunkDownloadInfo {
    hash: String,
    download_url: Option<String>,
    content: Option<Vec<u8>>,
}

#[derive(Deserialize)]
struct ChunkDownloadResponse {
    chunks: Vec<ChunkDownloadInfo>,
    /// Requested chunk hashes withheld under path-based permissions.
    #[serde(default)]
    restricted: Vec<String>,
}

fn with_auth(builder: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    if let Some(key) = api_key {
        builder.header("authorization", format!("Bearer {key}"))
    } else {
        builder
    }
}

// ---------------------------------------------------------------------------
// Commit + manifest fetch
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct CommitInfoRequest<'a> {
    hashes: &'a [String],
}

#[derive(Deserialize)]
struct CommitInfoResponse {
    commits: Vec<CommitData>,
    #[serde(default)]
    trees: Vec<TreeData>,
}

fn validate_exact_hash_response<'a>(
    requested: &[String],
    returned: impl IntoIterator<Item = &'a str>,
    context: &str,
) -> Result<()> {
    validate_hash_response_allowing(requested, returned, &[], context)
}

fn validate_hash_response_allowing<'a>(
    requested: &[String],
    returned: impl IntoIterator<Item = &'a str>,
    allowed_unrequested: &[&str],
    context: &str,
) -> Result<()> {
    let expected: std::collections::HashSet<&str> = requested.iter().map(String::as_str).collect();
    let allowed: std::collections::HashSet<&str> = allowed_unrequested.iter().copied().collect();
    let mut received = std::collections::HashSet::new();
    for hash in returned {
        if !expected.contains(hash) && !allowed.contains(hash) {
            return Err(OakError::Server(format!(
                "{context} returned unrequested hash {hash}"
            )));
        }
        if !received.insert(hash) {
            return Err(OakError::Server(format!(
                "{context} returned duplicate hash {hash}"
            )));
        }
    }
    let mut missing: Vec<_> = expected.difference(&received).copied().collect();
    missing.sort_unstable();
    if let Some(hash) = missing.first() {
        return Err(OakError::Server(format!(
            "{context} omitted requested hash {hash}"
        )));
    }
    Ok(())
}

/// Ensure each commit in `commit_hashes` is present locally, fetching missing
/// commits (and their manifests) from the remote. Does not touch blobs.
///
/// Used during lazy mount/pull hydration so an operation can reference any
/// commit the server knows about, not just commits the user has pulled.
pub async fn ensure_commits_local(
    repo: &SqliteRepository,
    remote_url: &str,
    owner: &str,
    repo_name: &str,
    api_key: Option<&str>,
    commit_hashes: &[Hash],
) -> Result<()> {
    let mut missing: Vec<String> = Vec::new();
    for hash in commit_hashes {
        match repo.get_commit(hash)? {
            Some(commit) if repo.get_manifest(&commit.manifest_hash)?.is_some() => {}
            Some(commit) if commit.manifest_hash == Manifest::empty().hash => {
                repo.store_manifest(&Manifest::empty())?;
            }
            _ => missing.push(hash.as_str().to_string()),
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort();
    missing.dedup();

    let client = shared_client();
    let url = format!("{remote_url}/api/{owner}/{repo_name}/commits/info");
    let resp = with_auth(
        client
            .post(&url)
            .json(&CommitInfoRequest { hashes: &missing }),
        api_key,
    )
    .send()
    .await
    .map_err(|e| OakError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(OakError::Server(format!(
            "commit info request failed: {}",
            crate::http::error_text(resp).await
        )));
    }

    let info: CommitInfoResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    validate_exact_hash_response(
        &missing,
        info.commits.iter().map(|commit| commit.hash.as_str()),
        "commit info response",
    )?;

    // Decode and validate the entire response before touching the local
    // repository. A valid commit DTO is not sufficient: its root tree and
    // every transitive subtree must be present exactly once, and the response
    // must not smuggle in trees unrelated to the requested commits.
    let mut commits = Vec::with_capacity(info.commits.len());
    for commit_data in &info.commits {
        commits.push(
            oak_core::protocol::commit_data_to_core(commit_data)
                .map_err(|e| OakError::Server(format!("invalid commit object from server: {e}")))?,
        );
    }

    let mut trees = Vec::with_capacity(info.trees.len());
    let mut tree_by_hash = std::collections::HashMap::with_capacity(info.trees.len());
    for tree_data in &info.trees {
        let tree = oak_core::protocol::tree_data_to_core(tree_data)
            .map_err(|e| OakError::Server(format!("invalid tree object from server: {e}")))?;
        if tree_by_hash
            .insert(tree.hash.clone(), tree.clone())
            .is_some()
        {
            return Err(OakError::Server(format!(
                "commit info response returned duplicate tree {}",
                tree.hash
            )));
        }
        trees.push(tree);
    }

    let empty_tree_hash = oak_core::Tree::empty_hash();
    let mut reachable = std::collections::HashSet::with_capacity(trees.len());
    let mut pending: Vec<Hash> = commits
        .iter()
        .filter(|commit| commit.manifest_hash != empty_tree_hash)
        .map(|commit| commit.manifest_hash.clone())
        .collect();
    while let Some(hash) = pending.pop() {
        if !reachable.insert(hash.clone()) {
            continue;
        }
        let tree = tree_by_hash
            .get(&hash)
            .ok_or_else(|| OakError::IncompleteManifestData {
                left: "remote commit fetch".to_string(),
                right: "server response".to_string(),
                missing: hash.short().to_string(),
            })?;
        pending.extend(
            tree.entries
                .iter()
                .filter(|entry| {
                    entry.kind == oak_core::TreeEntryKind::Tree && entry.hash != empty_tree_hash
                })
                .map(|entry| entry.hash.clone()),
        );
    }
    if let Some(extra) = tree_by_hash
        .keys()
        .filter(|hash| !reachable.contains(*hash))
        .min_by(|left, right| left.as_str().cmp(right.as_str()))
    {
        return Err(OakError::Server(format!(
            "commit info response returned unreachable tree {extra}"
        )));
    }

    // One transaction covers all tree/commit writes. Although every object
    // above is already valid, a storage failure halfway through must not leave
    // a torn local cache that makes a later fetch incorrectly look complete.
    let bulk = super::BulkTxn::begin(repo)?;
    for tree in &trees {
        repo.store_tree(tree)?;
    }
    if commits
        .iter()
        .any(|commit| commit.manifest_hash == empty_tree_hash)
    {
        repo.store_manifest(&Manifest::empty())?;
    }
    for commit in &commits {
        if repo.get_manifest(&commit.manifest_hash)?.is_none()
            && commit.manifest_hash != empty_tree_hash
        {
            return Err(OakError::IncompleteManifestData {
                left: "remote commit fetch".to_string(),
                right: "server response".to_string(),
                missing: commit.manifest_hash.short().to_string(),
            });
        }
        repo.store_commit(commit)?;
    }
    bulk.commit()?;

    Ok(())
}

/// Ensure every blob in `blob_hashes` is present in the local repo, fetching
/// missing ones from the remote.
///
/// Returns Ok(()) even if the set was empty. Fails if the remote is
/// reachable but returns no info for a blob the caller asked about —
/// that means the server doesn't have it either, and the caller should
/// fall back to `oak pull` or surface the underlying gap.
pub async fn ensure_blobs_local(
    repo: &SqliteRepository,
    remote_url: &str,
    owner: &str,
    repo_name: &str,
    api_key: Option<&str>,
    blob_hashes: &[Hash],
) -> Result<()> {
    ensure_blobs_local_with_legacy_branch(
        repo,
        remote_url,
        owner,
        repo_name,
        api_key,
        blob_hashes,
        None,
    )
    .await
}

/// Push-only bridge for released servers that predate `/blobs/info`.
/// The fallback reads one exact branch through the old pull protocol under a
/// strict resource envelope, then uses the same chunk and plaintext hash
/// verification as the normal hydration path before the push may mutate.
pub async fn ensure_blobs_local_for_legacy_push(
    repo: &SqliteRepository,
    remote_url: &str,
    owner: &str,
    repo_name: &str,
    branch_name: &str,
    api_key: Option<&str>,
    blob_hashes: &[Hash],
) -> Result<()> {
    ensure_blobs_local_with_legacy_branch(
        repo,
        remote_url,
        owner,
        repo_name,
        api_key,
        blob_hashes,
        Some(branch_name),
    )
    .await
}

async fn ensure_blobs_local_with_legacy_branch(
    repo: &SqliteRepository,
    remote_url: &str,
    owner: &str,
    repo_name: &str,
    api_key: Option<&str>,
    blob_hashes: &[Hash],
    legacy_branch_name: Option<&str>,
) -> Result<()> {
    // Filter to blobs that aren't already present locally. The empty blob
    // never needs the network: its bytes are implied by its hash, so
    // reconstruct it here rather than asking a server that may not have it
    // (a metadata-only row with no chunks and no object storage backing is a
    // known post-migration state) and then failing the whole fetch.
    let mut missing: Vec<String> = Vec::new();
    for hash in blob_hashes {
        if !repo.has_blob(hash)? && !oak_core::ensure_empty_blob(repo, hash)? {
            missing.push(hash.as_str().to_string());
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    // Deduplicate — the same blob can appear multiple times in a manifest.
    missing.sort();
    missing.dedup();

    let client = shared_client();

    // Step 1: resolve each missing blob's chunk refs. We often already know
    // them locally — `oak mount` records every blob→chunk mapping up front,
    // and each fetch persists its own mapping (see `store_blob_chunks` below) —
    // so a cold read can skip the `/blobs/info` round-trip entirely and go
    // straight to downloading chunks. Only blobs whose refs we don't have
    // locally cost an info request.
    let mut info_blobs: Vec<BlobData> = Vec::new();
    let mut need_info: Vec<String> = Vec::new();
    for hash in &missing {
        match repo.get_blob_chunks(&Hash(hash.clone()))? {
            Some(chunks) if !chunks.is_empty() => {
                let size = chunks.iter().try_fold(0u64, |size, chunk| {
                    size.checked_add(u64::from(chunk.length)).ok_or_else(|| {
                        OakError::Server(format!("cached blob {hash} chunk sizes overflow"))
                    })
                })?;
                let chunk_refs = chunks
                    .into_iter()
                    .map(|c| ChunkRef {
                        hash: c.hash.as_str().to_string(),
                        offset: c.offset,
                        size: c.length,
                    })
                    .collect();
                info_blobs.push(BlobData {
                    hash: hash.clone(),
                    content: Vec::new(),
                    size,
                    chunks: chunk_refs,
                });
            }
            _ => need_info.push(hash.clone()),
        }
    }

    if !need_info.is_empty() {
        let info_url = format!("{remote_url}/api/{owner}/{repo_name}/blobs/info");
        let info_resp = with_auth(
            client
                .post(&info_url)
                .json(&BlobInfoRequest { hashes: &need_info }),
            api_key,
        )
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

        let legacy_full_branch = matches!(info_resp.status().as_u16(), 404 | 405);
        let mut info = if legacy_full_branch {
            let branch = legacy_branch_name.ok_or_else(|| {
                OakError::Server(format!(
                    "blob info request failed: HTTP {}; this legacy server requires a branch-scoped pull for safe hydration",
                    info_resp.status()
                ))
            })?;
            BlobInfoResponse {
                blobs: legacy_pull_blob_info(
                    &client, remote_url, owner, repo_name, branch, api_key,
                )
                .await?,
                restricted: Vec::new(),
            }
        } else if !info_resp.status().is_success() {
            return Err(OakError::Server(format!(
                "blob info request failed: {}",
                crate::http::error_text(info_resp).await
            )));
        } else {
            info_resp
                .json()
                .await
                .map_err(|e| OakError::Http(e.to_string()))?
        };

        // Path permissions: the server names requested blobs it withheld
        // because the user isn't granted on their path. Record them (so
        // status/clone can explain the gap) and fail with the actual cause
        // instead of the generic missing-blob error below. Best-effort on the
        // record — the error itself is what matters here.
        if !info.restricted.is_empty() {
            super::restricted::record_restricted_blobs(repo, &info.restricted).ok();
            let mut restricted = info.restricted.clone();
            restricted.sort();
            return Err(OakError::RestrictedContent(format!(
                "{} file(s) are under a restricted path — content withheld by the server; {}. \
                 First withheld blob: {}",
                restricted.len(),
                super::restricted::ACCESS_HINT,
                restricted[0],
            )));
        }

        if legacy_full_branch {
            // Released servers return the whole branch snapshot. Narrow that
            // response to this request before validating or persisting it: a
            // same-cardinality set of unrelated branch blobs is not proof that
            // the requested bytes exist.
            let requested: std::collections::HashSet<&str> =
                need_info.iter().map(String::as_str).collect();
            info.blobs
                .retain(|blob| requested.contains(blob.hash.as_str()));
        }
        let empty_hash = Blob::empty_hash();
        validate_hash_response_allowing(
            &need_info,
            info.blobs.iter().map(|blob| blob.hash.as_str()),
            &[empty_hash.as_str()],
            "blob info response",
        )?;

        info_blobs.extend(info.blobs);
    }

    let validated_mappings: Vec<(Hash, Vec<ChunkInfo>)> = info_blobs
        .iter()
        .map(|blob| {
            if blob.chunks.is_empty() && !blob.content.is_empty() {
                let hash = Hash::from_hex(&blob.hash).map_err(|error| {
                    OakError::Server(format!("invalid inline blob hash: {error}"))
                })?;
                if blob.size != blob.content.len() as u64 {
                    return Err(OakError::Server(format!(
                        "inline blob {hash} has fetched size {}, expected {}",
                        blob.content.len(),
                        blob.size
                    )));
                }
                return Ok((hash, Vec::new()));
            }
            super::pull::validate_blob_mapping_descriptor(
                &blob.hash,
                blob.size,
                blob.chunks
                    .iter()
                    .map(|chunk| (chunk.hash.as_str(), chunk.offset, u64::from(chunk.size))),
            )
        })
        .collect::<Result<_>>()?;

    // All descriptor validation precedes the transaction. Every subsequent
    // cache deletion, chunk write, blob write, and mapping replacement rolls
    // back together if download or reassembly fails.
    let bulk = super::BulkTxn::begin(repo)?;

    // Step 2: collect chunk hashes across all chunked blobs we still need.
    let mut needed_chunk_hashes: Vec<String> = Vec::new();
    for (_blob, (_blob_hash, mapping)) in info_blobs.iter().zip(&validated_mappings) {
        for chunk in mapping {
            if !super::pull::chunk_is_present_and_valid(repo, &chunk.hash)? {
                needed_chunk_hashes.push(chunk.hash.to_string());
            }
        }
    }
    needed_chunk_hashes.sort();
    needed_chunk_hashes.dedup();

    // Step 3: fetch any missing chunks concurrently.
    if !needed_chunk_hashes.is_empty() {
        let dl_url = format!("{remote_url}/api/{owner}/{repo_name}/chunks/download");
        let mut dl = ChunkDownloadResponse {
            chunks: Vec::new(),
            restricted: Vec::new(),
        };
        for hashes in needed_chunk_hashes.chunks(oak_core::protocol::CHUNK_BATCH_MAX_HASHES) {
            let requested_page = hashes.to_vec();
            let dl_request = with_auth(
                client.post(&dl_url).json(&ChunkDownloadRequest {
                    hashes,
                    chunk_batch_protocol: oak_core::protocol::CHUNK_BATCH_PROTOCOL,
                }),
                api_key,
            );
            let dl_resp = crate::http::send_idempotent_with_retry_until(
                dl_request,
                "chunk download metadata",
                tokio::time::Instant::now() + std::time::Duration::from_secs(60),
            )
            .await?;

            if !dl_resp.status().is_success() {
                return Err(OakError::Server(format!(
                    "chunk download request failed: {}",
                    crate::http::error_text(dl_resp).await
                )));
            }

            let page: ChunkDownloadResponse = dl_resp
                .json()
                .await
                .map_err(|e| OakError::Http(e.to_string()))?;
            validate_exact_hash_response(
                &requested_page,
                page.chunks
                    .iter()
                    .map(|chunk| chunk.hash.as_str())
                    .chain(page.restricted.iter().map(String::as_str)),
                "chunk download response",
            )?;
            dl.chunks.extend(page.chunks);
            dl.restricted.extend(page.restricted);
        }

        // Defense-in-depth mirror of the blobs/info check: chunk refs can be
        // cached locally (skipping blobs/info entirely), so the withheld
        // marker may first appear here.
        if !dl.restricted.is_empty() {
            return Err(OakError::RestrictedContent(format!(
                "{} chunk(s) belong to a file under a restricted path — content withheld by \
                 the server; {}",
                dl.restricted.len(),
                super::restricted::ACCESS_HINT,
            )));
        }

        let concurrency = max_concurrent_downloads();
        let semaphore = Arc::new(Semaphore::new(concurrency));
        // Dedicated HTTP/1.1 client so concurrent chunk GETs each get their own
        // connection rather than sharing one throttled HTTP/2 connection.
        let dl_client = download_client(concurrency);
        let mut tasks: JoinSet<Result<(String, Vec<u8>)>> = JoinSet::new();

        for chunk in dl.chunks {
            if let Some(content) = chunk.content {
                // Inline content — store immediately, no task needed. R2 chunks
                // may be zstd-compressed; decode (raw passes through).
                let chunk_hash = Hash(chunk.hash);
                let content = oak_core::chunk_decode(content, &chunk_hash);
                repo.store_chunk(&chunk_hash, &content)?;
            } else if let Some(url) = chunk.download_url {
                let client = dl_client.clone();
                let sem = semaphore.clone();
                let hash = chunk.hash;
                tasks.spawn(async move {
                    let _permit = sem
                        .acquire_owned()
                        .await
                        .map_err(|e| OakError::Http(e.to_string()))?;
                    let resp = client
                        .get(&url)
                        .send()
                        .await
                        .map_err(|e| OakError::Http(e.to_string()))?;
                    if !resp.status().is_success() {
                        return Err(OakError::Server(format!(
                            "chunk download failed ({}): {}",
                            resp.status(),
                            resp.text().await.unwrap_or_default()
                        )));
                    }
                    let bytes = resp
                        .bytes()
                        .await
                        .map_err(|e| OakError::Http(e.to_string()))?;
                    Ok((hash, bytes.to_vec()))
                });
            } else {
                return Err(OakError::Server(format!(
                    "server returned no content or URL for chunk {}",
                    chunk.hash
                )));
            }
        }

        while let Some(result) = tasks.join_next().await {
            let (chunk_hash, data) =
                result.map_err(|e| OakError::Http(format!("chunk task panicked: {e}")))??;
            let chunk_hash = Hash(chunk_hash);
            let data = oak_core::chunk_decode(data, &chunk_hash);
            repo.store_chunk(&chunk_hash, &data)?;
        }
    }

    // Step 4: reassemble each blob from its chunks. The server always ships
    // chunk refs (small/inline blobs land as a single self-chunk on push).
    // An empty `chunks` list means the server couldn't resolve the blob's
    // bytes — refuse rather than storing an empty blob. The sole exception is
    // the empty blob itself, which is *correctly* chunkless and whose content
    // its hash already pins down.
    let mut repaired = 0usize;
    for (blob_data, (blob_hash, mapping)) in info_blobs.iter().zip(&validated_mappings) {
        if mapping.is_empty() {
            if oak_core::ensure_empty_blob(repo, blob_hash)? {
                continue;
            }
            if !blob_data.content.is_empty() {
                let verified =
                    super::pull::verify_blob_content(blob_data.content.clone(), blob_hash)?;
                repo.store_blob(&Blob {
                    hash: blob_hash.clone(),
                    size: verified.content.len() as u64,
                    content: verified.content,
                })?;
                continue;
            }
            return Err(OakError::Server(format!(
                "Server returned blob {} with no chunk refs — its bytes are unreachable. \
                 The server may need to run migrate-blobs-to-r2.",
                blob_data.hash
            )));
        }
        let mut chunk_data: Vec<Vec<u8>> = Vec::with_capacity(mapping.len());
        for chunk in mapping {
            let data = repo.get_chunk(&chunk.hash)?.ok_or_else(|| {
                OakError::Server(format!(
                    "chunk {} missing after download (blob {})",
                    chunk.hash, blob_hash
                ))
            })?;
            super::pull::validate_fetched_chunk(blob_hash, chunk, &data)?;
            chunk_data.push(data);
        }
        let refs: Vec<&[u8]> = chunk_data.iter().map(|d| d.as_slice()).collect();
        let content = reassemble_chunks(&refs);
        // Gate on hash(content) == hash before storing; repairs the
        // chunks-of-compressed-bytes server skew. See `verify_blob_content`.
        let verified = super::pull::verify_blob_content(content, blob_hash)?;
        if verified.repaired {
            repaired += 1;
        }
        let size = verified.content.len() as u64;
        repo.store_blob(&Blob {
            hash: blob_hash.clone(),
            content: verified.content,
            size,
        })?;
        super::pull::store_verified_blob_chunks(
            repo,
            blob_hash,
            mapping.clone(),
            verified.repaired,
        )?;
    }
    super::pull::warn_repaired_blobs(repaired);
    bulk.commit()?;
    super::known_loss::clear_recovered_known_lost_blobs(
        repo,
        info_blobs.iter().map(|blob| blob.hash.clone()),
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::protocol::CommitData;
    use oak_core::{Commit, FileMode, ManifestEntry, Repository};
    use tempfile::TempDir;
    use wiremock::matchers::{method, path as urlpath};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A blob the server marks `restricted` (path permissions) must fail with
    /// the access-hint error — not the generic "remote is missing blob(s)"
    /// corruption message — and land in the recorded restricted set.
    #[tokio::test(flavor = "current_thread")]
    async fn ensure_blobs_local_surfaces_restricted_marker() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let server = MockServer::start().await;

        let withheld = Hash("ab".repeat(32));
        Mock::given(method("POST"))
            .and(urlpath("/api/oak/oak/blobs/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "blobs": [],
                "restricted": [withheld.as_str()],
            })))
            .mount(&server)
            .await;

        let err = ensure_blobs_local(
            &repo,
            &server.uri(),
            "oak",
            "oak",
            None,
            std::slice::from_ref(&withheld),
        )
        .await
        .expect_err("permission-withheld blob must error");

        assert!(
            matches!(err, OakError::RestrictedContent(_)),
            "expected RestrictedContent, got {err:?}"
        );
        assert!(
            err.to_string().contains("ask an org admin"),
            "error must carry the access hint: {err}"
        );
        assert!(
            super::super::restricted::load_restricted_blobs(&repo).contains(withheld.as_str()),
            "withheld hash must be recorded for status/clone messaging"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ensure_commits_local_refuses_commit_when_manifest_tree_is_missing() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let server = MockServer::start().await;

        let missing_manifest = Hash("e1".repeat(32));
        let commit = Commit::with_timestamp(
            "dep".to_string(),
            None,
            None,
            missing_manifest.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
            chrono::Utc::now(),
        )
        .unwrap();
        let wire = CommitData {
            hash: commit.hash.to_string(),
            branch_name: commit.branch_name.clone(),
            parent_hash: None,
            merge_parent_hash: None,
            manifest_hash: missing_manifest.to_string(),
            author: commit.author.clone(),
            message: None,
            timestamp: commit.timestamp.to_rfc3339(),
            files: Vec::new(),
        };

        Mock::given(method("POST"))
            .and(urlpath("/api/oak/oak/commits/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commits": [wire],
                "trees": []
            })))
            .mount(&server)
            .await;

        let err = ensure_commits_local(
            &repo,
            &server.uri(),
            "oak",
            "oak",
            None,
            std::slice::from_ref(&commit.hash),
        )
        .await
        .expect_err("commit without its manifest tree must be rejected");

        assert!(
            matches!(
                err,
                OakError::IncompleteManifestData {
                    ref missing, ..
                } if missing.contains(missing_manifest.short())
            ),
            "expected typed missing-manifest error, got {err:?}"
        );
        assert!(
            repo.get_commit(&commit.hash).unwrap().is_none(),
            "commit row must not be stored without its manifest"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ensure_commits_local_refetches_existing_commit_with_missing_manifest() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let server = MockServer::start().await;

        let manifest = Manifest::new(vec![ManifestEntry {
            path: "README.md".to_string(),
            blob_hash: Hash("ab".repeat(32)),
            mode: FileMode::Regular,
        }]);
        let commit = Commit::with_timestamp(
            "dep".to_string(),
            None,
            None,
            manifest.hash.clone(),
            "tester".to_string(),
            None,
            Vec::new(),
            chrono::Utc::now(),
        )
        .unwrap();
        repo.store_commit(&commit).unwrap();
        assert!(
            repo.get_manifest(&manifest.hash).unwrap().is_none(),
            "fixture starts with a torn commit cache"
        );
        let wire = CommitData {
            hash: commit.hash.to_string(),
            branch_name: commit.branch_name.clone(),
            parent_hash: None,
            merge_parent_hash: None,
            manifest_hash: manifest.hash.to_string(),
            author: commit.author.clone(),
            message: None,
            timestamp: commit.timestamp.to_rfc3339(),
            files: Vec::new(),
        };
        let built = oak_core::build_tree(&manifest.entries).unwrap();
        let trees: Vec<_> = built
            .trees
            .iter()
            .map(oak_core::protocol::tree_to_wire)
            .collect();

        Mock::given(method("POST"))
            .and(urlpath("/api/oak/oak/commits/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "commits": [wire],
                "trees": trees
            })))
            .expect(1)
            .mount(&server)
            .await;

        ensure_commits_local(
            &repo,
            &server.uri(),
            "oak",
            "oak",
            None,
            std::slice::from_ref(&commit.hash),
        )
        .await
        .expect("existing commit with missing manifest should be repaired");

        assert!(repo.get_commit(&commit.hash).unwrap().is_some());
        assert!(
            repo.get_manifest(&manifest.hash).unwrap().is_some(),
            "manifest tree should be hydrated from the remote response"
        );
    }
}
