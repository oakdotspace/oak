use std::path::Path;
use std::sync::Arc;

use dialoguer::{Confirm, Input, Select};
use oak_core::{chunk_content, Hash, MetadataKey, OakError, Result, LARGE_FILE_THRESHOLD};
use oak_core::{Repository, SqliteRepository};
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::output;

/// Maximum number of concurrent chunk uploads.
const MAX_CONCURRENT_TRANSFERS: usize = 8;

// Wire protocol types live in `oak_core::protocol` (the single source of truth
// shared by the CLI, the hosted server, and `oak serve`). They're aliased back
// to the names this module has always used so the call sites below are
// unchanged. `BranchPushData`'s wire shape is identical to the old local
// `BranchData`, and `ChunkCheckServerResponse` to the old `ChunkCheckResponse`.
use oak_core::protocol::{
    BlobCheckResponse, BlobData, BranchPushData as BranchData,
    ChunkCheckServerResponse as ChunkCheckResponse, ChunkRefData as ChunkRef, CommitData,
    FileChangeData, PushRequest, PushResponse, TreeData, TreeEntryData,
};

/// Minimal client-side view of `GET /api/{owner}/{name}` — the push flow only
/// needs the repo head, so it deserializes leniently rather than pulling in the
/// full `protocol::RepoResponse`.
#[derive(Deserialize)]
struct RepoResponse {
    head: Option<String>,
}

/// Subset of the per-branch GET response we need to compute `remote_head`
/// for a branch-scoped push.
#[derive(Deserialize)]
struct BranchHeadResponse {
    head: Option<String>,
}

/// Helper to add auth header to a request builder
fn with_auth(builder: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    if let Some(key) = api_key {
        builder.header("authorization", format!("Bearer {key}"))
    } else {
        builder
    }
}

/// Push commits to remote server
pub async fn run(
    path: &Path,
    remote_url: &str,
    force: bool,
    repo_spec: Option<&str>,
) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let db_path = ctx.db_path()?;
    let work_tree = ctx.work_tree.clone();
    let repo = SqliteRepository::open(&db_path)?;

    // Save remote URL for future use
    repo.set_metadata(MetadataKey::RemoteUrl, remote_url)?;

    // Get current branch name
    let branch_name = repo.get_current_branch_name().ok().flatten();

    // Get API key from env var, per-repo metadata, or global credentials
    let api_key = std::env::var("OAK_API_KEY")
        .ok()
        .or_else(|| repo.get_metadata(MetadataKey::ApiKey).ok().flatten())
        .or_else(|| super::credentials::get_token_for_server(remote_url));

    let owner_meta = repo.get_metadata(MetadataKey::RepoOwner)?;
    let (owner, repo_name) = if let Some(owner) = owner_meta {
        let name = repo.get_metadata(MetadataKey::RepoName)?.ok_or_else(|| {
            OakError::Server(
                "Repository metadata missing repo name; re-clone with `oak clone <owner>/<repo>`"
                    .to_string(),
            )
        })?;
        (owner, name)
    } else if let Some(spec) = repo_spec {
        // `--repo owner/name` (or OAK_REPO): link non-interactively. The repo
        // is auto-created on the server when the push 404s; OWNER must be an
        // existing org slug.
        link_remote_identity(&repo, remote_url, spec)?
    } else {
        setup_remote_identity(&repo, remote_url, api_key.as_deref()).await?
    };
    push_async(
        &repo,
        &work_tree,
        remote_url,
        &owner,
        &repo_name,
        branch_name.as_deref(),
        force,
        api_key.as_deref(),
    )
    .await
}

/// Async push implementation
#[allow(clippy::too_many_arguments)]
pub async fn push_async(
    repo: &SqliteRepository,
    work_tree: &Path,
    remote: &str,
    owner: &str,
    repo_name: &str,
    branch_name: Option<&str>,
    force: bool,
    api_key: Option<&str>,
) -> Result<()> {
    push_async_with_endpoint(
        repo,
        work_tree,
        remote,
        &format!("{owner}/{repo_name}"),
        branch_name,
        force,
        api_key,
        None,
    )
    .await
}

/// Send a branch-metadata-only push — no commits, no blobs, no trees,
/// just the `BranchPushData` payload so the server upserts the branch
/// row with the current description / parent / status. Used by
/// `oak desc` inside a mount, where we want the new description to land
/// on the server immediately rather than waiting for the next push with
/// real commits. The server's push handler is already shaped to accept
/// this (`PushRequest.commits = []` is fine; see the push handler in
/// `crates/oak-server/src/api/repos.rs`, which calls `store_branch`
/// regardless of whether any commits accompany the payload).
///
/// Returns `Ok(())` when the server accepts the update. The caller is
/// responsible for translating network / auth failures into a friendly
/// "saved locally, retry with `oak push`" message — we want the local
/// description change to be durable even if this round-trip fails.
pub async fn push_branch_metadata(
    repo: &SqliteRepository,
    remote: &str,
    owner: &str,
    repo_name: &str,
    branch_name: &str,
    api_key: Option<&str>,
) -> Result<()> {
    let branch = repo
        .get_branch(branch_name)?
        .ok_or_else(|| OakError::BranchNotFound(branch_name.to_string()))?;

    let push_req = PushRequest {
        expected_head: None,
        expected_branch_head: None,
        force: false,
        branch: Some(BranchData {
            name: branch.name,
            description: branch.description,
            parent_branch: branch.parent_branch,
            status: branch.status.to_string(),
            created_at: branch.created_at.to_rfc3339(),
        }),
        commits: Vec::new(),
        blobs: Vec::new(),
        trees: Vec::new(),
    };

    let client = reqwest::Client::new();
    let resp = with_auth(
        client
            .post(format!("{remote}/api/{owner}/{repo_name}/push"))
            .header("x-oak-user", super::commit::get_author())
            .json(&push_req),
        api_key,
    )
    .send()
    .await
    .map_err(|e| OakError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        let err_text = resp.text().await.unwrap_or_default();
        return Err(OakError::Server(err_text));
    }
    Ok(())
}

