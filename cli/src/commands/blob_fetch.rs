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
    /// Wire field; the stored size is recomputed from the verified content
    /// (`verify_blob_content`), so nothing reads it anymore.
    #[allow(dead_code)]
    size: u64,
    #[serde(default)]
    chunks: Vec<ChunkRef>,
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
    for tree_data in &info.trees {
        let tree = oak_core::protocol::tree_data_to_core(tree_data)
            .map_err(|e| OakError::Server(format!("invalid tree object from server: {e}")))?;
        repo.store_tree(&tree)?;
    }

    let mut commits = Vec::new();
    for commit_data in &info.commits {
        let commit = oak_core::protocol::commit_data_to_core(commit_data)
            .map_err(|e| OakError::Server(format!("invalid commit object from server: {e}")))?;
        if repo.get_manifest(&commit.manifest_hash)?.is_none() {
            if commit.manifest_hash == Manifest::empty().hash {
                repo.store_manifest(&Manifest::empty())?;
            } else {
                return Err(OakError::IncompleteManifestData {
                    left: "remote commit fetch".to_string(),
                    right: "server response".to_string(),
                    missing: commit.manifest_hash.short().to_string(),
                });
            }
        }
        commits.push(commit);
    }

    for commit in &commits {
        repo.store_commit(commit)?;
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
                let size = chunks.iter().map(|c| c.length as u64).sum();
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

        if !info_resp.status().is_success() {
            return Err(OakError::Server(format!(
                "blob info request failed: {}",
                crate::http::error_text(info_resp).await
            )));
        }

        let info: BlobInfoResponse = info_resp
            .json()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;

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

        // Detect blobs the server simply doesn't know about — this usually
        // means the referencing commit hasn't been pushed, so the dep is
        // unresolvable until the owner pushes the commit.
        if info.blobs.len() < need_info.len() {
            let got: std::collections::HashSet<&str> =
                info.blobs.iter().map(|b| b.hash.as_str()).collect();
            let missing_on_server: Vec<&str> = need_info
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

        // Persist the freshly-learned chunk refs so future cold reads of these
        // blobs skip the info request above.
        for blob in &info.blobs {
            if !blob.chunks.is_empty() {
                let infos: Vec<ChunkInfo> = blob
                    .chunks
                    .iter()
                    .map(|c| ChunkInfo {
                        hash: Hash(c.hash.clone()),
                        offset: c.offset,
                        length: c.size,
                    })
                    .collect();
                repo.store_blob_chunks(&Hash(blob.hash.clone()), &infos)?;
            }
        }
        info_blobs.extend(info.blobs);
    }

    // Step 2: collect chunk hashes across all chunked blobs we still need.
    let mut needed_chunk_hashes: Vec<String> = Vec::new();
    for blob in &info_blobs {
        for chunk in &blob.chunks {
            if !super::pull::chunk_is_present_and_valid(repo, &Hash(chunk.hash.clone()))? {
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
            return Err(OakError::Server(format!(
                "chunk download request failed: {}",
                crate::http::error_text(dl_resp).await
            )));
        }

        let dl: ChunkDownloadResponse = dl_resp
            .json()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;

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
    // bytes — refuse rather than storing an empty blob.
    let mut repaired = 0usize;
    for blob_data in &info_blobs {
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
        // Gate on hash(content) == hash before storing; repairs the
        // chunks-of-compressed-bytes server skew. See `verify_blob_content`.
        let verified = super::pull::verify_blob_content(content, &blob_hash)?;
        if verified.repaired {
            repaired += 1;
        }
        let size = verified.content.len() as u64;
        repo.store_blob(&Blob {
            hash: blob_hash.clone(),
            content: verified.content,
            size,
        })?;
        super::pull::store_verified_blob_chunks(repo, &blob_hash, chunk_infos, verified.repaired)?;
    }
    super::pull::warn_repaired_blobs(repaired);

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
