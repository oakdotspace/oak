//! `oak root`: roots, the separate timelines inside one repo. A root starts
//! empty or from a commit on `main` (how a release line that takes patches
//! apart from `main` is made).
//!
//! - `list | show | create | archive` wrap `/api/{owner}/{repo}/roots`.
//!   `--json` prints the server's document unchanged.
//! - `pull` materializes a root into a local directory (`roots/<name>/` at the
//!   checkout root by default), and `push` snapshots that directory into one
//!   commit on the root.
//!
//! A root directory carries a [`ROOT_STATE_FILE`] (`.oakroot`) recording which
//! server, repo and root it holds, the head it was last synced to, and that
//! head's trees. The trees are what make pulls incremental (`?since=head`
//! returns only the trees the new commits add) and pushes small (only blobs
//! the head doesn't already have are sent). Inside a checkout the directory is
//! listed in `.oak/root-dirs`, which the checkout's ignore rules read, so a
//! root's files never land on `main`.
//!
//! Pull is a three-way update over paths: a file you didn't touch takes the
//! root's version, a file only you changed is kept, and a file both sides
//! changed is a conflict that refuses the whole pull (exit 5) unless
//! `--force` takes the root's version. Push is a head compare-and-swap: if the
//! root moved since the last pull, the server answers `head_moved` and push
//! exits 5, telling you to pull.
//!
//! A root started from `main` begins with that `main` commit: its parent is on
//! `main`, outside the root, and the root owns its whole tree. Pull only ever
//! asks for the head commit, so that parent is never followed; to the root it
//! is the start of history. Pushing on top of it sends only content the head's
//! tree doesn't already hold, so `main`'s files aren't uploaded again.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use oak_core::protocol::{BlobData, CommitData, TreeData};
use oak_core::{
    Blob, Commit, FileMode, Hash, IgnorePatterns, ManifestEntry, OakError, Result, Tree,
    ROOT_DIRS_FILE, ROOT_STATE_FILE,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::output;

use super::credentials::effective_token_for_worktree;

/// Largest file a root push carries (the server's inline limit).
pub const MAX_ROOT_BLOB_BYTES: usize = 4 * 1024 * 1024;
/// Most file content one root push carries.
pub const MAX_ROOT_PUSH_BYTES: usize = 64 * 1024 * 1024;

/// Where roots land in a checkout: `roots/<name>/`.
pub const ROOTS_DIR: &str = "roots";

/// What `oak root` was asked to do.
#[derive(Debug)]
pub enum RootAction {
    List {
        archived: bool,
    },
    Show {
        root: String,
    },
    Create {
        name: String,
        owner_group: Option<String>,
        /// `main` or a full commit hash on main; `None` starts empty.
        from: Option<String>,
        sync: bool,
    },
    Archive {
        root: String,
    },
    Pull {
        root: Option<String>,
        dir: Option<PathBuf>,
        force: bool,
    },
    Push {
        root: Option<String>,
        dir: Option<PathBuf>,
    },
}

/// `~name` for a root name given as `name` or `~name`.
pub fn display_name(raw: &str) -> String {
    format!("~{}", raw.trim().trim_start_matches('~'))
}

/// The name without its `~`, for directory names.
fn bare_name(raw: &str) -> &str {
    raw.trim().trim_start_matches('~')
}

fn validate_root_arg(raw: &str) -> Result<()> {
    let bare = bare_name(raw);
    if bare.is_empty()
        || bare.len() > 64
        || !bare
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        || bare.starts_with('.')
    {
        return Err(OakError::InvalidArgument(format!(
            "invalid root name {raw:?}: use letters, digits, '-', '_' and '.'"
        )));
    }
    Ok(())
}

/// `--from` takes `main` or a full commit hash; the server checks that the
/// commit is on main's first-parent history.
fn validate_from_arg(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw == "main" || (raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Ok(raw.to_ascii_lowercase());
    }
    Err(OakError::InvalidArgument(format!(
        "--from takes `main` or a full 64-character commit hash on main, not {raw:?}"
    )))
}

// ---------------------------------------------------------------------------
// Wire types (lenient).

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct RootWire {
    name: String,
    /// The owner group's slug; absent when only the org's owners and admins
    /// write it.
    owner_group: Option<String>,
    write_mode: String,
    /// The `main` commit it started from; absent for a root that started
    /// empty.
    base_commit: Option<String>,
    head: Option<String>,
    archived_at: Option<String>,
    /// `metadata`, `read` or `write`.
    access: String,
    can_archive: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ListWire {
    roots: Vec<RootWire>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CreateWire {
    root: RootWire,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PullWire {
    head: Option<String>,
    commits: Vec<CommitData>,
    trees: Vec<TreeData>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PushWire {
    head: String,
}

fn decode<T: for<'de> Deserialize<'de>>(value: &Value) -> Result<T> {
    T::deserialize(value).map_err(|e| {
        OakError::Server(format!(
            "the server returned an unreadable roots response: {e}"
        ))
    })
}

// ---------------------------------------------------------------------------
// HTTP

/// A refusal the server explained with `{"error", "code"}`.
struct Refusal {
    status: u16,
    code: String,
    message: String,
}

enum Failure {
    Oak(OakError),
    Refused(Refusal),
}

impl From<OakError> for Failure {
    fn from(e: OakError) -> Self {
        Failure::Oak(e)
    }
}

impl Failure {
    fn into_oak(self) -> OakError {
        match self {
            Failure::Oak(e) => e,
            Failure::Refused(r) if r.status == 404 => OakError::Server(format!(
                "{} (no live root by that name (it may be archived), roots aren't turned on for this repo, or you can't see it)",
                r.message
            )),
            Failure::Refused(r) => OakError::Server(r.message),
        }
    }
}

struct Api {
    remote: String,
    token: Option<String>,
    base: String,
    client: reqwest::Client,
}

impl Api {
    fn new(remote: &str, token: Option<String>, owner: &str, repo: &str) -> Self {
        Self {
            remote: remote.trim_end_matches('/').to_string(),
            token,
            base: format!(
                "/api/{}/{}/roots",
                urlencoding::encode(owner),
                urlencoding::encode(repo)
            ),
            client: crate::http::api_client(),
        }
    }

    fn root_path(&self, root: &str) -> String {
        format!("{}/{}", self.base, urlencoding::encode(&display_name(root)))
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> std::result::Result<reqwest::Response, Failure> {
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.remote));
        if let Some(body) = body {
            req = req.json(body);
        }
        if let Some(token) = &self.token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        let status = resp.status();
        if status.as_u16() == 401 {
            return Err(OakError::AuthRequired {
                remote: crate::http::redact_url_userinfo(&self.remote),
                status: 401,
            }
            .into());
        }
        if status.is_redirection() {
            return Err(crate::http::server_error(resp).await.into());
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            #[derive(Deserialize)]
            struct ErrorBody {
                #[serde(default)]
                error: String,
                #[serde(default)]
                code: String,
            }
            let parsed: Option<ErrorBody> = serde_json::from_str(&text).ok();
            let (message, code) = match parsed {
                Some(b) if !b.error.is_empty() => (b.error, b.code),
                _ => (format!("HTTP {status}"), String::new()),
            };
            return Err(Failure::Refused(Refusal {
                status: status.as_u16(),
                code,
                message,
            }));
        }
        Ok(resp)
    }

    async fn json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> std::result::Result<Value, Failure> {
        let resp = self.request(method, path, body).await?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|_| {
            Failure::Oak(OakError::Server(format!(
                "the server returned a non-JSON response (HTTP {status})"
            )))
        })
    }

    async fn get_json(&self, path: &str) -> Result<Value> {
        self.json(reqwest::Method::GET, path, None)
            .await
            .map_err(Failure::into_oak)
    }

    async fn show(&self, root: &str) -> Result<RootWire> {
        decode(&self.get_json(&self.root_path(root)).await?)
    }

    /// One blob's bytes, verified against its hash.
    async fn blob(&self, root: &str, hash: &str) -> Result<Vec<u8>> {
        let path = format!("{}/blobs/{hash}", self.root_path(root));
        let resp = self
            .request(reqwest::Method::GET, &path, None)
            .await
            .map_err(Failure::into_oak)?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?
            .to_vec();
        let computed = Blob::new(bytes.clone()).hash;
        if computed.as_str() != hash {
            return Err(OakError::Server(format!(
                "blob {hash} came back with different content (hashes to {computed})"
            )));
        }
        Ok(bytes)
    }
}

// ---------------------------------------------------------------------------
// Local state

/// The `.oakroot` file at the top of a root directory.
#[derive(Serialize, Deserialize, Clone)]
pub struct RootState {
    pub schema_version: u32,
    pub remote: String,
    pub owner: String,
    pub repo: String,
    /// `~name`.
    pub root: String,
    /// The root head this directory was last synced to; absent before the
    /// first push to an empty root.
    #[serde(default)]
    pub head: Option<String>,
    /// That head's tree.
    #[serde(default)]
    pub manifest: Option<String>,
    /// Every tree the head's tree reaches.
    #[serde(default)]
    pub trees: Vec<TreeData>,
}

impl RootState {
    pub fn read(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(ROOT_STATE_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let state: RootState = serde_json::from_str(&text).map_err(|e| {
            OakError::InvalidArgument(format!("{} is unreadable: {e}", path.display()))
        })?;
        Ok(Some(state))
    }

    fn write(&self, dir: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| OakError::InvalidArgument(e.to_string()))?;
        crate::atomic_file::write_atomic(&dir.join(ROOT_STATE_FILE), &bytes)?;
        Ok(())
    }

    fn tree_map(&self) -> Result<HashMap<Hash, Tree>> {
        let mut map = HashMap::new();
        for td in &self.trees {
            let tree = oak_core::protocol::tree_data_to_core(td).map_err(|e| {
                OakError::InvalidArgument(format!("{ROOT_STATE_FILE} holds a bad tree: {e}"))
            })?;
            map.insert(tree.hash.clone(), tree);
        }
        Ok(map)
    }
}

type Flat = BTreeMap<String, (Hash, FileMode)>;

fn flatten(manifest: Option<&str>, trees: &HashMap<Hash, Tree>) -> Result<Flat> {
    let Some(manifest) = manifest else {
        return Ok(Flat::new());
    };
    let root = Hash::from_hex(manifest)
        .map_err(|e| OakError::InvalidArgument(format!("bad tree hash {manifest}: {e}")))?;
    let mut fetch = |h: &Hash| -> Result<Tree> {
        trees
            .get(h)
            .cloned()
            .ok_or_else(|| OakError::Server(format!("tree {h} is missing from the root")))
    };
    Ok(oak_core::tree::flatten_tree(&root, &mut fetch)?
        .into_iter()
        .map(|e| (e.path, (e.blob_hash, e.mode)))
        .collect())
}

/// The trees `manifest` reaches, as wire data.
fn closure(manifest: &str, trees: &HashMap<Hash, Tree>) -> Result<Vec<TreeData>> {
    let root = Hash::from_hex(manifest)
        .map_err(|e| OakError::InvalidArgument(format!("bad tree hash {manifest}: {e}")))?;
    let mut fetch = |h: &Hash| -> Result<Tree> {
        trees
            .get(h)
            .cloned()
            .ok_or_else(|| OakError::Server(format!("tree {h} is missing from the root")))
    };
    let mut out: Vec<TreeData> = oak_core::tree::collect_tree_objects(&root, &mut fetch)?
        .iter()
        .map(oak_core::protocol::tree_to_wire)
        .collect();
    out.sort_by(|a, b| a.hash.cmp(&b.hash));
    Ok(out)
}

/// Every file under `dir` (minus `.oakroot`, `.oak/`, OS metadata and the
/// directory's own `.oakignore`), as path → (blob hash, mode).
pub fn scan_dir(dir: &Path) -> Result<Flat> {
    let mut out = Flat::new();
    if !dir.exists() {
        return Ok(out);
    }
    let ignore = IgnorePatterns::new(dir)?;
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((abs, rel)) = stack.pop() {
        for entry in std::fs::read_dir(&abs)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(OakError::InvalidPath(format!(
                    "{} isn't valid UTF-8",
                    entry.path().display()
                )));
            };
            let path = if rel.is_empty() {
                name.to_string()
            } else {
                format!("{rel}/{name}")
            };
            let meta = std::fs::symlink_metadata(entry.path())?;
            let is_dir = meta.is_dir();
            if ignore.is_ignored(Path::new(&path), is_dir) {
                continue;
            }
            if is_dir {
                stack.push((entry.path(), path));
                continue;
            }
            let (content, mode) = read_entry(&entry.path(), &meta)?;
            out.insert(path, (Blob::new(content).hash, mode));
        }
    }
    Ok(out)
}

fn read_entry(path: &Path, meta: &std::fs::Metadata) -> Result<(Vec<u8>, FileMode)> {
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        let target = target.to_str().ok_or_else(|| {
            OakError::InvalidPath(format!("symlink {} isn't valid UTF-8", path.display()))
        })?;
        return Ok((target.replace('\\', "/").into_bytes(), FileMode::Symlink));
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            FileMode::Executable
        } else {
            FileMode::Regular
        }
    };
    #[cfg(not(unix))]
    let mode = FileMode::Regular;
    Ok((std::fs::read(path)?, mode))
}