/// Batch size for the bootstrap loop that pushes imported git history.
/// Each batch is one round-trip and one round of in-memory blob/tree
/// collection — so the value trades request count vs. peak memory. 500
/// commits keeps the per-batch JSON body well under the 100 MB Cloudflare
/// request limit and bounds the blob working set for typical repos.
const BOOTSTRAP_BATCH_SIZE: usize = 500;

/// Async push implementation that works with both repo and space endpoints.
/// `endpoint_path` is e.g. "repos/my-repo".
///
/// `max_commits` caps the number of commits pushed in this single request
/// (oldest-first, per `get_commits_since` ordering). Used by the bootstrap
/// loop to batch a 80k-commit git import into many small fast-forward
/// pushes; `None` means "send everything pending" (the normal case).
#[allow(clippy::too_many_arguments)]
async fn push_async_with_endpoint(
    repo: &SqliteRepository,
    work_tree: &Path,
    remote: &str,
    endpoint_path: &str,
    branch_name: Option<&str>,
    force: bool,
    api_key: Option<&str>,
    max_commits: Option<usize>,
) -> Result<()> {
    let client = reqwest::Client::new();

    // Get remote head
    output::vlog(&format!(
        "push: GET {remote}/api/{endpoint_path} (fetch remote head)"
    ));
    let t0 = std::time::Instant::now();
    let resp = with_auth(client.get(format!("{remote}/api/{endpoint_path}")), api_key)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    output::vlog(&format!(
        "push: GET head returned {} in {:.3}s",
        resp.status(),
        t0.elapsed().as_secs_f64()
    ));

    // Repo-wide head (server's `main` head). For branch-scoped pushes this
    // is the *wrong* "since" point — `default` and `main` are independent
    // chains, so using the repo head would treat every commit on the
    // pushed branch as new and re-collect tree objects for commits the
    // server already has. We only use the repo GET here to confirm the
    // repo exists (and to auto-create it on 404); the actual
    // `remote_head` for a branch push is fetched from the per-branch
    // endpoint below.
    // Tracks whether we auto-created the repo on this push. Used after
    // the push succeeds to prompt for a merge — a fresh repo with only a
    // feature-branch push looks empty in the web UI until `main` has a
    // commit, which reads like the push silently failed.
    let mut created_repo = false;
    let repo_head: Option<Hash> = if resp.status().is_success() {
        let repo_info: RepoResponse = resp
            .json()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        repo_info.head.map(Hash)
    } else if resp.status().as_u16() == 404 {
        // Auto-create the repo on the server. `endpoint_path` is `owner/name`.
        if let Some((owner, repo_name)) = endpoint_path.split_once('/') {
            output::info(&format!(
                "Creating repository '{owner}/{repo_name}' on server..."
            ));

            let create_resp = with_auth(
                client
                    .post(format!("{remote}/api/repos"))
                    .json(&serde_json::json!({
                        "name": repo_name,
                        "description": null,
                        "organization_slug": owner,
                    })),
                api_key,
            )
            .send()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;

            if !create_resp.status().is_success() {
                let err_text = create_resp.text().await.unwrap_or_default();
                return Err(OakError::Server(err_text));
            }
            created_repo = true;
        }

        None
    } else {
        let err_text = resp.text().await.unwrap_or_default();
        return Err(OakError::Server(err_text));
    };

    // Bootstrap pass: when the server's repo is brand-new (empty `main`)
    // and the user is on a personal branch parented onto `main` whose
    // local main has imported commits, push those imported commits first
    // with `branch_name = "main"` on the wire. This is what makes
    // `oak clone <git-url>` + `oak push` and `oak init` (with git import)
    // + `oak push` actually transfer the imported history — without it
    // push would walk only the personal branch's own commits (zero right
    // after import) and report "already up to date" while abandoning the
    // imported history on the client.
    //
    // The server's bootstrap exception (`pushes_to_main` in
    // `oak-server/src/api/repos.rs`) requires `req.branch.name == "main"`
    // AND at least one commit with `branch_name == "main"`. Both hold
    // for the recursive call because `convert_history` writes imported
    // commits with `branch_name = "main"` and the recursive call sets
    // branch_data from the local `main` row.
    let mut bootstrapped_main = false;
    if repo_head.is_none() {
        if let Some(name) = branch_name {
            if name != "main" {
                let on_main_child = repo
                    .get_branch(name)?
                    .and_then(|b| b.parent_branch)
                    .is_some_and(|p| p == "main");
                let local_main_has_commits = repo.get_branch_head("main")?.is_some();
                if on_main_child && local_main_has_commits {
                    let local_main_head = repo.get_branch_head("main")?;
                    let total_main_commits = repo.get_commits_since("main", None)?.len();
                    output::info(&format!(
                        "Bootstrapping `main` from imported history ({total_main_commits} commits, batched in groups of {BOOTSTRAP_BATCH_SIZE})..."
                    ));

                    let pb = indicatif::ProgressBar::new(total_main_commits as u64);
                    pb.set_style(
                        indicatif::ProgressStyle::default_bar()
                            .template(
                                "  Bootstrap [{bar:30.magenta/dim}] {pos}/{len} commits ({elapsed_precise})",
                            )
                            .unwrap()
                            .progress_chars("━╸─"),
                    );

                    // Each iteration pushes up to BOOTSTRAP_BATCH_SIZE commits
                    // and advances the server's main head by that much. We're
                    // done when the server has caught up to local main.
                    loop {
                        let server_main = fetch_remote_branch_head(
                            &client,
                            remote,
                            endpoint_path,
                            "main",
                            api_key,
                        )
                        .await?;
                        if server_main == local_main_head {
                            break;
                        }

                        Box::pin(push_async_with_endpoint(
                            repo,
                            work_tree,
                            remote,
                            endpoint_path,
                            Some("main"),
                            force,
                            api_key,
                            Some(BOOTSTRAP_BATCH_SIZE),
                        ))
                        .await?;

                        // Recompute progress from server state — keeps the bar
                        // accurate even if the recursive push pushed fewer
                        // commits than requested (e.g. last partial batch).
                        let new_server = fetch_remote_branch_head(
                            &client,
                            remote,
                            endpoint_path,
                            "main",
                            api_key,
                        )
                        .await?;
                        let remaining = repo.get_commits_since("main", new_server.as_ref())?.len();
                        pb.set_position((total_main_commits - remaining) as u64);
                    }
                    pb.finish_and_clear();
                    output::success(&format!(
                        "Bootstrapped `main` with {total_main_commits} commits"
                    ));
                    bootstrapped_main = true;
                }
            }
        }
    }

    // For a branch-scoped push, resolve `remote_head` against the per-branch
    // endpoint. 404 means the server doesn't know this branch yet (e.g.
    // first push of a personal branch) — treat that as `None` so all
    // commits on the local branch are pushed. For a repo-scoped push
    // (no branch name) fall back to the repo head.
    let remote_head: Option<Hash> = if let Some(name) = branch_name {
        output::vlog(&format!(
            "push: GET {remote}/api/{endpoint_path}/branches/{name} (fetch branch head)"
        ));
        let resp = with_auth(
            client.get(format!("{remote}/api/{endpoint_path}/branches/{name}")),
            api_key,
        )
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

        if resp.status().is_success() {
            let body: BranchHeadResponse = resp
                .json()
                .await
                .map_err(|e| OakError::Http(e.to_string()))?;
            body.head.map(Hash)
        } else if resp.status().as_u16() == 404 {
            None
        } else {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(OakError::Server(err_text));
        }
    } else {
        repo_head
    };

    // Check for divergent history
    if !force {
        if let Some(ref rh) = remote_head {
            if !repo.has_commit(rh)? {
                let local_head = if let Some(name) = branch_name {
                    repo.get_branch_head(name)?
                } else {
                    repo.get_head()?
                };
                if local_head.is_some() {
                    output::error("Conflict detected: remote has commits not in local history.");
                    output::info(
                        "Use 'oak pull --force' to discard local commits and sync with remote.",
                    );
                    return Err(OakError::ConflictDetected);
                }
            }
        }
    } else {
        output::warning("Force push: overwriting remote history");
    }

    // Get commits to push
    let mut commits = if let Some(name) = branch_name {
        repo.get_commits_since(name, remote_head.as_ref())?
    } else {
        // Fallback: get all commits
        repo.get_all_commits()?
    };

    // Cap to `max_commits` if requested. `get_commits_since` returns commits
    // oldest-first, so taking the prefix is always a valid fast-forward chain.
    if let Some(limit) = max_commits {
        if commits.len() > limit {
            commits.truncate(limit);
        }
    }

    if commits.is_empty() {
        if !bootstrapped_main {
            output::info("Already up to date, nothing to push");
        }
        return Ok(());
    }

    output::info(&format!("Pushing {} commit(s)...", commits.len()));
    output::vlog(&format!(
        "push: collecting blobs/manifests for {} commit(s)",
        commits.len()
    ));
    let t_collect = std::time::Instant::now();

    // Collect blobs and tree objects
    let mut blobs: Vec<BlobData> = Vec::new();
    let mut trees: Vec<TreeData> = Vec::new();
    let mut seen_blobs = std::collections::HashSet::new();
    let mut seen_trees: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Track all chunk refs for large blobs so we can upload them
    let mut all_chunk_data: Vec<(ChunkRef, Vec<u8>)> = Vec::new();

    // Progress bar for the collection phase — silent on tiny pushes (a few
    // commits finish in milliseconds and the bar would flash and disappear)
    // but essential during bootstrap batches, where walking trees + reading
    // blob bytes for hundreds of commits can take a few seconds with no
    // other visible output.
    let collect_pb = if commits.len() >= 50 {
        let pb = indicatif::ProgressBar::new(commits.len() as u64);
        pb.set_style(
            indicatif::ProgressStyle::default_bar()
                .template(
                    "  Collecting [{bar:30.cyan/dim}] {pos}/{len} commits ({elapsed_precise})",
                )
                .unwrap()
                .progress_chars("━╸─"),
        );
        Some(pb)
    } else {
        None
    };

    for commit in &commits {
        if let Some(pb) = &collect_pb {
            pb.inc(1);
        }
        if seen_trees.contains(commit.manifest_hash.as_str()) {
            continue;
        }

        // Collect every tree object reachable from this commit, then walk
        // the flat manifest for blobs.
        let mut fetch = |h: &oak_core::Hash| -> oak_core::Result<oak_core::Tree> {
            repo.get_tree(h)?
                .ok_or_else(|| oak_core::OakError::ManifestNotFound(h.to_string()))
        };
        let commit_trees = oak_core::collect_tree_objects(&commit.manifest_hash, &mut fetch)?;
        for tree in &commit_trees {
            if seen_trees.insert(tree.hash.0.clone()) {
                trees.push(TreeData {
                    hash: tree.hash.to_string(),
                    entries: tree
                        .entries
                        .iter()
                        .map(|e| TreeEntryData {
                            name: e.name.clone(),
                            kind: e.kind.as_str().to_string(),
                            hash: e.hash.to_string(),
                            mode: match (e.kind, e.mode) {
                                (oak_core::TreeEntryKind::Tree, _) => "tree".to_string(),
                                (oak_core::TreeEntryKind::Blob, oak_core::FileMode::Regular) => {
                                    "regular".to_string()
                                }
                                (oak_core::TreeEntryKind::Blob, oak_core::FileMode::Executable) => {
                                    "executable".to_string()
                                }
                                (oak_core::TreeEntryKind::Blob, oak_core::FileMode::Symlink) => {
                                    "symlink".to_string()
                                }
                            },
                        })
                        .collect(),
                });
            }
        }

        // Now grab the flat blob list from the same root tree.
        if let Some(manifest) = repo.get_manifest(&commit.manifest_hash)? {
            // (kept this scope to minimize diff for the blob loop below)
            {
                for entry in &manifest.entries {
                    if !seen_blobs.contains(&entry.blob_hash) {
                        if let Some(blob) = repo.get_blob(&entry.blob_hash)? {
                            if blob.size >= LARGE_FILE_THRESHOLD {
                                // Chunk large blob using FastCDC
                                let chunked = chunk_content(&blob.content);
                                let chunk_refs: Vec<ChunkRef> = chunked
                                    .iter()
                                    .map(|(info, _)| ChunkRef {
                                        hash: info.hash.to_string(),
                                        offset: info.offset,
                                        size: info.length,
                                    })
                                    .collect();

                                for (info, data) in chunked {
                                    all_chunk_data.push((
                                        ChunkRef {
                                            hash: info.hash.to_string(),
                                            offset: info.offset,
                                            size: info.length,
                                        },
                                        data,
                                    ));
                                }

                                blobs.push(BlobData {
                                    hash: blob.hash.to_string(),
                                    content: Vec::new(),
                                    size: blob.size,
                                    chunks: chunk_refs,
                                });
                            } else {
                                // Small blob — include content inline
                                blobs.push(BlobData {
                                    hash: blob.hash.to_string(),
                                    content: blob.content,
                                    size: blob.size,
                                    chunks: Vec::new(),
                                });
                            }
                            seen_blobs.insert(entry.blob_hash.clone());
                        }
                    }
                }
            }
        }
    }
    if let Some(pb) = collect_pb {
        pb.finish_and_clear();
    }

    // Ask the server which of these blobs it already has so we can drop
    // their content from the push payload. Without this we re-upload every
    // blob reachable from HEAD on every push — for a 1-file commit on a
    // 250-file repo that's ~99% wasted bandwidth and an equal amount of
    // wasted R2 PUTs + Postgres no-op inserts on the server.
    //
    // Best-effort: any non-200 (e.g. older server without /blobs/check)
    // falls through to "send everything," matching the pre-dedup behavior.
    if !blobs.is_empty() {
        let blob_hashes: Vec<String> = blobs.iter().map(|b| b.hash.clone()).collect();
        output::vlog(&format!(
            "push: POST {}/api/{}/blobs/check ({} hashes)",
            remote,
            endpoint_path,
            blob_hashes.len()
        ));
        let t_check = std::time::Instant::now();
        let resp_result = with_auth(
            client
                .post(format!("{remote}/api/{endpoint_path}/blobs/check"))
                .json(&serde_json::json!({ "hashes": blob_hashes })),
            api_key,
        )
        .send()
        .await;

        match resp_result {
            Ok(resp) if resp.status().is_success() => {
                let body: BlobCheckResponse = resp
                    .json()
                    .await
                    .map_err(|e| OakError::Http(e.to_string()))?;
                let missing: std::collections::HashSet<String> = body.missing.into_iter().collect();
                let before = blobs.len();
                blobs.retain(|b| missing.contains(&b.hash));
                output::vlog(&format!(
                    "push: blobs/check kept {}/{} blob(s) in {:.3}s",
                    blobs.len(),
                    before,
                    t_check.elapsed().as_secs_f64()
                ));
            }
            Ok(resp) => {
                // 404 = old server; any other status = transient issue. In
                // both cases sending all blobs is correct and safe.
                output::vlog(&format!(
                    "push: blobs/check returned {} in {:.3}s; sending all blobs",
                    resp.status(),
                    t_check.elapsed().as_secs_f64()
                ));
            }
            Err(e) => {
                output::vlog(&format!(
                    "push: blobs/check request failed ({e}); sending all blobs"
                ));
            }
        }
    }

    // If total inline blob content exceeds 15 MB, move small blobs into the chunk
    // upload pipeline to avoid hitting Cloudflare's 100 MB request-body limit.
    // Note: Vec<u8> serializes as a JSON number array, inflating ~5x (each byte
    // becomes up to 4-5 chars), so 15 MB raw ≈ 75 MB on the wire.
    const MAX_INLINE_BYTES: u64 = 15 * 1024 * 1024;
    let total_inline: u64 = blobs.iter().map(|b| b.content.len() as u64).sum();
    if total_inline > MAX_INLINE_BYTES {
        for blob in &mut blobs {
            if blob.content.is_empty() {
                continue; // already chunked
            }
            let data = std::mem::take(&mut blob.content);
            let chunk_ref = ChunkRef {
                hash: blob.hash.clone(),
                offset: 0,
                size: data.len() as u32,
            };
            all_chunk_data.push((chunk_ref.clone(), data));
            blob.chunks = vec![chunk_ref];
        }
    }

    let total_inline_bytes: u64 = blobs.iter().map(|b| b.content.len() as u64).sum();
    output::vlog(&format!(
        "push: collected {} blob(s) ({} inline bytes), {} tree(s), {} chunk(s) in {:.3}s",
        blobs.len(),
        total_inline_bytes,
        trees.len(),
        all_chunk_data.len(),
        t_collect.elapsed().as_secs_f64()
    ));

    // Upload chunks for large blobs: first check which are missing, then upload only those
    if !all_chunk_data.is_empty() {
        // Deduplicate chunks
        let mut unique_chunks: std::collections::HashMap<String, Vec<u8>> =
            std::collections::HashMap::new();
        for (chunk_ref, data) in all_chunk_data {
            unique_chunks.entry(chunk_ref.hash).or_insert(data);
        }

        let chunk_hashes: Vec<String> = unique_chunks.keys().cloned().collect();
        // Sizes parallel to chunk_hashes — sent so the server can enforce the
        // organization storage quota before issuing presigned upload URLs.
        let chunk_sizes: Vec<u64> = chunk_hashes
            .iter()
            .map(|h| unique_chunks.get(h).map(|d| d.len() as u64).unwrap_or(0))
            .collect();

        output::info(&format!(
            "Checking {} chunk(s) on server...",
            chunk_hashes.len()
        ));

        // Ask server which chunks are missing (returns presigned upload URLs if R2 is configured)
        output::vlog(&format!(
            "push: POST {}/api/{}/chunks/check ({} hashes)",
            remote,
            endpoint_path,
            chunk_hashes.len()
        ));
        let t_check = std::time::Instant::now();
        let check_resp = with_auth(
            client
                .post(format!("{remote}/api/{endpoint_path}/chunks/check"))
                .json(&serde_json::json!({
                    "hashes": chunk_hashes,
                    "sizes": chunk_sizes,
                })),
            api_key,
        )
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
        output::vlog(&format!(
            "push: chunks/check returned {} in {:.3}s",
            check_resp.status(),
            t_check.elapsed().as_secs_f64()
        ));

        if !check_resp.status().is_success() {
            let err = check_resp.text().await.unwrap_or_default();
            return Err(OakError::Server(format!("Failed to check chunks: {err}")));
        }

        let check_result: ChunkCheckResponse = check_resp
            .json()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;

        if !check_result.missing.is_empty() {
            let total = check_result.missing.len();
            let total_bytes: u64 = check_result
                .missing
                .iter()
                .filter_map(|info| unique_chunks.get(&info.hash).map(|d| d.len() as u64))
                .sum();

            output::vlog(&format!(
                "push: uploading {total} chunk(s), {total_bytes} bytes (concurrency={MAX_CONCURRENT_TRANSFERS})"
            ));
            let t_upload = std::time::Instant::now();

            let pb = indicatif::ProgressBar::new(total_bytes);
            pb.set_style(
                indicatif::ProgressStyle::default_bar()
                    .template(
                        "  Uploading [{bar:30.green/dim}] {bytes}/{total_bytes} ({bytes_per_sec})",
                    )
                    .unwrap()
                    .progress_chars("━╸─"),
            );

            // Upload chunks concurrently (up to MAX_CONCURRENT_TRANSFERS at a time)
            let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_TRANSFERS));
            let mut join_set: JoinSet<Result<Option<(String, u64)>>> = JoinSet::new();

            // Clone borrowed values into owned types for task ownership
            let remote_owned = remote.to_string();
            let endpoint_owned = endpoint_path.to_string();
            let api_key_owned = api_key.map(|s| s.to_string());

            for info in check_result.missing {
                if let Some(data) = unique_chunks.remove(&info.hash) {
                    let client = client.clone();
                    let pb = pb.clone();
                    let sem = semaphore.clone();
                    let remote = remote_owned.clone();
                    let endpoint = endpoint_owned.clone();
                    let api_key = api_key_owned.clone();
                    let hash = info.hash;
                    let upload_url = info.upload_url;

                    join_set.spawn(async move {
                        let _permit = sem
                            .acquire_owned()
                            .await
                            .map_err(|e| OakError::Http(e.to_string()))?;
                        let data_len = data.len() as u64;

                        if let Some(url) = upload_url {
                            // Upload directly to R2 via presigned URL
                            let resp = client
                                .put(&url)
                                .header("content-type", "application/octet-stream")
                                .body(data)
                                .send()
                                .await
                                .map_err(|e| OakError::Http(e.to_string()))?;
                            if !resp.status().is_success() {
                                let err = resp.text().await.unwrap_or_default();
                                return Err(OakError::Server(format!(
                                    "Failed to upload chunk to R2: {err}"
                                )));
                            }
                            pb.inc(data_len);
                            Ok(Some((hash, data_len)))
                        } else {
                            // No R2 — upload through the server
                            let mut builder = client
                                .put(format!("{remote}/api/{endpoint}/chunks/{hash}"))
                                .header("content-type", "application/octet-stream")
                                .body(data);
                            if let Some(ref key) = api_key {
                                builder = builder.header("authorization", format!("Bearer {key}"));
                            }
                            let resp = builder
                                .send()
                                .await
                                .map_err(|e| OakError::Http(e.to_string()))?;
                            if !resp.status().is_success() {
                                let err = resp.text().await.unwrap_or_default();
                                return Err(OakError::Server(format!(
                                    "Failed to upload chunk: {err}"
                                )));
                            }
                            pb.inc(data_len);
                            Ok(None)
                        }
                    });
                }
            }

            // Collect all results, then check for errors
            let mut raw_results = Vec::new();
            while let Some(result) = join_set.join_next().await {
                raw_results.push(result);
            }
            pb.finish_and_clear();

            let mut r2_uploaded: Vec<(String, u64)> = Vec::new();
            for result in raw_results {
                let inner =
                    result.map_err(|e| OakError::Http(format!("Upload task panicked: {e}")))?;
                if let Some(r2_info) = inner? {
                    r2_uploaded.push(r2_info);
                }
            }

            output::vlog(&format!(
                "push: chunk uploads done in {:.3}s",
                t_upload.elapsed().as_secs_f64()
            ));
            output::success(&format!("Uploaded {total} chunk(s)"));

            // Confirm R2 uploads with the server so it records metadata
            if !r2_uploaded.is_empty() {
                let entries: Vec<serde_json::Value> = r2_uploaded
                    .iter()
                    .map(|(hash, size)| serde_json::json!({ "hash": hash, "size": size }))
                    .collect();
                let confirm_resp = with_auth(
                    client
                        .post(format!("{remote}/api/{endpoint_path}/chunks/uploaded"))
                        .json(&serde_json::json!({ "hashes": entries })),
                    api_key,
                )
                .send()
                .await
                .map_err(|e| OakError::Http(e.to_string()))?;
                if !confirm_resp.status().is_success() {
                    let err = confirm_resp.text().await.unwrap_or_default();
                    return Err(OakError::Server(format!(
                        "Failed to confirm chunk uploads: {err}"
                    )));
                }
            }
        } else {
            output::info("All chunks already on server, skipping upload");
        }
    }

    // Build commit data
    let commit_data: Vec<CommitData> = commits
        .iter()
        .map(|c| CommitData {
            hash: c.hash.to_string(),
            branch_name: c.branch_name.clone(),
            parent_hash: c.parent_hash.as_ref().map(|h| h.to_string()),
            merge_parent_hash: c.merge_parent_hash.as_ref().map(|h| h.to_string()),
            manifest_hash: c.manifest_hash.to_string(),
            author: c.author.clone(),
            message: c.message.clone(),
            timestamp: c.timestamp.to_rfc3339(),
            files: c
                .files
                .iter()
                .map(|f| FileChangeData {
                    path: f.path.clone(),
                    change_type: match f.change_type {
                        oak_core::ChangeType::Added => "added".to_string(),
                        oak_core::ChangeType::Modified => "modified".to_string(),
                        oak_core::ChangeType::Deleted => "deleted".to_string(),
                        oak_core::ChangeType::Renamed => "renamed".to_string(),
                    },
                    old_blob_hash: f.old_blob_hash.as_ref().map(|h| h.to_string()),
                    new_blob_hash: f.new_blob_hash.as_ref().map(|h| h.to_string()),
                    old_path: f.old_path.clone(),
                })
                .collect(),
        })
        .collect();

    // Build branch data if we have a current branch
    let branch_data = if let Some(name) = branch_name {
        repo.get_branch(name)?.map(|br| BranchData {
            name: br.name,
            description: br.description,
            parent_branch: br.parent_branch,
            status: br.status.to_string(),
            created_at: br.created_at.to_rfc3339(),
        })
    } else {
        None
    };

    // Send push request
    let push_req = PushRequest {
        expected_head: if force {
            None
        } else {
            remote_head.map(|h| h.to_string())
        },
        expected_branch_head: None, // server derives it from commit parents
        force,
        branch: branch_data,
        commits: commit_data,
        blobs,
        trees,
    };

    output::vlog(&format!(
        "push: POST {}/api/{}/push ({} commit(s), {} blob(s), {} tree(s))",
        remote,
        endpoint_path,
        push_req.commits.len(),
        push_req.blobs.len(),
        push_req.trees.len()
    ));
    let t_push = std::time::Instant::now();
    let resp = with_auth(
        client
            .post(format!("{remote}/api/{endpoint_path}/push"))
            .header("x-oak-user", super::commit::get_author())
            .json(&push_req),
        api_key,
    )
    .send()
    .await
    .map_err(|e| OakError::Http(e.to_string()))?;
    output::vlog(&format!(
        "push: /push returned {} in {:.3}s",
        resp.status(),
        t_push.elapsed().as_secs_f64()
    ));

    if !resp.status().is_success() {
        let err_text = resp.text().await.unwrap_or_default();
        return Err(OakError::Server(err_text));
    }

    let push_resp: PushResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if push_resp.success {
        output::success("Push complete");
    } else {
        return Err(OakError::ConflictDetected);
    }

    // First push to a freshly-created repo: the push lands on the user's
    // personal/feature branch, but `main` stays empty. The web UI's Code
    // tab and any Pages site then look empty — which reads like the push
    // silently failed. Offer to land the work on main right here so the
    // user doesn't have to discover the squash-merge step on their own.
    if created_repo {
        if let Some(name) = branch_name {
            let parented_on_main = repo
                .get_branch(name)
                .ok()
                .flatten()
                .and_then(|b| b.parent_branch)
                .is_some_and(|p| p == "main");
            if parented_on_main {
                if is_interactive() {
                    let prompt = format!(
                        "Land '{name}' on main now? Otherwise the repo will look empty in the web UI until you run `oak merge`."
                    );
                    let confirm = Confirm::new()
                        .with_prompt(prompt)
                        .default(true)
                        .interact()
                        .unwrap_or(false);
                    if confirm {
                        super::merge::merge_to_main_via_server(repo, work_tree, name).await?;
                    }
                } else {
                    output::info(
                        "Run `oak merge` to land this on main — otherwise the repo will look empty in the web UI.",
                    );
                }
            }
        }
    }

    Ok(())
}

