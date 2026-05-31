//! Lazy blob fetch for view-dependency resolution.
//!
//! When a user activates a view with pinned dependencies, they may not have
//! every blob needed by the dep manifests — `oak pull` only fetches blobs
//! matching the active view's filter, so blobs outside that scope are absent.
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

use oak_core::{
    reassemble_chunks, Blob, ChangeType, ChunkInfo, Commit, FileChange, FileMode, Hash, OakError,
    Result,
};
use oak_core::{Repository, SqliteRepository};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

const MAX_CONCURRENT_CHUNK_DOWNLOADS: usize = 8;

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
    size: u64,
    #[serde(default)]
    chunks: Vec<ChunkRef>,
}

#[derive(Deserialize)]
struct BlobInfoResponse {
    blobs: Vec<BlobData>,
}

#[derive(Serialize)]
struct ChunkDownloadRequest<'a> {
    hashes: &'a [String],
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
struct FileChangeData {
    path: String,
    change_type: String,
    #[serde(default)]
    old_blob_hash: Option<String>,
    #[serde(default)]
    new_blob_hash: Option<String>,
    #[serde(default)]
    old_path: Option<String>,
}

#[derive(Deserialize)]
struct CommitData {
    hash: String,
    branch_name: String,
    #[serde(default)]
    parent_hash: Option<String>,
    #[serde(default)]
    merge_parent_hash: Option<String>,
    manifest_hash: String,
    author: String,
    #[serde(default)]
    message: Option<String>,
    timestamp: String,
    #[serde(default)]
    files: Vec<FileChangeData>,
}

#[derive(Deserialize)]
struct TreeEntryData {
    name: String,
    kind: String,
    hash: String,
    mode: String,
}

#[derive(Deserialize)]
struct TreeData {
    hash: String,
    entries: Vec<TreeEntryData>,
}

#[derive(Deserialize)]
struct CommitInfoResponse {
    commits: Vec<CommitData>,
    #[serde(default)]
    trees: Vec<TreeData>,
}