/// List `dir` in `.oak/root-dirs` of the checkout it sits in, so the checkout
/// ignores it. Returns the repo-relative path when it did (or already had).
fn register_root_dir(dir: &Path) -> Result<Option<String>> {
    let Some(parent) = dir.parent() else {
        return Ok(None);
    };
    let Ok(ctx) = crate::resolve::resolve(parent) else {
        return Ok(None);
    };
    let work_tree = ctx
        .work_tree
        .canonicalize()
        .unwrap_or(ctx.work_tree.clone());
    let dir = dir.canonicalize().unwrap_or(dir.to_path_buf());
    let Ok(rel) = dir.strip_prefix(&work_tree) else {
        return Ok(None);
    };
    let rel = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    if rel.is_empty() {
        return Err(OakError::InvalidArgument(
            "a root can't be pulled into the top of a checkout; pass --dir".to_string(),
        ));
    }
    let existing = oak_core::root_dirs(&work_tree);
    if !existing.iter().any(|d| d == &rel) {
        let file = ctx.oak_dir.join(ROOT_DIRS_FILE);
        let mut text = std::fs::read_to_string(&file).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&rel);
        text.push('\n');
        crate::atomic_file::write_atomic(&file, text.as_bytes())?;
    }
    Ok(Some(rel))
}