/// Whether stdin is a TTY — used to skip interactive prompts in scripted
/// pushes so they don't hang waiting for input.
fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// GET `/api/{endpoint}/branches/{branch}` and pull out the head hash.
/// Returns `None` on 404 (branch doesn't exist on the server yet).
/// Used by the bootstrap loop to drive iteration progress without
/// duplicating the per-branch GET that `push_async_with_endpoint`
/// already does internally.
async fn fetch_remote_branch_head(
    client: &reqwest::Client,
    remote: &str,
    endpoint_path: &str,
    branch: &str,
    api_key: Option<&str>,
) -> Result<Option<Hash>> {
    let resp = with_auth(
        client.get(format!("{remote}/api/{endpoint_path}/branches/{branch}")),
        api_key,
    )
    .send()
    .await
    .map_err(|e| OakError::Http(e.to_string()))?;

    if resp.status().is_success() {
        let body: BranchHeadResponse = resp
            .json()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        Ok(body.head.map(Hash))
    } else if resp.status().as_u16() == 404 {
        Ok(None)
    } else {
        let err_text = resp.text().await.unwrap_or_default();
        Err(OakError::Server(err_text))
    }
}

#[derive(Deserialize)]
struct OrganizationItem {
    slug: String,
    name: String,
    emoji: Option<String>,
    role: Option<String>,
}

