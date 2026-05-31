//! `oak serve` — a minimal, self-hostable Oak server backed by SQLite.
//!
//! This is the answer to "I want to host my own Oak repos without running the
//! whole oak.space stack." It speaks the same push/pull/clone wire protocol the
//! hosted server speaks (the shared [`oak_core::protocol`] types), but with a
//! deliberately tiny surface:
//!
//!   * **Storage is SQLite + the local blob store** — one `*.oakdb` file per
//!     repo under `--dir`, opened through [`oak_core::SqliteRepository`] (the
//!     exact store the `oak` CLI uses locally). No PostgreSQL, no R2.
//!   * **No organizations, users, or auth model.** Repos are addressed as
//!     `{owner}/{name}` purely as a directory namespace — `owner` carries no
//!     permission meaning. An optional `--token` shared secret is the only
//!     access control, and it's off by default.
//!
//! Point the CLI at it with `oak clone <host>/<owner>/<name>`, `oak push`,
//! `oak pull` — unchanged. Because this server never configures R2, every
//! chunk-check response sets `upload_url: None`, which routes the client onto
//! the server-mediated `PUT /chunks/{hash}` + inline-download path it already
//! supports in production. No client changes are required.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path as AxPath, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};

use oak_core::protocol::{
    tree_data_to_core, tree_to_wire, BlobCheckRequest, BlobCheckResponse, BlobData,
    BranchPullData, ChunkCheckRequest, ChunkCheckServerResponse, ChunkDownloadInfo,
    ChunkDownloadRequest, ChunkDownloadResponse, ChunkRefData, ChunkUploadInfo,
    ChunkUploadedRequest, CommitData, CreateRepoRequest, ErrorResponse, FileChangeData, PullQuery,
    PullResponse, PushRequest, PushResponse, RepoResponse,
};
use oak_core::{
    collect_tree_objects, reassemble_chunks, Blob, Branch, BranchStatus, ChangeType, ChunkInfo,
    Commit, FileChange, Hash, OakError, Repository, Result, SqliteRepository, Tree,
};

/// Shared server state. Cheap to clone (a path + an `Arc`), as axum requires.
#[derive(Clone)]
struct ServeState {
    /// Directory holding one `<owner>/<name>.oakdb` per repo.
    root: PathBuf,
    /// Optional shared bearer token. When `Some`, every request must send
    /// `Authorization: Bearer <token>`. When `None`, the server is open.
    token: Option<Arc<String>>,
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

/// A handler error that renders as `(status, {"error": ...})` — the same JSON
/// error shape the hosted server uses, so the CLI's existing error parsing
/// works unchanged.
struct ServeError {
    status: StatusCode,
    message: String,
}

impl ServeError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
}

impl IntoResponse for ServeError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

impl From<OakError> for ServeError {
    fn from(e: OakError) -> Self {
        ServeError::internal(e.to_string())
    }
}

