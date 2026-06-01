use std::fs;
use std::path::Path;
use std::sync::Arc;

use oak_core::{
    reassemble_chunks, Blob, Branch, BranchStatus, ChangeType, ChunkInfo, Commit, FileChange, Hash,
    MetadataKey, OakError, Result,
};
use oak_core::{Repository, SqliteRepository};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::output;

/// Maximum number of concurrent chunk downloads.
const MAX_CONCURRENT_TRANSFERS: usize = 8;

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
    BranchPullData as BranchData, ChunkDownloadResponse, PullResponse, TreeData,
};

fn wire_to_core_tree(td: &TreeData) -> oak_core::Result<oak_core::Tree> {
    oak_core::protocol::tree_data_to_core(td).map_err(OakError::Database)
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
            if !repo.has_chunk(&chunk_hash)? && !all_missing_sizes.contains_key(&chunk_ref.hash) {
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
            let err = dl_resp.text().await.unwrap_or_default();
            return Err(OakError::Server(format!(
                "Failed to get chunk download info: {err}"
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

        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_TRANSFERS));
        let mut join_set: JoinSet<Result<(Hash, Vec<u8>, u64)>> = JoinSet::new();
        let chunk_count = dl_result.chunks.len();

        for chunk_info in dl_result.chunks {
            let chunk_size = *all_missing_sizes.get(&chunk_info.hash).unwrap_or(&0) as u64;

            if let Some(content) = chunk_info.content {
                let chunk_hash = Hash(chunk_info.hash);
                repo.store_chunk(&chunk_hash, &content)?;
                pb.inc(chunk_size);
                bytes_since_flush += content.len() as u64;
                if bytes_since_flush >= BULK_FLUSH_BYTES {
                    bulk.flush()?;
                    bytes_since_flush = 0;
                }
            } else if let Some(download_url) = chunk_info.download_url {
                let client = client.clone();
                let pb = pb.clone();
                let sem = semaphore.clone();
                let hash = chunk_info.hash;

                join_set.spawn(async move {
                    let _permit = sem
                        .acquire_owned()
                        .await
                        .map_err(|e| OakError::Http(e.to_string()))?;

                    let chunk_resp = client
                        .get(&download_url)
                        .send()
                        .await
                        .map_err(|e| OakError::Http(e.to_string()))?;

                    if !chunk_resp.status().is_success() {
                        let err = chunk_resp.text().await.unwrap_or_default();
                        return Err(OakError::Server(format!(
                            "Failed to download chunk from R2: {err}"
                        )));
                    }

                    let chunk_content = chunk_resp
                        .bytes()
                        .await
                        .map_err(|e| OakError::Http(e.to_string()))?;
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

        let mut raw_results = Vec::new();
        while let Some(result) = join_set.join_next().await {
            raw_results.push(result);
        }
        pb.finish_and_clear();

        for result in raw_results {
            let (chunk_hash, chunk_data, _) =
                result.map_err(|e| OakError::Http(format!("Download task panicked: {e}")))??;
            bytes_since_flush += chunk_data.len() as u64;
            repo.store_chunk(&chunk_hash, &chunk_data)?;
            if bytes_since_flush >= BULK_FLUSH_BYTES {
                bulk.flush()?;
                bytes_since_flush = 0;
            }
        }

        output::success(&format!("Downloaded {chunk_count} chunk(s)"));
    }

    // Reassemble every blob from its (now-local) chunks and write the
    // `blobs` row + `blob_chunks` mapping.
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

        let blob = Blob {
            hash: Hash(blob_data.hash.clone()),
            content,
            size: blob_data.size,
        };
        repo.store_blob(&blob)?;

        let blob_hash = Hash(blob_data.hash.clone());
        repo.store_blob_chunks(&blob_hash, &chunk_infos)?;

        bytes_since_flush += blob_data.size;
        if bytes_since_flush >= BULK_FLUSH_BYTES {
            bulk.flush()?;
            bytes_since_flush = 0;
        }
    }

    bulk.commit()?;
    Ok(())
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
pub async fn run(
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

    // Phase 2: fetch current branch
    fetch_current_branch(path, remote_url, force).await?;

    // Phase 3: sync parent into current branch. The branch may have no
    // parent (e.g. detached or a freshly-init'd repo with no remote yet);
    // surface those as success — there's nothing to sync.
    match super::sync::sync_from_parent(path).await {
        Ok(()) => Ok(()),
        Err(OakError::BranchNotFound(msg)) if msg.contains("no parent to sync from") => Ok(()),
        Err(OakError::NoCommits) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Phase 2 of `oak pull`: fetch any new commits on the current branch from
/// the server and fast-forward the working tree. Was the entire body of
/// `oak pull` before `oak sync` was folded in.
pub async fn fetch_current_branch(path: &Path, remote_url: &str, force: bool) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let _lock = crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir)?;
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

    // If the repo is scoped to a team or project, ask the server to filter
    // the pull response so we don't download blob content for out-of-scope
    // paths.
    let team = super::project::active_team(&repo)?;
    let project = super::project::active_project(&repo)?;

    pull_async(
        &repo,
        remote_url,
        &format!("{owner}/{repo_name}/pull"),
        branch_name.as_deref(),
        local_head.as_ref(),
        force,
        &work_tree,
        api_key.as_deref(),
        team.as_deref(),
        project.as_deref(),
    )
    .await
}

/// Async pull implementation.
/// `endpoint_pull_path` is e.g. "repos/my-repo/pull".
///
/// `team` / `project`: when set, the server filters its response to omit
/// blob content for paths outside the team's or project's `path_prefix`
/// values. Older servers ignore the params and return the full graph; the
/// client still re-filters at materialization time, so the response is
/// correct either way — just larger on the wire.
#[allow(clippy::too_many_arguments)]
pub async fn pull_async(
    repo: &SqliteRepository,
    remote: &str,
    endpoint_pull_path: &str,
    branch_name: Option<&str>,
    local_head: Option<&Hash>,
    force: bool,
    work_tree: &Path,
    api_key: Option<&str>,
    team: Option<&str>,
    project: Option<&str>,
) -> Result<()> {
    let client = reqwest::Client::new();

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
    if let Some(team_slug) = team {
        params.push(format!("team={team_slug}"));
    }
    if let Some(project_slug) = project {
        params.push(format!("project={project_slug}"));
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
        output::error("Conflict detected: local has commits not in remote history.");
        output::info("Use 'oak pull --force' to discard local commits and sync with remote.");
        return Err(OakError::ConflictDetected);
    }

    if force && local_head.is_some() {
        output::warning("Force pull: discarding local commits not on remote");
    }

    if !resp.status().is_success() {
        let err_text = resp.text().await.unwrap_or_default();
        return Err(OakError::Server(err_text));
    }

    let pull_resp: PullResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    // Apply rename events first — they may have happened without any new
    // commits, and any incoming commits/branches in this response refer
    // to branches by their new names. Replay must precede branch/commit
    // ingestion. Skip events whose old branch is missing locally
    // (typically: this client performed the rename) so replay is
    // idempotent.
    if !pull_resp.renames.is_empty() {
        repo.set_foreign_keys(false)?;
        let _fk_guard = super::FkGuard { repo };
        let mut applied = 0usize;
        let mut highest_id: i64 = last_rename_id;
        for rn in &pull_resp.renames {
            highest_id = highest_id.max(rn.id);
            if rn.old_name == rn.new_name {
                continue;
            }
            let has_old = repo.get_branch(&rn.old_name)?.is_some();
            if !has_old {
                continue;
            }
            if repo.get_branch(&rn.new_name)?.is_some() {
                output::warning(&format!(
                    "Skipping rename '{}' -> '{}': target name already exists locally",
                    rn.old_name, rn.new_name
                ));
                continue;
            }
            repo.rename_branch(&rn.old_name, &rn.new_name)?;
            applied += 1;
        }
        if applied > 0 {
            output::info(&format!("Applied {applied} branch rename(s)"));
        }
        if highest_id > last_rename_id {
            repo.set_metadata(MetadataKey::LastRenameId, &highest_id.to_string())?;
        }
    }

    if pull_resp.commits.is_empty() {
        output::info("Already up to date");
        return Ok(());
    }

    output::info(&format!("Pulling {} commit(s)...", pull_resp.commits.len()));

    repo.set_foreign_keys(false)?;
    let _fk_guard = super::FkGuard { repo };

    // Store all branches referenced by commits
    // Sort parent-before-child so the self-referential FK on
    // branches(parent_branch) is satisfied during insertion.
    let sorted_branches = super::sort_branches_topologically(
        pull_resp.branches.iter().collect::<Vec<_>>(),
        |b: &&BranchData| b.name.as_str(),
        |b: &&BranchData| b.parent_branch.as_deref(),
    );
    for br_data in sorted_branches {
        let status = BranchStatus::from_db_str(&br_data.status);
        let created_at = chrono::DateTime::parse_from_rfc3339(&br_data.created_at)
            .map_err(|e| OakError::Database(e.to_string()))?
            .with_timezone(&chrono::Utc);

        let br = Branch {
            name: br_data.name.clone(),
            description: br_data.description.clone(),
            parent_branch: br_data.parent_branch.clone(),
            status,
            created_at,
        };
        repo.store_branch(&br)?;
    }
    // Store explicitly requested branch
    if let Some(br_data) = &pull_resp.branch {
        let status = BranchStatus::from_db_str(&br_data.status);
        let created_at = chrono::DateTime::parse_from_rfc3339(&br_data.created_at)
            .map_err(|e| OakError::Database(e.to_string()))?
            .with_timezone(&chrono::Utc);

        let br = Branch {
            name: br_data.name.clone(),
            description: br_data.description.clone(),
            parent_branch: br_data.parent_branch.clone(),
            status,
            created_at,
        };
        repo.store_branch(&br)?;

        // Set as current if we don't have one
        if repo.get_current_branch_name()?.is_none() {
            repo.set_current_branch(&br_data.name)?;
        }
    }
    // Set current branch from branches vec if not already set
    if pull_resp.branch.is_none() && repo.get_current_branch_name()?.is_none() {
        if let Some(br_data) = pull_resp.branches.first() {
            repo.set_current_branch(&br_data.name)?;
        }
    }

    // Store every tree object up front. Trees are small (no blob content) and
    // having them in the local DB lets us walk them via `repo.walk_tree`
    // below without an in-memory index.
    for tree_data in &pull_resp.trees {
        let tree = wire_to_core_tree(tree_data)?;
        repo.store_tree(&tree)?;
    }

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

    // Trees were already stored at the top of this function. Nothing else
    // to do here.

    // Store commits, tracking the latest hash per branch (commits are
    // ordered ASC by timestamp, so the last one for each branch wins).
    // Tracking per-branch — rather than a single `new_head` — prevents
    // a multi-branch payload from leaking the last commit's hash into
    // the current branch's head.
    let mut branch_heads_seen: std::collections::HashMap<String, Hash> =
        std::collections::HashMap::new();
    for commit_data in &pull_resp.commits {
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

        let commit = Commit {
            hash: Hash(commit_data.hash.clone()),
            branch_name: commit_data.branch_name.clone(),
            parent_hash: commit_data.parent_hash.clone().map(Hash),
            merge_parent_hash: commit_data.merge_parent_hash.clone().map(Hash),
            manifest_hash: Hash(commit_data.manifest_hash.clone()),
            author: commit_data.author.clone(),
            message: commit_data.message.clone(),
            timestamp,
            files,
        };

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

    // Persist branch heads for every branch we actually saw a commit on.
    for (br_name, br_head) in &branch_heads_seen {
        repo.set_branch_head(br_name, br_head)?;
    }

    // Determine the head to advance the current branch / global head to.
    // Prefer the head of the current branch from this payload; if the
    // payload had no commits on the current branch, fall back to the
    // local head (no change).
    let new_head: Option<Hash> = branch_name
        .and_then(|n| branch_heads_seen.get(n).cloned())
        .or_else(|| local_head.cloned());

    if let Some(ref head) = new_head {
        // Update legacy global head — only meaningful when we actually
        // advanced the current branch in this pull.
        repo.set_head(head)?;
    }

    // Update working directory to match new head
    if let Some(ref head) = new_head {
        update_working_dir(work_tree, repo, head)?;
    }

    output::success(&format!("Pulled {} commit(s)", pull_resp.commits.len()));

    Ok(())
}

/// Update working directory to match the given commit, filtered by the
/// active scope (if any).
fn update_working_dir(path: &Path, repo: &SqliteRepository, head: &Hash) -> Result<()> {
    let commit = repo
        .get_commit(head)?
        .ok_or_else(|| OakError::CommitNotFound(head.to_string()))?;

    let manifest = repo
        .get_manifest(&commit.manifest_hash)?
        .ok_or_else(|| OakError::ManifestNotFound(commit.manifest_hash.to_string()))?;

    let prefixes = super::project::active_prefixes(repo)?;

    let allow_partial = std::env::var("OAK_ALLOW_PARTIAL_CLONE")
        .ok()
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
    let mut skipped: Vec<String> = Vec::new();
    // Fresh stat-cache rows for the files we write, so the cache reflects the
    // pulled content instead of a stale row from another branch's version of
    // the path (which a later scan would otherwise trust and record as a
    // foreign blob). `update_working_dir` only writes — it never deletes — so
    // these are applied with no pruning below.
    let mut cache_upserts = Vec::new();

    for entry in &manifest.entries {
        if !prefixes.is_empty() && !oak_core::path_in_any_prefix(&prefixes, &entry.path) {
            continue;
        }
        let file_path = path.join(&entry.path);

        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Same rationale as `clone_repo`'s `write_working_directory`:
        // a silent skip on a missing blob produces a partial working
        // tree that masquerades as "everything modified" in `oak status`.
        // Surface the gap as a hard error instead — unless the operator
        // opts into a partial pull with OAK_ALLOW_PARTIAL_CLONE=1 to
        // recover from a server in a broken state.
        let blob = match repo.get_blob(&entry.blob_hash)? {
            Some(b) => b,
            None if allow_partial => {
                skipped.push(entry.path.clone());
                continue;
            }
            None => {
                return Err(OakError::Server(format!(
                    "pull is missing blob {} for '{}'. The server didn't ship \
                     this blob's bytes — check `blob_chunks` / R2 state for \
                     this hash on the server. Set OAK_ALLOW_PARTIAL_CLONE=1 \
                     to skip missing files instead.",
                    entry.blob_hash, entry.path,
                )));
            }
        };
        fs::write(&file_path, &blob.content)?;
        crate::file_permissions::apply_file_permissions(&file_path, entry.mode)?;
        if entry.mode != oak_core::FileMode::Symlink {
            if let Some(u) = crate::commands::commit::stat_cache_upsert(
                &entry.path,
                &file_path,
                &entry.blob_hash,
            ) {
                cache_upserts.push(u);
            }
        }
    }

    if !skipped.is_empty() {
        output::warning(&format!(
            "OAK_ALLOW_PARTIAL_CLONE: skipped {} missing file(s):",
            skipped.len()
        ));
        for p in &skipped {
            output::warning(&format!("  - {p}"));
        }
    }

    // Upsert-only (no pruning): this materializer writes files but doesn't
    // delete any, so rows for paths we didn't touch stay valid.
    repo.update_stat_cache(&cache_upserts, &[])?;

    Ok(())
}