#[derive(Deserialize)]
struct OrganizationListResp {
    organizations: Vec<OrganizationItem>,
}

/// Non-interactive first-push linking from `--repo OWNER/NAME` (or `OAK_REPO`).
/// Parses and validates the spec, persists `RepoOwner` + `RepoName`, and lets
/// the downstream push auto-create the repo on the server when it 404s. OWNER
/// must already be an organization slug you can push to.
fn link_remote_identity(
    repo: &SqliteRepository,
    remote: &str,
    spec: &str,
) -> Result<(String, String)> {
    let (owner, name) = spec.split_once('/').ok_or_else(|| {
        OakError::Server(format!(
            "--repo expects OWNER/NAME (got '{spec}'). Example: --repo my-org/my-repo"
        ))
    })?;
    let owner = owner.trim();
    let name = name.trim();
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || name.contains(' ')
        || name.contains('\\')
        || owner.contains(' ')
    {
        return Err(OakError::Server(format!(
            "Invalid --repo value '{spec}'. Use OWNER/NAME with no spaces; NAME may not contain '/' or '\\'."
        )));
    }

    repo.set_metadata(MetadataKey::RepoOwner, owner)?;
    repo.set_metadata(MetadataKey::RepoName, name)?;
    output::success(&format!("Linked to {owner}/{name} on {remote}"));
    Ok((owner.to_string(), name.to_string()))
}