/// Run a synchronous (rusqlite) repository operation off the async runtime.
/// SQLite calls block, so they must not run on a tokio worker thread.
async fn blocking<T>(
    f: impl FnOnce() -> std::result::Result<T, ServeError> + Send + 'static,
) -> std::result::Result<T, ServeError>
where
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => Err(ServeError::internal(format!("blocking task panicked: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// Repo path resolution
// ---------------------------------------------------------------------------

/// Validate a single `{owner}` / `{name}` path segment and reject anything
/// that could escape the data directory (`..`, slashes, etc.).
fn check_segment(seg: &str) -> std::result::Result<(), ServeError> {
    let ok = !seg.is_empty()
        && seg != ".."
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(ServeError::bad_request(format!(
            "invalid repo path segment: {seg:?}"
        )))
    }
}

/// `<root>/<owner>/<name>.oakdb`.
fn repo_db_path(root: &Path, owner: &str, name: &str) -> std::result::Result<PathBuf, ServeError> {
    check_segment(owner)?;
    check_segment(name)?;
    Ok(root.join(owner).join(format!("{name}.oakdb")))
}

/// Open an existing repo, or 404 if its file doesn't exist yet.
fn open_existing(
    root: &Path,
    owner: &str,
    name: &str,
) -> std::result::Result<SqliteRepository, ServeError> {
    let path = repo_db_path(root, owner, name)?;
    if !path.exists() {
        return Err(ServeError::not_found(format!(
            "repository '{owner}/{name}' not found"
        )));
    }
    SqliteRepository::open(&path).map_err(ServeError::from)
}

/// Open a repo for writing, creating the file + schema if absent. Foreign-key
/// enforcement is off (like the mount cache) so we can ingest server-shaped
/// data — branches whose parent isn't materialized, commits out of topo order —
/// exactly as the CLI's bulk-import path does.
fn open_for_write(
    root: &Path,
    owner: &str,
    name: &str,
) -> std::result::Result<SqliteRepository, ServeError> {
    let path = repo_db_path(root, owner, name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ServeError::internal(format!("create repo dir: {e}")))?;
    }
    SqliteRepository::open_relaxed(&path).map_err(ServeError::from)
}

// ---------------------------------------------------------------------------
// Default-branch resolution + conversions (mirrors the hosted server/CLI)
// ---------------------------------------------------------------------------

/// Resolve the name of the repo's default branch for clone/pull:
///   1. `main` if it has a head (the post-merge norm);
///   2. else the first root branch (no parent) with a head;
///   3. else *any* branch with a head.
///
/// Step 3 matters for this minimal server: it has no squash-merge-to-`main`
/// endpoint, so a repo that's only ever had a feature branch pushed to it would
/// otherwise be un-cloneable. Falling back to the feature branch makes the
/// plain "push here, clone there" flow work without a merge step.
fn default_branch_name(repo: &SqliteRepository) -> Result<Option<String>> {
    if repo.get_branch_head("main")?.is_some() {
        return Ok(Some("main".to_string()));
    }
    let branches = repo.list_branches()?;
    for b in &branches {
        if b.parent_branch.is_none() && repo.get_branch_head(&b.name)?.is_some() {
            return Ok(Some(b.name.clone()));
        }
    }
    for b in &branches {
        if repo.get_branch_head(&b.name)?.is_some() {
            return Ok(Some(b.name.clone()));
        }
    }
    Ok(None)
}

fn branch_to_pull(b: &Branch) -> BranchPullData {
    BranchPullData {
        name: b.name.clone(),
        description: b.description.clone(),
        parent_branch: b.parent_branch.clone(),
        status: b.status.to_string(),
        created_at: b.created_at.to_rfc3339(),
    }
}

fn commit_to_wire(c: &Commit) -> CommitData {
    CommitData {
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
                    ChangeType::Added => "added".to_string(),
                    ChangeType::Modified => "modified".to_string(),
                    ChangeType::Deleted => "deleted".to_string(),
                    ChangeType::Renamed => "renamed".to_string(),
                },
                old_blob_hash: f.old_blob_hash.as_ref().map(|h| h.to_string()),
                new_blob_hash: f.new_blob_hash.as_ref().map(|h| h.to_string()),
                old_path: f.old_path.clone(),
            })
            .collect(),
    }
}