// ---------------------------------------------------------------------------
// Resolving which root and directory

struct Target {
    remote: String,
    owner: String,
    repo: String,
    token: Option<String>,
}

fn checkout_target(cwd: &Path, repo_arg: Option<&str>, remote_arg: Option<&str>) -> Result<Target> {
    let (owner, repo) = super::repo_visibility::resolve_repo(cwd, repo_arg)?;
    let remote = super::repo_visibility::resolve_remote(cwd, remote_arg);
    let token = effective_token_for_worktree(&remote, cwd);
    Ok(Target {
        remote,
        owner,
        repo,
        token,
    })
}

/// The nearest directory at or above `start` holding a `.oakroot`.
fn find_root_dir(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(ROOT_STATE_FILE).is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

fn absolute(cwd: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// A root directory and what it holds.
struct Located {
    dir: PathBuf,
    state: Option<RootState>,
    api: Api,
    target: Target,
    root: String,
    /// The root as the server shows it, when we asked.
    wire: Option<RootWire>,
}

async fn locate(
    cwd: &Path,
    repo_arg: Option<&str>,
    remote_arg: Option<&str>,
    root_arg: Option<&str>,
    dir_arg: Option<&Path>,
) -> Result<Located> {
    if let Some(root) = root_arg {
        validate_root_arg(root)?;
    }
    let holds = |dir: &Path| match (root_arg, RootState::read(dir).ok().flatten()) {
        (Some(root), Some(state)) => display_name(root) == state.root,
        (None, Some(_)) => true,
        (_, None) => false,
    };
    // An explicit --dir, the root directory we're standing in, or the
    // default directory of this checkout when it already holds the root.
    // Local state wins over asking the server.
    let found = match dir_arg {
        Some(dir) => Some(absolute(cwd, dir)),
        None => find_root_dir(cwd).filter(|d| holds(d)).or_else(|| {
            let work_tree = crate::resolve::resolve(cwd).ok()?.work_tree;
            let dir = work_tree.join(ROOTS_DIR).join(bare_name(root_arg?));
            holds(&dir).then_some(dir)
        }),
    };
    let state = match &found {
        Some(dir) => RootState::read(dir)?,
        None => None,
    };
    if let Some(state) = &state {
        if let Some(root) = root_arg {
            if display_name(root) != state.root {
                return Err(OakError::InvalidArgument(format!(
                    "{} holds {}, not {}",
                    found
                        .as_ref()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default(),
                    state.root,
                    display_name(root)
                )));
            }
        }
        if let Some(spec) = repo_arg {
            let (owner, repo) = super::parse_owner_repo(spec)?;
            if owner != state.owner || repo != state.repo {
                return Err(OakError::InvalidArgument(format!(
                    "this directory holds a root of {}/{}, not {spec}",
                    state.owner, state.repo
                )));
            }
        }
        let remote = remote_arg
            .map(|r| r.trim_end_matches('/').to_string())
            .unwrap_or_else(|| state.remote.clone());
        let token = effective_token_for_worktree(&remote, cwd);
        let target = Target {
            remote,
            owner: state.owner.clone(),
            repo: state.repo.clone(),
            token,
        };
        let api = Api::new(
            &target.remote,
            target.token.clone(),
            &target.owner,
            &target.repo,
        );
        return Ok(Located {
            dir: found.expect("state came from a directory"),
            root: state.root.clone(),
            state: Some(state.clone()),
            api,
            target,
            wire: None,
        });
    }

    let Some(root_arg) = root_arg else {
        return Err(OakError::InvalidArgument(match found {
            Some(dir) => format!(
                "{} isn't a root directory yet; name the root (`oak root list` shows them)",
                dir.display()
            ),
            None => "name the root, or run this inside a root directory \
                     (`oak root list` shows the roots)"
                .to_string(),
        }));
    };
    let target = checkout_target(cwd, repo_arg, remote_arg)?;
    let api = Api::new(
        &target.remote,
        target.token.clone(),
        &target.owner,
        &target.repo,
    );
    let wire = api.show(root_arg).await?;
    let root = display_name(&wire.name);
    let dir = match found {
        Some(dir) => dir,
        None => {
            let ctx = crate::resolve::resolve(cwd).map_err(|_| {
                OakError::InvalidArgument(
                    "not inside an Oak checkout; pass --dir PATH for the root's directory"
                        .to_string(),
                )
            })?;
            ctx.work_tree.join(ROOTS_DIR).join(bare_name(&root))
        }
    };
    // The default directory may already hold this root from an earlier pull.
    let state = match RootState::read(&dir)? {
        Some(state) if state.root == root => Some(state),
        Some(state) => {
            return Err(OakError::InvalidArgument(format!(
                "{} holds {}, not {root}; pass --dir",
                dir.display(),
                state.root
            )))
        }
        None => None,
    };
    Ok(Located {
        dir,
        state,
        api,
        target,
        root,
        wire: Some(wire),
    })
}

// ---------------------------------------------------------------------------
// Refusals with their own exit codes

/// A refusal that changed nothing. With `--json` it is returned as the error
/// for `main`'s epilogue to render (the standard envelope; code and exit
/// from the contract mapping). In text mode it is printed here, with its
/// paths and next commands, and the mapping's exit code is returned.
fn refuse(
    json_out: bool,
    reason: oak_core::RootRefusal,
    message: &str,
    paths: &[String],
    next: &[String],
) -> Result<i32> {
    let refusal = OakError::RootRefused {
        reason,
        message: message.to_string(),
        paths: paths.to_vec(),
        next_commands: next.to_vec(),
    };
    if json_out {
        return Err(refusal);
    }
    output::error(message);
    for p in paths.iter().take(50) {
        output::print_line(&format!("  {p}"));
    }
    if paths.len() > 50 {
        output::print_line(&format!("  … and {} more", paths.len() - 50));
    }
    for cmd in next {
        output::info(&format!("next: {cmd}"));
    }
    Ok(crate::contract::classify(&refusal).exit.code())
}

/// The command that repeats `verb` on this directory.
fn command_for(verb: &str, dir: &Path, cwd: &Path, extra: &str) -> String {
    let shown = dir
        .strip_prefix(cwd)
        .ok()
        .map(|p| p.display().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| dir.display().to_string());
    let quoted = if shown.contains(char::is_whitespace) {
        format!("'{shown}'")
    } else {
        shown
    };
    format!("oak root {verb} --dir {quoted}{extra}")
}

// ---------------------------------------------------------------------------
// Entry point

/// Run one `oak root` action; returns the process exit code.
pub async fn run(
    cwd: &Path,
    repo_arg: Option<&str>,
    remote_arg: Option<&str>,
    json_out: bool,
    action: RootAction,
) -> Result<i32> {
    use reqwest::Method;
    match action {
        RootAction::Pull { root, dir, force } => {
            let loc = locate(cwd, repo_arg, remote_arg, root.as_deref(), dir.as_deref()).await?;
            pull(cwd, loc, force, json_out).await
        }
        RootAction::Push { root, dir } => {
            let loc = locate(cwd, repo_arg, remote_arg, root.as_deref(), dir.as_deref()).await?;
            push(cwd, loc, json_out).await
        }
        RootAction::List { archived } => {
            let target = target_for(cwd, repo_arg, remote_arg)?;
            let api = Api::new(&target.remote, target.token, &target.owner, &target.repo);
            let path = if archived {
                format!("{}?archived=1", api.base)
            } else {
                api.base.clone()
            };
            let value = api.get_json(&path).await?;
            if json_out {
                output::print_json(&value)?;
            } else {
                print_list(&decode(&value)?);
            }
            Ok(0)
        }
        RootAction::Show { root } => {
            validate_root_arg(&root)?;
            let target = target_for(cwd, repo_arg, remote_arg)?;
            let api = Api::new(&target.remote, target.token, &target.owner, &target.repo);
            let value = api.get_json(&api.root_path(&root)).await?;
            if json_out {
                output::print_json(&value)?;
            } else {
                print_root(&decode(&value)?);
            }
            Ok(0)
        }
        RootAction::Create {
            name,
            owner_group,
            from,
            sync,
        } => {
            validate_root_arg(&name)?;
            let from = from.as_deref().map(validate_from_arg).transpose()?;
            let target = target_for(cwd, repo_arg, remote_arg)?;
            let api = Api::new(&target.remote, target.token, &target.owner, &target.repo);
            let mut body = serde_json::Map::new();
            body.insert("name".into(), Value::from(bare_name(&name).to_string()));
            if let Some(group) = owner_group {
                body.insert("owner_group".into(), Value::from(group));
            }
            if sync {
                body.insert("write_mode".into(), Value::from("sync"));
            }
            if let Some(from) = from {
                body.insert("base_commit".into(), Value::from(from));
            }
            let value = api
                .json(Method::POST, &api.base, Some(&Value::Object(body)))
                .await
                .map_err(Failure::into_oak)?;
            if json_out {
                output::print_json(&value)?;
            } else {
                let created: CreateWire = decode(&value)?;
                let name = display_name(&created.root.name);
                output::success(&format!("Created {name}"));
                print_root(&created.root);
                output::info(&format!(
                    "`oak root pull {name}` brings its files into roots/{}/",
                    bare_name(&name)
                ));
            }
            Ok(0)
        }
        RootAction::Archive { root } => {
            validate_root_arg(&root)?;
            let target = target_for(cwd, repo_arg, remote_arg)?;
            let api = Api::new(&target.remote, target.token, &target.owner, &target.repo);
            let value = api
                .json(Method::DELETE, &api.root_path(&root), None)
                .await
                .map_err(Failure::into_oak)?;
            if json_out {
                output::print_json(&value)?;
            } else {
                output::success(&format!(
                    "Archived {} (its history stays readable; it no longer takes pushes)",
                    display_name(&root)
                ));
            }
            Ok(0)
        }
    }
}

/// The repo for list/show/create/archive: `--repo`, else the root directory
/// we're standing in, else the checkout.
fn target_for(cwd: &Path, repo_arg: Option<&str>, remote_arg: Option<&str>) -> Result<Target> {
    if repo_arg.is_none() {
        if let Some(state) = find_root_dir(cwd).and_then(|d| RootState::read(&d).ok().flatten()) {
            let remote = remote_arg
                .map(|r| r.trim_end_matches('/').to_string())
                .unwrap_or(state.remote);
            let token = effective_token_for_worktree(&remote, cwd);
            return Ok(Target {
                remote,
                owner: state.owner,
                repo: state.repo,
                token,
            });
        }
    }
    checkout_target(cwd, repo_arg, remote_arg)
}

// ---------------------------------------------------------------------------
// Pull

/// What happens to one path in a pull.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Write(Hash, FileMode),
    Delete,
}

/// The three-way plan: steps to apply, local edits kept, and conflicts.
#[derive(Debug, Default)]
struct Plan {
    steps: BTreeMap<String, Step>,
    kept: Vec<String>,
    conflicts: Vec<String>,
}

fn plan_pull(old: &Flat, local: &Flat, new: &Flat, force: bool) -> Plan {
    let mut plan = Plan::default();
    let paths: std::collections::BTreeSet<&String> =
        old.keys().chain(local.keys()).chain(new.keys()).collect();
    for path in paths {
        let (o, l, n) = (old.get(path), local.get(path), new.get(path));
        if l == n {
            continue; // already what the root has
        }
        let step = match n {
            Some((hash, mode)) => Step::Write(hash.clone(), *mode),
            None => Step::Delete,
        };
        if l == o {
            plan.steps.insert(path.clone(), step); // untouched here: take theirs
        } else if n == o {
            plan.kept.push(path.clone()); // changed only here: keep it
        } else if force {
            plan.conflicts.push(path.clone());
            plan.steps.insert(path.clone(), step);
        } else {
            plan.conflicts.push(path.clone());
        }
    }
    plan
}

/// The root's head, its tree, and the trees that tree reaches beyond what
/// `since`'s tree reaches. One commit is enough: the server returns every
/// tree the returned commits reach minus `since`'s closure, so with the head
/// alone that is exactly what we're missing.
async fn fetch_head(
    loc: &Located,
    since: Option<&str>,
) -> std::result::Result<(Option<String>, Option<String>, Vec<TreeData>), Failure> {
    let mut path = format!("{}/pull?limit=1", loc.api.root_path(&loc.root));
    if let Some(since) = since {
        path.push_str(&format!("&since={since}"));
    }
    let value = loc.api.json(reqwest::Method::GET, &path, None).await?;
    let page: PullWire = decode(&value)?;
    let manifest = match (&page.head, page.commits.last()) {
        (Some(head), Some(commit)) if &commit.hash == head => Some(commit.manifest_hash.clone()),
        (Some(head), _) if Some(head.as_str()) == since => None,
        (Some(head), _) => {
            return Err(Failure::Oak(OakError::Server(format!(
                "the server didn't return the root's head commit {head}"
            ))))
        }
        (None, _) => None,
    };
    Ok((page.head, manifest, page.trees))
}

async fn pull(cwd: &Path, loc: Located, force: bool, json_out: bool) -> Result<i32> {
    let dir = loc.dir.clone();
    let mut known = match &loc.state {
        Some(state) => state.tree_map()?,
        None => HashMap::new(),
    };
    let old_head = loc.state.as_ref().and_then(|s| s.head.clone());
    let old_manifest = loc.state.as_ref().and_then(|s| s.manifest.clone());

    let (head, fetched_manifest, trees) = match fetch_head(&loc, old_head.as_deref()).await {
        Ok(found) => found,
        // Our head isn't in the root's history any more: start over.
        Err(Failure::Refused(r)) if r.code == "invalid_since" => {
            fetch_head(&loc, None).await.map_err(Failure::into_oak)?
        }
        Err(e) => return Err(e.into_oak()),
    };
    for td in &trees {
        let tree = oak_core::protocol::tree_data_to_core(td)
            .map_err(|e| OakError::Server(format!("the server sent a bad tree: {e}")))?;
        known.insert(tree.hash.clone(), tree);
    }
    let new_manifest = if head == old_head {
        old_manifest.clone()
    } else {
        fetched_manifest
    };

    let old = flatten(old_manifest.as_deref(), &known)?;
    let new = flatten(new_manifest.as_deref(), &known)?;
    let local = scan_dir(&dir)?;
    let plan = plan_pull(&old, &local, &new, force);
    if !plan.conflicts.is_empty() && !force {
        return refuse(
            json_out,
            oak_core::RootRefusal::Conflict,
            &format!(
                "{} file(s) changed both here and on {}; nothing was changed. \
                 Move your copies aside, or take the root's with --force",
                plan.conflicts.len(),
                loc.root
            ),
            &plan.conflicts,
            &[command_for("pull", &dir, cwd, " --force")],
        );
    }

    // Keep the directory out of the checkout before writing anything into it.
    std::fs::create_dir_all(&dir)?;
    let registered = register_root_dir(&dir)?;

    let mut written = Vec::new();
    let mut deleted = Vec::new();
    for (path, step) in &plan.steps {
        oak_core::tree::validate_tree_path(path)?;
        let file = dir.join(path);
        match step {
            Step::Write(hash, mode) => {
                let content = loc.api.blob(&loc.root, hash.as_str()).await?;
                crate::materialize::create_parent_dirs(&dir, path)?;
                if let Ok(meta) = std::fs::symlink_metadata(&file) {
                    if meta.is_dir() {
                        std::fs::remove_dir_all(&file)?;
                    }
                }
                crate::materialize::materialize_path(&file, *mode, &content)?;
                written.push(path.clone());
            }
            Step::Delete => {
                match std::fs::remove_file(&file) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                prune_empty_parents(&dir, &file);
                deleted.push(path.clone());
            }
        }
    }

    let state = RootState {
        schema_version: 1,
        remote: loc.target.remote.clone(),
        owner: loc.target.owner.clone(),
        repo: loc.target.repo.clone(),
        root: loc.root.clone(),
        head: head.clone(),
        trees: match &new_manifest {
            Some(m) => closure(m, &known)?,
            None => Vec::new(),
        },
        manifest: new_manifest,
    };
    state.write(&dir)?;

    if json_out {
        output::print_json(&json!({
            "schema_version": 1,
            "operation": "root_pull",
            "root": loc.root,
            "dir": dir.display().to_string(),
            "ignored_by_checkout": registered,
            "previous_head": old_head,
            "head": head,
            "written": written,
            "deleted": deleted,
            "kept_local": plan.kept,
            "overwritten_conflicts": plan.conflicts,
        }))?;
    } else {
        let where_ = dir
            .strip_prefix(cwd)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| dir.display().to_string());
        if head == old_head && written.is_empty() && deleted.is_empty() {
            output::success(&format!("{} is up to date in {where_}/", loc.root));
        } else {
            output::success(&format!(
                "Pulled {} into {where_}/: {} written, {} deleted",
                loc.root,
                written.len(),
                deleted.len()
            ));
        }
        if !plan.kept.is_empty() {
            output::info(&format!(
                "{} local change(s) kept; `oak root push` sends them",
                plan.kept.len()
            ));
        }
        if !plan.conflicts.is_empty() {
            output::warning(&format!(
                "{} local change(s) overwritten by --force",
                plan.conflicts.len()
            ));
        }
    }
    Ok(0)
}