/// Interactive first-push setup: pick or create an organization, choose a repo
/// name, persist `RepoOwner` + `RepoName` to local metadata. The downstream
/// push flow auto-creates the repo on the server when it 404s.
async fn setup_remote_identity(
    repo: &SqliteRepository,
    remote: &str,
    api_key: Option<&str>,
) -> Result<(String, String)> {
    // No TTY → the org picker below would hang. Fail with the exact flag to
    // re-run with, so scripted / agent pushes get an actionable error instead
    // of a stuck prompt.
    if !is_interactive() {
        return Err(OakError::Server(format!(
            "This repository isn't linked to a remote, and there's no terminal for the \
             interactive org picker. Re-run with `oak push --repo <owner>/<name>` (or set \
             OAK_REPO=<owner>/<name>), where <owner> is an existing organization slug on \
             {remote}. Create the org first on {remote} if you don't have one yet."
        )));
    }

    output::blank();
    output::header("This repository isn't linked to a remote yet");
    output::info(&format!(
        "Pick an organization on {remote} to push to. The repository will be created there."
    ));
    output::blank();

    let proceed = Confirm::new()
        .with_prompt("Configure remote organization and repo name?")
        .default(true)
        .interact()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
    if !proceed {
        return Err(OakError::Server(
            "Push cancelled. Run `oak push` again to configure the remote, or `oak clone` to pick an existing repo.".to_string(),
        ));
    }

    let token = api_key
        .map(String::from)
        .or_else(|| super::credentials::get_token_for_server(remote));
    let token = match token {
        Some(t) => t,
        None => {
            output::error("You're not logged in to this server.");
            output::info(&format!("Run: oak login -r {remote}"));
            return Err(OakError::Server(
                "Authentication required to push a new repo".to_string(),
            ));
        }
    };

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{remote}/api/orgs"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(OakError::Server(format!(
            "Failed to fetch organizations ({status}): {body}"
        )));
    }
    let list: OrganizationListResp = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    let owner = if list.organizations.is_empty() {
        output::info("You don't belong to any organizations yet. Let's create one.");
        create_organization_interactive(&client, remote, &token).await?
    } else {
        let mut items: Vec<String> = list
            .organizations
            .iter()
            .map(|ws| {
                let emoji = ws
                    .emoji
                    .as_deref()
                    .map(|e| format!("{e} "))
                    .unwrap_or_default();
                let role = ws
                    .role
                    .as_deref()
                    .map(|r| format!(" [{r}]"))
                    .unwrap_or_default();
                format!("{}{} ({}){}", emoji, ws.name, ws.slug, role)
            })
            .collect();
        items.push("+ Create new organization…".to_string());

        let idx = Select::new()
            .with_prompt("Organization")
            .items(&items)
            .default(0)
            .interact()
            .map_err(|e| OakError::Io(std::io::Error::other(e)))?;

        if idx == list.organizations.len() {
            create_organization_interactive(&client, remote, &token).await?
        } else {
            list.organizations[idx].slug.clone()
        }
    };

    let default_name = repo
        .get_metadata(MetadataKey::RepoName)?
        .unwrap_or_default();
    let name: String = Input::new()
        .with_prompt(format!("Repo name (under {owner})"))
        .with_initial_text(&default_name)
        .interact_text()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
    let name = name.trim().to_string();
    if name.is_empty() || name.contains('/') || name.contains(' ') || name.contains('\\') {
        return Err(OakError::Server(
            "Repo name cannot be empty or contain spaces, '/', or '\\'".to_string(),
        ));
    }

    repo.set_metadata(MetadataKey::RepoOwner, &owner)?;
    repo.set_metadata(MetadataKey::RepoName, &name)?;
    output::success(&format!("Linked to {owner}/{name} on {remote}"));
    output::blank();

    Ok((owner, name))
}

async fn create_organization_interactive(
    client: &reqwest::Client,
    remote: &str,
    token: &str,
) -> Result<String> {
    let slug: String = Input::new()
        .with_prompt("New organization slug (lowercase, no spaces)")
        .interact_text()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
    let slug = slug.trim().to_string();
    if slug.is_empty() || slug.contains('/') || slug.contains(' ') {
        return Err(OakError::Server(
            "Organization slug cannot be empty or contain spaces or '/'".to_string(),
        ));
    }

    let display_name: String = Input::new()
        .with_prompt("Organization display name")
        .with_initial_text(&slug)
        .interact_text()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;

    let resp = client
        .post(format!("{remote}/api/orgs"))
        .header("authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "slug": slug,
            "name": display_name.trim(),
        }))
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(OakError::Server(format!(
            "Failed to create organization ({status}): {body}"
        )));
    }

    output::success(&format!("Created organization '{slug}'"));
    Ok(slug)
}