fn parse_ts(s: &str) -> std::result::Result<chrono::DateTime<chrono::Utc>, ServeError> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&chrono::Utc))
        .map_err(|e| ServeError::bad_request(format!("invalid timestamp {s:?}: {e}")))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /api/repos` — create a repo. `organization_slug` is treated as the
/// owner namespace (no org concept); it defaults to `local`.
async fn create_repo(
    State(state): State<ServeState>,
    Json(req): Json<CreateRepoRequest>,
) -> std::result::Result<Json<RepoResponse>, ServeError> {
    let owner = req
        .organization_slug
        .clone()
        .unwrap_or_else(|| "local".to_string());
    let name = req.name.clone();
    let root = state.root.clone();
    blocking(move || {
        // Opening for write creates the file + schema.
        open_for_write(&root, &owner, &name)?;
        Ok(Json(RepoResponse {
            name,
            description: req.description,
            head: None,
            is_public: req.is_public,
            owner: Some(owner),
            emoji: None,
            updated_at: None,
            projects: Vec::new(),
        }))
    })
    .await
}

/// `GET /api/{owner}/{name}` — repo existence + default-branch head.
async fn get_repo(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
) -> std::result::Result<Json<RepoResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let head = match default_branch_name(&repo)? {
            Some(b) => repo.get_branch_head(&b)?.map(|h| h.to_string()),
            None => None,
        };
        Ok(Json(RepoResponse {
            name,
            description: None,
            head,
            is_public: true,
            owner: Some(owner),
            emoji: None,
            updated_at: None,
            projects: Vec::new(),
        }))
    })
    .await
}

/// `GET /api/{owner}/{name}/branches/{branch}` — the branch head (the client
/// reads only `head`). Unknown branch → `{ "head": null }`.
async fn get_branch(
    State(state): State<ServeState>,
    AxPath((owner, name, branch)): AxPath<(String, String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let head = repo.get_branch_head(&branch)?.map(|h| h.to_string());
        Ok(Json(serde_json::json!({ "head": head })))
    })
    .await
}

/// `POST /api/{owner}/{name}/push` — ingest commits/blobs/trees onto a branch.
/// No org/quota/`main`-protection logic — just optimistic-concurrency conflict
/// detection and a store.
async fn push(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Json(req): Json<PushRequest>,
) -> std::result::Result<Json<PushResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_for_write(&root, &owner, &name)?;

        // Optimistic-concurrency check (skipped on --force). Mirrors the hosted
        // server: a per-branch fast-forward/idempotent check when the push
        // carries branch metadata, else the legacy global-head check.
        if !req.force {
            let conflicted = if let Some(branch_data) = &req.branch {
                let pushed: HashSet<&str> = req
                    .commits
                    .iter()
                    .filter(|c| c.branch_name == branch_data.name)
                    .map(|c| c.hash.as_str())
                    .collect();
                let chain_starts: Vec<&CommitData> = req
                    .commits
                    .iter()
                    .filter(|c| {
                        c.branch_name == branch_data.name
                            && match &c.parent_hash {
                                None => true,
                                Some(p) => !pushed.contains(p.as_str()),
                            }
                    })
                    .collect();
                if chain_starts.is_empty() {
                    let expected = req.expected_head.as_ref().map(|s| Hash(s.clone()));
                    repo.get_head()? != expected
                } else {
                    let server_head = repo.get_branch_head(&branch_data.name)?;
                    let server_head_str = server_head.as_ref().map(|h| h.0.as_str());
                    let extending = chain_starts
                        .iter()
                        .any(|c| c.parent_hash.as_deref() == server_head_str);
                    let idempotent = req.commits.iter().any(|c| {
                        c.branch_name == branch_data.name
                            && Some(c.hash.as_str()) == server_head_str
                    });
                    !(server_head_str.is_none() || extending || idempotent)
                }
            } else {
                let expected = req.expected_head.as_ref().map(|s| Hash(s.clone()));
                repo.get_head()? != expected
            };

            if conflicted {
                return Ok(Json(PushResponse {
                    success: false,
                    new_head: repo.get_head()?.map(|h| h.to_string()),
                    message: "Conflict: remote head has changed. Pull first.".to_string(),
                }));
            }
        }

        // Branch metadata.
        if let Some(bd) = &req.branch {
            let branch = Branch {
                name: bd.name.clone(),
                description: bd.description.clone(),
                parent_branch: bd.parent_branch.clone(),
                status: BranchStatus::from_db_str(&bd.status),
                created_at: parse_ts(&bd.created_at)?,
            };
            repo.store_branch(&branch)?;
        }

        // Tree objects.
        for td in &req.trees {
            let tree = tree_data_to_core(td).map_err(ServeError::bad_request)?;
            repo.store_tree(&tree)?;
        }

        // Blobs. The client's download path (pull *and* clone) always expects
        // blobs to come back as chunk refs — there is no inline-content wire
        // format on read. So every blob lands as a `blob_chunks` mapping, just
        // like the hosted server:
        //   * inline blobs (bytes on the wire) → one self-chunk keyed by the
        //     blob hash;
        //   * chunked blobs → their chunks were uploaded ahead of this request
        //     via `PUT /chunks/{hash}`; we record the map and reassemble the
        //     `blobs` row (what `has_blob` uses for dedup on the next push).
        for b in &req.blobs {
            let blob_hash = Hash(b.hash.clone());
            if b.chunks.is_empty() {
                let length = u32::try_from(b.size).map_err(|_| {
                    ServeError::bad_request(format!(
                        "inline blob {} too large for single-chunk encoding ({} bytes)",
                        b.hash, b.size
                    ))
                })?;
                repo.store_chunk(&blob_hash, &b.content)?;
                repo.store_blob(&Blob {
                    hash: blob_hash.clone(),
                    content: b.content.clone(),
                    size: b.size,
                })?;
                repo.store_blob_chunks(
                    &blob_hash,
                    &[ChunkInfo {
                        hash: blob_hash.clone(),
                        offset: 0,
                        length,
                    }],
                )?;
            } else {
                let mut refs = b.chunks.clone();
                refs.sort_by_key(|c| c.offset);
                let mut datas: Vec<Vec<u8>> = Vec::with_capacity(refs.len());
                for c in &refs {
                    let content = repo.get_chunk(&Hash(c.hash.clone()))?.ok_or_else(|| {
                        ServeError::bad_request(format!(
                            "chunk {} for blob {} was not uploaded before push",
                            c.hash, b.hash
                        ))
                    })?;
                    datas.push(content);
                }
                let slices: Vec<&[u8]> = datas.iter().map(|d| d.as_slice()).collect();
                let content = reassemble_chunks(&slices);
                repo.store_blob(&Blob {
                    hash: blob_hash.clone(),
                    content,
                    size: b.size,
                })?;
                let chunk_infos: Vec<ChunkInfo> = refs
                    .iter()
                    .map(|c| ChunkInfo {
                        hash: Hash(c.hash.clone()),
                        offset: c.offset,
                        length: c.size,
                    })
                    .collect();
                repo.store_blob_chunks(&blob_hash, &chunk_infos)?;
            }
        }

        // Commits + branch heads. Commits arrive oldest-first, so the last one
        // seen per branch is that branch's new head.
        let mut heads: HashMap<String, Hash> = HashMap::new();
        for c in &req.commits {
            let files: Vec<FileChange> = c
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
            let commit = Commit {
                hash: Hash(c.hash.clone()),
                branch_name: c.branch_name.clone(),
                parent_hash: c.parent_hash.clone().map(Hash),
                merge_parent_hash: c.merge_parent_hash.clone().map(Hash),
                manifest_hash: Hash(c.manifest_hash.clone()),
                author: c.author.clone(),
                message: c.message.clone(),
                timestamp: parse_ts(&c.timestamp)?,
                files,
            };
            repo.store_commit(&commit)?;
            heads.insert(commit.branch_name.clone(), commit.hash.clone());
        }
        for (branch, head) in &heads {
            repo.set_branch_head(branch, head)?;
        }

        let target = req
            .branch
            .as_ref()
            .map(|b| b.name.clone())
            .or_else(|| req.commits.last().map(|c| c.branch_name.clone()));
        let new_head = match target.as_ref().and_then(|t| heads.get(t).cloned()) {
            Some(h) => Some(h),
            None => repo.get_head()?,
        };

        Ok(Json(PushResponse {
            success: true,
            new_head: new_head.map(|h| h.to_string()),
            message: "ok".to_string(),
        }))
    })
    .await
}

/// `GET /api/{owner}/{name}/pull` — return commits (+ reachable trees/blobs and
/// branch heads) on the requested branch since `?since=`. Clone (no
/// `branch_name`) defaults to the repo's default branch.
async fn pull(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Query(query): Query<PullQuery>,
) -> std::result::Result<Json<PullResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;

        let target = match &query.branch_name {
            Some(b) => Some(b.clone()),
            None => default_branch_name(&repo)?,
        };

        let since = query.since.as_ref().map(|s| Hash(s.clone()));
        let commits = match &target {
            Some(b) => repo.get_commits_since(b, since.as_ref())?,
            None => Vec::new(),
        };

        // Collect every tree object and blob reachable from the returned
        // commits' root trees.
        let mut seen_trees: HashSet<String> = HashSet::new();
        let mut trees = Vec::new();
        let mut seen_blobs: HashSet<String> = HashSet::new();
        let mut blobs = Vec::new();
        let empty = Tree::empty_hash();
        for c in &commits {
            let root_tree = &c.manifest_hash;
            if root_tree == &empty {
                continue;
            }
            let mut fetch = |h: &Hash| {
                repo.get_tree(h)?
                    .ok_or_else(|| OakError::ManifestNotFound(h.to_string()))
            };
            for t in collect_tree_objects(root_tree, &mut fetch)? {
                if seen_trees.insert(t.hash.0.clone()) {
                    trees.push(tree_to_wire(&t));
                }
            }
            for entry in repo.walk_tree(root_tree)? {
                if !seen_blobs.insert(entry.blob_hash.0.clone()) {
                    continue;
                }
                match repo.get_blob_chunks(&entry.blob_hash)? {
                    Some(chunks) if !chunks.is_empty() => {
                        let size = chunks.iter().map(|c| c.length as u64).sum();
                        blobs.push(BlobData {
                            hash: entry.blob_hash.to_string(),
                            content: Vec::new(),
                            size,
                            chunks: chunks
                                .iter()
                                .map(|c| ChunkRefData {
                                    hash: c.hash.to_string(),
                                    offset: c.offset,
                                    size: c.length,
                                })
                                .collect(),
                        });
                    }
                    _ => {
                        if let Some(blob) = repo.get_blob(&entry.blob_hash)? {
                            blobs.push(BlobData {
                                hash: blob.hash.to_string(),
                                content: blob.content,
                                size: blob.size,
                                chunks: Vec::new(),
                            });
                        }
                    }
                }
            }
        }

        let all_branches = repo.list_branches()?;
        let branches: Vec<BranchPullData> = all_branches.iter().map(branch_to_pull).collect();
        let (branch, head) = match &target {
            Some(b) => {
                let bp = all_branches.iter().find(|x| &x.name == b).map(branch_to_pull);
                (bp, repo.get_branch_head(b)?.map(|h| h.to_string()))
            }
            None => (None, None),
        };
        let commits_wire: Vec<CommitData> = commits.iter().map(commit_to_wire).collect();

        Ok(Json(PullResponse {
            head,
            branch,
            branches,
            commits: commits_wire,
            blobs,
            trees,
            renames: Vec::new(),
            path_prefixes: Vec::new(),
        }))
    })
    .await
}

/// `POST /api/{owner}/{name}/blobs/check` — which of `hashes` the server lacks.
async fn check_blobs(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Json(req): Json<BlobCheckRequest>,
) -> std::result::Result<Json<BlobCheckResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_for_write(&root, &owner, &name)?;
        let mut missing = Vec::new();
        for h in &req.hashes {
            if !repo.has_blob(&Hash(h.clone()))? {
                missing.push(h.clone());
            }
        }
        Ok(Json(BlobCheckResponse { missing }))
    })
    .await
}

/// `POST /api/{owner}/{name}/chunks/check` — which of `hashes` the server
/// lacks. `upload_url` is always `None` (no R2), routing the client onto the
/// server-mediated `PUT /chunks/{hash}` path.
async fn check_chunks(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Json(req): Json<ChunkCheckRequest>,
) -> std::result::Result<Json<ChunkCheckServerResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_for_write(&root, &owner, &name)?;
        let mut missing = Vec::new();
        for h in &req.hashes {
            if !repo.has_chunk(&Hash(h.clone()))? {
                missing.push(ChunkUploadInfo {
                    hash: h.clone(),
                    upload_url: None,
                });
            }
        }
        Ok(Json(ChunkCheckServerResponse { missing }))
    })
    .await
}

/// `PUT /api/{owner}/{name}/chunks/{hash}` — store one chunk's raw bytes.
async fn upload_chunk(
    State(state): State<ServeState>,
    AxPath((owner, name, hash)): AxPath<(String, String, String)>,
    body: Bytes,
) -> std::result::Result<StatusCode, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_for_write(&root, &owner, &name)?;
        repo.store_chunk(&Hash(hash), &body)?;
        Ok(StatusCode::OK)
    })
    .await
}

/// `POST /api/{owner}/{name}/chunks/download` — inline chunk content (no R2, so
/// never a presigned URL). Missing hashes are silently omitted.
async fn download_chunks(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Json(req): Json<ChunkDownloadRequest>,
) -> std::result::Result<Json<ChunkDownloadResponse>, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let mut chunks = Vec::new();
        for h in &req.hashes {
            if let Some(content) = repo.get_chunk(&Hash(h.clone()))? {
                chunks.push(ChunkDownloadInfo {
                    hash: h.clone(),
                    download_url: None,
                    content: Some(content),
                });
            }
        }
        Ok(Json(ChunkDownloadResponse { chunks }))
    })
    .await
}

/// `POST /api/{owner}/{name}/chunks/uploaded` — a no-op here. On the hosted
/// server this confirms a presigned-R2 upload landed; with the server-mediated
/// path the bytes are already durable from `PUT /chunks/{hash}`.
async fn confirm_chunks_uploaded(
    State(_state): State<ServeState>,
    AxPath((_owner, _name)): AxPath<(String, String)>,
    Json(_req): Json<ChunkUploadedRequest>,
) -> StatusCode {
    StatusCode::OK
}

// ---------------------------------------------------------------------------
// Auth middleware + router
// ---------------------------------------------------------------------------

/// Optional shared-secret gate. Active only when `--token` is set.
async fn require_token(
    State(state): State<ServeState>,
    request: Request,
    next: Next,
) -> std::result::Result<Response, ServeError> {
    if let Some(expected) = &state.token {
        let presented = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "));
        if presented != Some(expected.as_str()) {
            return Err(ServeError::new(
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token",
            ));
        }
    }
    Ok(next.run(request).await)
}

fn build_router(state: ServeState) -> Router {
    Router::new()
        .route("/api/repos", post(create_repo))
        .route("/api/{owner}/{name}", get(get_repo))
        .route("/api/{owner}/{name}/push", post(push))
        .route("/api/{owner}/{name}/pull", get(pull))
        .route("/api/{owner}/{name}/branches/{branch}", get(get_branch))
        .route("/api/{owner}/{name}/blobs/check", post(check_blobs))
        .route("/api/{owner}/{name}/chunks/check", post(check_chunks))
        .route("/api/{owner}/{name}/chunks/{hash}", put(upload_chunk))
        .route("/api/{owner}/{name}/chunks/download", post(download_chunks))
        .route(
            "/api/{owner}/{name}/chunks/uploaded",
            post(confirm_chunks_uploaded),
        )
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        // Allow large blob/chunk uploads (default axum limit is 2 MB).
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(state)
}

/// Entry point for `oak serve` (run on the CLI's shared tokio runtime).
pub async fn run(dir: PathBuf, port: u16, token: Option<String>) -> Result<()> {
    std::fs::create_dir_all(&dir)?;
    let root = dir.canonicalize().unwrap_or(dir);
    let has_token = token.is_some();
    let state = ServeState {
        root: root.clone(),
        token: token.map(Arc::new),
    };
    let app = build_router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| OakError::Server(format!("failed to bind {addr}: {e}")))?;

    println!("oak serve — data dir: {}", root.display());
    println!("listening on http://{addr}  (Ctrl-C to stop)");
    println!(
        "auth: {}",
        if has_token {
            "shared bearer token required"
        } else {
            "open (no token — use --token to require one)"
        }
    );
    println!("clone with:  oak clone http://<host>:{port}/<owner>/<name>");

    axum::serve(listener, app)
        .await
        .map_err(|e| OakError::Server(format!("server error: {e}")))?;
    Ok(())
}