fn prune_empty_parents(top: &Path, file: &Path) {
    let mut cur = file.parent();
    while let Some(dir) = cur {
        if dir == top || !dir.starts_with(top) {
            break;
        }
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

// ---------------------------------------------------------------------------
// Push

async fn push(cwd: &Path, loc: Located, json_out: bool) -> Result<i32> {
    let dir = loc.dir.clone();
    if !dir.is_dir() {
        return Err(OakError::InvalidArgument(format!(
            "{} doesn't exist; run `oak root pull` first, or create it",
            dir.display()
        )));
    }
    // Without local state, the root must still be empty: otherwise we'd
    // build on a head we never saw.
    if loc.state.is_none() {
        let head = match &loc.wire {
            Some(wire) => wire.head.clone(),
            None => loc.api.show(&loc.root).await?.head,
        };
        if head.is_some() {
            return refuse(
                json_out,
                oak_core::RootRefusal::PullRequired,
                &format!(
                    "{} already has commits; pull it into {} first, then push",
                    loc.root,
                    dir.display()
                ),
                &[],
                &[command_for("pull", &dir, cwd, "")],
            );
        }
    }
    let known = match &loc.state {
        Some(state) => state.tree_map()?,
        None => HashMap::new(),
    };
    let old_head = loc.state.as_ref().and_then(|s| s.head.clone());
    let old_manifest = loc.state.as_ref().and_then(|s| s.manifest.clone());
    let old = flatten(old_manifest.as_deref(), &known)?;
    let local = scan_dir(&dir)?;
    let registered = register_root_dir(&dir)?;

    if local == old && old_head.is_some() {
        if json_out {
            output::print_json(&json!({
                "schema_version": 1,
                "operation": "root_push",
                "root": loc.root,
                "dir": dir.display().to_string(),
                "ignored_by_checkout": registered,
                "pushed": false,
                "head": old_head,
            }))?;
        } else {
            output::info(&format!(
                "Nothing to push: {} matches {}",
                dir.display(),
                loc.root
            ));
        }
        return Ok(0);
    }

    // Trees.
    let entries: Vec<ManifestEntry> = local
        .iter()
        .map(|(path, (hash, mode))| ManifestEntry {
            path: path.clone(),
            blob_hash: hash.clone(),
            mode: *mode,
        })
        .collect();
    let built = oak_core::tree::build_tree(&entries)?;
    let mut all_trees: HashMap<Hash, Tree> = HashMap::new();
    for tree in built.trees {
        all_trees.insert(tree.hash.clone(), tree);
    }
    let send_trees: Vec<TreeData> = all_trees
        .values()
        .filter(|t| !known.contains_key(&t.hash))
        .map(oak_core::protocol::tree_to_wire)
        .collect();

    // Blobs the root's head doesn't already have.
    let have: HashSet<&Hash> = old.values().map(|(h, _)| h).collect();
    let empty = Blob::new(Vec::new()).hash;
    let mut blobs: Vec<BlobData> = Vec::new();
    let mut sent: HashSet<Hash> = HashSet::new();
    let mut total = 0usize;
    for (path, (hash, _)) in &local {
        if have.contains(hash) || *hash == empty || !sent.insert(hash.clone()) {
            continue;
        }
        let file = dir.join(path);
        let meta = std::fs::symlink_metadata(&file)?;
        let (content, _) = read_entry(&file, &meta)?;
        if content.len() > MAX_ROOT_BLOB_BYTES {
            return Err(OakError::InvalidArgument(format!(
                "{path} is {} bytes; files in a root can be at most {MAX_ROOT_BLOB_BYTES} bytes for now",
                content.len()
            )));
        }
        let blob = Blob::new(content);
        if blob.hash != *hash {
            return Err(OakError::InvalidArgument(format!(
                "{path} changed while pushing; push again"
            )));
        }
        total += blob.content.len();
        if total > MAX_ROOT_PUSH_BYTES {
            return Err(OakError::InvalidArgument(format!(
                "this push would carry more than {MAX_ROOT_PUSH_BYTES} bytes of new content; \
                 push fewer new files at a time"
            )));
        }
        blobs.push(BlobData {
            hash: blob.hash.to_string(),
            size: blob.size,
            content: blob.content,
            chunks: Vec::new(),
            mapping_proof_token: None,
        });
    }

    // The commit, labelled with the root's name.
    let parent = old_head
        .as_deref()
        .map(Hash::from_hex)
        .transpose()
        .map_err(|e| OakError::InvalidArgument(format!("{ROOT_STATE_FILE} has a bad head: {e}")))?;
    let commit = Commit::new(
        loc.root.clone(),
        parent,
        None,
        built.root_hash.clone(),
        super::commit::get_author(),
        None,
        Vec::new(),
    )?;
    let commit_data = CommitData {
        hash: commit.hash.to_string(),
        branch_name: commit.branch_name.clone(),
        parent_hash: commit.parent_hash.as_ref().map(|h| h.to_string()),
        merge_parent_hash: None,
        manifest_hash: commit.manifest_hash.to_string(),
        author: commit.author.clone(),
        message: None,
        timestamp: commit.timestamp.to_rfc3339(),
        files: Vec::new(),
    };
    let body = json!({
        "expected_head": old_head,
        "commits": [commit_data],
        "trees": send_trees,
        "blobs": blobs,
    });
    let path = format!("{}/push", loc.api.root_path(&loc.root));
    let value = match loc
        .api
        .json(reqwest::Method::POST, &path, Some(&body))
        .await
    {
        Ok(v) => v,
        Err(Failure::Refused(r)) if r.code == "head_moved" => {
            return refuse(
                json_out,
                oak_core::RootRefusal::HeadMoved,
                &format!(
                    "{} moved since you last pulled; pull (your changes are kept), then push again",
                    loc.root
                ),
                &[],
                &[
                    command_for("pull", &dir, cwd, ""),
                    command_for("push", &dir, cwd, ""),
                ],
            )
        }
        Err(e) => return Err(e.into_oak()),
    };
    let pushed: PushWire = decode(&value)?;

    let manifest = built.root_hash.to_string();
    let state = RootState {
        schema_version: 1,
        remote: loc.target.remote.clone(),
        owner: loc.target.owner.clone(),
        repo: loc.target.repo.clone(),
        root: loc.root.clone(),
        head: Some(pushed.head.clone()),
        trees: closure(&manifest, &all_trees)?,
        manifest: Some(manifest),
    };
    state.write(&dir)?;

    let changed = local.iter().filter(|(p, v)| old.get(*p) != Some(v)).count()
        + old.keys().filter(|p| !local.contains_key(*p)).count();
    if json_out {
        output::print_json(&json!({
            "schema_version": 1,
            "operation": "root_push",
            "root": loc.root,
            "dir": dir.display().to_string(),
            "ignored_by_checkout": registered,
            "pushed": true,
            "previous_head": old_head,
            "head": pushed.head,
            "changed_files": changed,
            "blobs_sent": blobs.len(),
            "bytes_sent": total,
        }))?;
    } else {
        output::success(&format!(
            "Pushed {} ({changed} file(s) changed, {} new blob(s)) → {}",
            loc.root,
            blobs.len(),
            &pushed.head[..pushed.head.len().min(12)]
        ));
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Human output

/// What stands in for a head the caller doesn't get to see.
fn head_label(root: &RootWire) -> &str {
    match root.head.as_deref() {
        Some(h) => &h[..h.len().min(12)],
        None if root.access == "metadata" => "(can't read)",
        None => "(empty)",
    }
}

fn owner_label(root: &RootWire) -> String {
    match &root.owner_group {
        Some(slug) => format!("@group/{slug}"),
        None => "-".to_string(),
    }
}

fn print_list(list: &ListWire) {
    if list.roots.is_empty() {
        output::info(
            "No roots you can see. `oak root create NAME` makes one, \
             `--from main` to start it at main's head.",
        );
        return;
    }
    for root in &list.roots {
        output::print_line(&format!(
            "{:<24} {:<20} {:<6} {:<8} {}{}{}",
            display_name(&root.name),
            owner_label(root),
            root.write_mode,
            root.access,
            head_label(root),
            match root.base_commit.as_deref() {
                Some(base) => format!("  from main {}", &base[..base.len().min(12)]),
                None => String::new(),
            },
            if root.archived_at.is_some() {
                "  [archived]"
            } else {
                ""
            }
        ));
    }
}

fn print_root(root: &RootWire) {
    output::print_line(&display_name(&root.name));
    output::print_line(&format!(
        "  owner:      {}",
        match &root.owner_group {
            Some(slug) => format!("@group/{slug}"),
            None => "none (the org's owners and admins write it)".to_string(),
        }
    ));
    output::print_line(&format!("  write mode: {}", root.write_mode));
    output::print_line(&format!(
        "  started:    {}",
        match &root.base_commit {
            Some(base) => format!("from main at {base}"),
            None => "empty".to_string(),
        }
    ));
    output::print_line(&format!("  access:     {}", root.access));
    output::print_line(&format!("  head:       {}", head_label(root)));
    if root.access == "metadata" {
        output::print_line(
            "  it holds main's whole tree, so reading it takes being able to read every path of the repo",
        );
    }
    if let Some(at) = &root.archived_at {
        output::print_line(&format!("  archived:   {at}"));
    }
    if root.can_archive && root.archived_at.is_none() {
        output::print_line("  you can archive it (`oak root archive`)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> Hash {
        Hash::from_hex(&format!("{n:064x}")).unwrap()
    }

    fn flat(items: &[(&str, u8)]) -> Flat {
        items
            .iter()
            .map(|(p, n)| (p.to_string(), (h(*n), FileMode::Regular)))
            .collect()
    }

    #[test]
    fn three_way_pull_plan() {
        let old = flat(&[("a", 1), ("b", 2), ("c", 3), ("d", 4)]);
        // a: untouched here, changed there → take theirs
        // b: changed here only → keep
        // c: changed on both sides → conflict
        // d: deleted there, untouched here → delete
        // e: new there → write
        let local = flat(&[("a", 1), ("b", 9), ("c", 8), ("d", 4)]);
        let new = flat(&[("a", 5), ("b", 2), ("c", 7), ("e", 6)]);
        let plan = plan_pull(&old, &local, &new, false);
        assert_eq!(plan.conflicts, vec!["c".to_string()]);
        assert_eq!(plan.kept, vec!["b".to_string()]);
        assert_eq!(
            plan.steps.get("a"),
            Some(&Step::Write(h(5), FileMode::Regular))
        );
        assert_eq!(plan.steps.get("d"), Some(&Step::Delete));
        assert_eq!(
            plan.steps.get("e"),
            Some(&Step::Write(h(6), FileMode::Regular))
        );
        assert!(!plan.steps.contains_key("c"));

        let forced = plan_pull(&old, &local, &new, true);
        assert_eq!(
            forced.steps.get("c"),
            Some(&Step::Write(h(7), FileMode::Regular))
        );
    }

    #[test]
    fn first_pull_into_a_populated_dir_only_conflicts_on_different_content() {
        let old = Flat::new();
        let local = flat(&[("same", 1), ("mine", 2), ("clash", 3)]);
        let new = flat(&[("same", 1), ("clash", 4), ("theirs", 5)]);
        let plan = plan_pull(&old, &local, &new, false);
        assert_eq!(plan.conflicts, vec!["clash".to_string()]);
        assert_eq!(plan.kept, vec!["mine".to_string()]);
        assert_eq!(plan.steps.len(), 1);
    }

    #[test]
    fn from_takes_main_or_a_full_hash() {
        assert_eq!(validate_from_arg("main").unwrap(), "main");
        let full = "AB".repeat(32);
        assert_eq!(validate_from_arg(&full).unwrap(), "ab".repeat(32));
        assert!(validate_from_arg("abc123").is_err());
        assert!(validate_from_arg("stable").is_err());
        assert!(validate_from_arg(&"g".repeat(64)).is_err());
    }

    #[test]
    fn names() {
        assert_eq!(display_name("zach"), "~zach");
        assert_eq!(display_name("~zach"), "~zach");
        assert!(validate_root_arg("~ops").is_ok());
        assert!(validate_root_arg("stable-1.x").is_ok());
        assert!(validate_root_arg("../x").is_err());
        assert!(validate_root_arg("~").is_err());
    }
}