/// Ensure each commit in `commit_hashes` is present locally, fetching missing
/// commits (and their manifests) from the remote. Does not touch blobs.
///
/// Used during view-dependency resolution so a pin can target any commit the
/// server knows about, not just commits the user has pulled.
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
        if repo.get_commit(hash)?.is_none() {
            missing.push(hash.as_str().to_string());
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort();
    missing.dedup();

    let client = reqwest::Client::new();
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
        let body = resp.text().await.unwrap_or_default();
        return Err(OakError::Server(format!(
            "commit info request failed: {body}"
        )));
    }

    let info: CommitInfoResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if info.commits.len() < missing.len() {
        let got: std::collections::HashSet<&str> =
            info.commits.iter().map(|c| c.hash.as_str()).collect();
        let missing_on_server: Vec<&str> = missing
            .iter()
            .filter(|h| !got.contains(h.as_str()))
            .map(|s| s.as_str())
            .collect();
        return Err(OakError::Server(format!(
            "remote is missing {} commit(s) referenced by a pinned dependency; ensure the dep commit has been pushed. First missing: {}",
            missing_on_server.len(),
            missing_on_server.first().unwrap_or(&"")
        )));
    }

    // Store tree objects first — commits reference the root tree by hash.
    for tree_data in info.trees {
        let mut entries = Vec::with_capacity(tree_data.entries.len());
        for e in tree_data.entries {
            let kind = match e.kind.as_str() {
                "tree" => oak_core::TreeEntryKind::Tree,
                "blob" => oak_core::TreeEntryKind::Blob,
                other => {
                    return Err(OakError::Server(format!(
                        "invalid tree entry kind from server: {other}"
                    )))
                }
            };
            let mode = match e.mode.as_str() {
                "executable" => FileMode::Executable,
                "symlink" => FileMode::Symlink,
                _ => FileMode::Regular,
            };
            entries.push(oak_core::TreeEntry {
                name: e.name,
                kind,
                hash: Hash(e.hash),
                mode,
            });
        }
        let tree = oak_core::Tree {
            hash: Hash(tree_data.hash),
            entries,
        };
        repo.store_tree(&tree)?;
    }

    for commit_data in info.commits {
        let timestamp = chrono::DateTime::parse_from_rfc3339(&commit_data.timestamp)
            .map_err(|e| OakError::Config(format!("invalid commit timestamp: {e}")))?
            .with_timezone(&chrono::Utc);
        let files: Vec<FileChange> = commit_data
            .files
            .into_iter()
            .map(|f| FileChange {
                path: f.path,
                change_type: match f.change_type.as_str() {
                    "modified" => ChangeType::Modified,
                    "deleted" => ChangeType::Deleted,
                    "renamed" => ChangeType::Renamed,
                    _ => ChangeType::Added,
                },
                old_blob_hash: f.old_blob_hash.map(Hash),
                new_blob_hash: f.new_blob_hash.map(Hash),
                old_path: f.old_path,
                old_mode: None,
                new_mode: None,
            })
            .collect();
        let commit = Commit {
            hash: Hash(commit_data.hash),
            branch_name: commit_data.branch_name,
            parent_hash: commit_data.parent_hash.map(Hash),
            merge_parent_hash: commit_data.merge_parent_hash.map(Hash),
            manifest_hash: Hash(commit_data.manifest_hash),
            author: commit_data.author,
            message: commit_data.message,
            timestamp,
            files,
        };
        repo.store_commit(&commit)?;
    }

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
    // Filter to blobs that aren't already present locally.
    let mut missing: Vec<String> = Vec::new();
    for hash in blob_hashes {
        if !repo.has_blob(hash)? {
            missing.push(hash.as_str().to_string());
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    // Deduplicate — the same blob can appear multiple times in a manifest.
    missing.sort();
    missing.dedup();

    let client = reqwest::Client::new();

    // Step 1: fetch blob metadata for the missing set.
    let info_url = format!("{remote_url}/api/{owner}/{repo_name}/blobs/info");
    let info_resp = with_auth(
        client
            .post(&info_url)
            .json(&BlobInfoRequest { hashes: &missing }),
        api_key,
    )
    .send()
    .await
    .map_err(|e| OakError::Http(e.to_string()))?;

    if !info_resp.status().is_success() {
        let body = info_resp.text().await.unwrap_or_default();
        return Err(OakError::Server(format!(
            "blob info request failed: {body}"
        )));
    }

    let info: BlobInfoResponse = info_resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    // Detect blobs the server simply doesn't know about — this usually
    // means the referencing commit hasn't been pushed, so the dep is
    // unresolvable until the owner pushes the commit.
    if info.blobs.len() < missing.len() {
        let got: std::collections::HashSet<&str> =
            info.blobs.iter().map(|b| b.hash.as_str()).collect();
        let missing_on_server: Vec<&str> = missing
            .iter()
            .filter(|h| !got.contains(h.as_str()))
            .map(|s| s.as_str())
            .collect();
        return Err(OakError::Server(format!(
            "remote is missing {} blob(s) referenced by a pinned dependency; ensure the dep commit has been pushed. First missing: {}",
            missing_on_server.len(),
            missing_on_server.first().unwrap_or(&"")
        )));
    }

    // Step 2: collect chunk hashes across all chunked blobs we still need.
    let mut needed_chunk_hashes: Vec<String> = Vec::new();
    for blob in &info.blobs {
        for chunk in &blob.chunks {
            if !repo.has_chunk(&Hash(chunk.hash.clone()))? {
                needed_chunk_hashes.push(chunk.hash.clone());
            }
        }
    }
    needed_chunk_hashes.sort();
    needed_chunk_hashes.dedup();

    // Step 3: fetch any missing chunks concurrently.
    if !needed_chunk_hashes.is_empty() {
        let dl_url = format!("{remote_url}/api/{owner}/{repo_name}/chunks/download");
        let dl_resp = with_auth(
            client.post(&dl_url).json(&ChunkDownloadRequest {
                hashes: &needed_chunk_hashes,
            }),
            api_key,
        )
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

        if !dl_resp.status().is_success() {
            let body = dl_resp.text().await.unwrap_or_default();
            return Err(OakError::Server(format!(
                "chunk download request failed: {body}"
            )));
        }

        let dl: ChunkDownloadResponse = dl_resp
            .json()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;

        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CHUNK_DOWNLOADS));
        let mut tasks: JoinSet<Result<(String, Vec<u8>)>> = JoinSet::new();

        for chunk in dl.chunks {
            if let Some(content) = chunk.content {
                // Inline content — store immediately, no task needed.
                repo.store_chunk(&Hash(chunk.hash), &content)?;
            } else if let Some(url) = chunk.download_url {
                let client = client.clone();
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
            repo.store_chunk(&Hash(chunk_hash), &data)?;
        }
    }

    // Step 4: reassemble each blob from its chunks. The server always ships
    // chunk refs (small/inline blobs land as a single self-chunk on push).
    // An empty `chunks` list means the server couldn't resolve the blob's
    // bytes — refuse rather than storing an empty blob.
    for blob_data in &info.blobs {
        if blob_data.chunks.is_empty() {
            return Err(OakError::Server(format!(
                "Server returned blob {} with no chunk refs — its bytes are unreachable. \
                 The server may need to run migrate-blobs-to-r2.",
                blob_data.hash
            )));
        }
        let mut chunk_infos: Vec<ChunkInfo> = Vec::with_capacity(blob_data.chunks.len());
        let mut chunk_data: Vec<Vec<u8>> = Vec::with_capacity(blob_data.chunks.len());
        for chunk in &blob_data.chunks {
            let ch_hash = Hash(chunk.hash.clone());
            let data = repo.get_chunk(&ch_hash)?.ok_or_else(|| {
                OakError::Server(format!(
                    "chunk {} missing after download (blob {})",
                    chunk.hash, blob_data.hash
                ))
            })?;
            chunk_infos.push(ChunkInfo {
                hash: ch_hash,
                offset: chunk.offset,
                length: chunk.size,
            });
            chunk_data.push(data);
        }
        let refs: Vec<&[u8]> = chunk_data.iter().map(|d| d.as_slice()).collect();
        let content = reassemble_chunks(&refs);
        let blob_hash = Hash(blob_data.hash.clone());
        repo.store_blob(&Blob {
            hash: blob_hash.clone(),
            content,
            size: blob_data.size,
        })?;
        repo.store_blob_chunks(&blob_hash, &chunk_infos)?;
    }

    Ok(())
}
