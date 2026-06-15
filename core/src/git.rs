//! `Repository` implementation backed by a real `.git` directory.
//!
//! Phase 1 is read-only: callers can `oak log` / `status` / `diff` / `switch`
//! against an existing git repo with no `.oak/` present. Write methods return
//! [`OakError::Unsupported`] — Phase 2 fills those in.
//!
//! Hash semantics on this backend:
//! - `Blob.hash` is the git blob OID.
//! - `Manifest.hash` is the git tree OID.
//! - `Commit.hash` is the git commit OID.
//!
//! That's a deliberate departure from the SQLite backend (which uses BLAKE3
//! everywhere). Storing native git OIDs lets Oak commits round-trip through
//! `git log` and other git tools without translation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::{
    Blob, Branch, BranchStatus, Capabilities, ChunkInfo, CloseReason, Commit, FileChange, FileMode,
    Hash, Manifest, ManifestEntry, MetadataKey, OakError, Result, Tag,
};
use gix::bstr::BString;
use gix::ObjectId;

use crate::traits::Repository;

const BACKEND: &str = "git";

/// Read-only `Repository` over an existing git directory.
pub struct GitRepository {
    /// Thread-safe handle; we re-derive a thread-local `gix::Repository` per call.
    inner: gix::ThreadSafeRepository,
    /// Sidecar directory for Oak-only state (workdir lock, branch metadata, etc.).
    /// Lives at `<git_dir>/oak/` so it's hidden from normal git operations.
    oak_sidecar_dir: PathBuf,
    /// In-memory metadata fallback. Phase 1 doesn't persist these (writes are unsupported)
    /// but `set_metadata` for `Head` / `CurrentBranch` is harmless to accept silently
    /// because subsequent reads come from the live git refs anyway.
    scratch_metadata: Mutex<std::collections::HashMap<String, String>>,
}

impl GitRepository {
    /// Open an existing git repository at `git_dir` (the `.git` folder, or a bare repo).
    /// `oak_sidecar_dir` is the directory where Oak-only state files live (created on demand).
    pub fn open(git_dir: &Path, oak_sidecar_dir: &Path) -> Result<Self> {
        let inner = gix::ThreadSafeRepository::open(git_dir).map_err(map_git_err)?;
        Ok(Self {
            inner,
            oak_sidecar_dir: oak_sidecar_dir.to_path_buf(),
            scratch_metadata: Mutex::new(Default::default()),
        })
    }

    fn repo(&self) -> gix::Repository {
        self.inner.to_thread_local()
    }

    fn oid(hash: &Hash) -> Result<ObjectId> {
        ObjectId::from_hex(hash.as_str().as_bytes())
            .map_err(|_| OakError::InvalidHash(hash.as_str().to_string()))
    }

    /// Load a commit by OID and convert it to an `crate::Commit`.
    /// `files` is left empty — callers that need it should compute a tree diff lazily.
    fn load_commit(&self, oid: ObjectId) -> Result<Commit> {
        let repo = self.repo();
        let obj = repo.find_object(oid).map_err(map_git_err)?;
        let git_commit = obj
            .try_into_commit()
            .map_err(|e| OakError::Git(e.to_string()))?;

        let tree_id = git_commit.tree_id().map_err(map_git_err)?.detach();
        let parents: Vec<Hash> = git_commit
            .parent_ids()
            .map(|p| Hash(p.detach().to_string()))
            .collect();

        let author = git_commit
            .author()
            .map(|a| a.name.to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        // Git always has a message string; treat empty/whitespace-only as
        // "no message" to match the new oak model where messages are
        // exclusively for main-branch squash-merges.
        let message = git_commit
            .message_raw()
            .ok()
            .map(|m| m.to_string())
            .filter(|s| !s.trim().is_empty());

        let timestamp = git_commit
            .time()
            .ok()
            .and_then(|t| chrono::DateTime::from_timestamp(t.seconds, 0))
            .unwrap_or_else(chrono::Utc::now);

        // Git commits don't carry a branch name — the caller's context (which ref they
        // walked) is the only signal. Leaving empty here; the `get_commits_for_branch`
        // path stamps the branch name on commits it returns.
        Ok(Commit {
            hash: Hash(oid.to_string()),
            branch_name: String::new(),
            parent_hash: parents.first().cloned(),
            merge_parent_hash: parents.get(1).cloned(),
            manifest_hash: Hash(tree_id.to_string()),
            author,
            message,
            timestamp,
            files: Vec::<FileChange>::new(),
        })
    }
}

fn map_git_err<E: std::fmt::Display>(e: E) -> OakError {
    OakError::Git(e.to_string())
}

fn unsupported(op: &'static str) -> OakError {
    OakError::Unsupported {
        backend: BACKEND,
        op,
    }
}

// --- Branch metadata sidecar ------------------------------------------------
//
// Git refs only carry a name and a commit OID. Oak's `Branch` struct also has a
// description, parent_branch, status (open/closed), and created_at. Rather than
// silently drop those on the git backend, we persist them in a TOML file at
// `<git_dir>/oak/branches.toml` — outside the git object/ref store, but inside
// `.git/` so it lives with the repo. The sidecar is read lazily and written on
// any branch metadata change.

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct BranchSidecar {
    /// Map of branch name → metadata. Branches without a sidecar entry default
    /// to `(description=None, parent_branch=None, status=Open, created_at=now)`.
    #[serde(default)]
    branches: std::collections::BTreeMap<String, BranchMeta>,
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct BranchMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_branch: Option<String>,
    /// "open" or "closed" — serialized as a string for forward-compat.
    #[serde(default = "default_status")]
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    close_reason: Option<String>,
    /// RFC3339 timestamp; omitted means "unknown" and we fall back to now-ish on read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
}

fn default_status() -> String {
    "open".to_string()
}

impl BranchSidecar {
    fn path(sidecar_dir: &Path) -> PathBuf {
        sidecar_dir.join("branches.toml")
    }

    fn load(sidecar_dir: &Path) -> Result<Self> {
        let path = Self::path(sidecar_dir);
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)?;
        toml::from_str(&raw).map_err(|e| OakError::Config(format!("branches.toml: {e}")))
    }

    fn save(&self, sidecar_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(sidecar_dir)?;
        let raw = toml::to_string_pretty(self)
            .map_err(|e| OakError::Config(format!("serialize branches.toml: {e}")))?;
        std::fs::write(Self::path(sidecar_dir), raw)?;
        Ok(())
    }
}

fn meta_to_branch(name: &str, meta: &BranchMeta) -> Branch {
    let status = BranchStatus::from_db_str(&meta.status);
    let close_reason = meta
        .close_reason
        .as_deref()
        .map(CloseReason::parse)
        .transpose()
        .unwrap_or(None);
    let created_at = meta
        .created_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now);
    Branch {
        name: name.to_string(),
        description: meta.description.clone(),
        parent_branch: meta.parent_branch.clone(),
        status,
        close_reason,
        created_at,
    }
}

fn branch_to_meta(branch: &Branch) -> BranchMeta {
    BranchMeta {
        description: branch.description.clone(),
        parent_branch: branch.parent_branch.clone(),
        status: branch.status.as_str().to_string(),
        close_reason: branch.close_reason.map(|r| r.as_str().to_string()),
        created_at: Some(branch.created_at.to_rfc3339()),
    }
}

// --- Write-path helpers -----------------------------------------------------

/// Convert an Oak `FileMode` to the matching git tree entry mode.
fn oak_mode_to_git(mode: FileMode) -> gix::objs::tree::EntryMode {
    match mode {
        FileMode::Regular => gix::objs::tree::EntryKind::Blob.into(),
        FileMode::Executable => gix::objs::tree::EntryKind::BlobExecutable.into(),
        FileMode::Symlink => gix::objs::tree::EntryKind::Link.into(),
    }
}

/// Intermediate node used to build a nested git tree from Oak's flat manifest.
enum TreeNode {
    Blob {
        oid: ObjectId,
        mode: FileMode,
    },
    Dir {
        children: BTreeMap<String, TreeNode>,
    },
}

impl TreeNode {
    fn new_dir() -> Self {
        TreeNode::Dir {
            children: BTreeMap::new(),
        }
    }

    /// Insert a flat path (e.g. `"src/foo.rs"`) into the nested tree.
    fn insert(&mut self, path: &str, oid: ObjectId, mode: FileMode) {
        let TreeNode::Dir { children } = self else {
            return; // Shouldn't happen — root is always a Dir.
        };
        let mut parts = path.splitn(2, '/');
        let head = parts.next().unwrap_or("").to_string();
        match parts.next() {
            None => {
                children.insert(head, TreeNode::Blob { oid, mode });
            }
            Some(rest) => {
                let entry = children.entry(head).or_insert_with(TreeNode::new_dir);
                entry.insert(rest, oid, mode);
            }
        }
    }
}

/// Recursively write nested `TreeNode`s as git trees, returning the root tree OID.
fn write_tree_recursive(repo: &gix::Repository, node: &TreeNode) -> Result<ObjectId> {
    let TreeNode::Dir { children } = node else {
        return Err(OakError::Git(
            "internal error: tried to write a Blob node as a tree".into(),
        ));
    };

    let mut entries = Vec::with_capacity(children.len());
    for (name, child) in children {
        match child {
            TreeNode::Blob { oid, mode } => {
                entries.push(gix::objs::tree::Entry {
                    mode: oak_mode_to_git(*mode),
                    filename: name.as_str().into(),
                    oid: *oid,
                });
            }
            TreeNode::Dir { .. } => {
                let subtree = write_tree_recursive(repo, child)?;
                entries.push(gix::objs::tree::Entry {
                    mode: gix::objs::tree::EntryKind::Tree.into(),
                    filename: name.as_str().into(),
                    oid: subtree,
                });
            }
        }
    }

    let tree = gix::objs::Tree { entries };
    let id = repo.write_object(&tree).map_err(map_git_err)?;
    Ok(id.detach())
}

/// Parse an `author` string of the form `"Name <email>"` into separate name/email parts.
/// Falls back to a synthesized email if the input doesn't carry one.
fn split_author(author: &str) -> (String, String) {
    if let Some(start) = author.find('<') {
        if let Some(end) = author[start + 1..].find('>') {
            let name = author[..start].trim().to_string();
            let email = author[start + 1..start + 1 + end].trim().to_string();
            return (name, email);
        }
    }
    (author.to_string(), "oak@oak.space".to_string())
}

/// Walk a git tree recursively into a flat `Vec<ManifestEntry>`.
/// Submodules and other non-blob entries are skipped.
fn read_tree_recursive(
    repo: &gix::Repository,
    tree_id: ObjectId,
    prefix: &str,
    out: &mut Vec<ManifestEntry>,
) -> Result<()> {
    let obj = repo.find_object(tree_id).map_err(map_git_err)?;
    let tree = obj
        .try_into_tree()
        .map_err(|e| OakError::Git(e.to_string()))?;

    for entry in tree.iter() {
        let entry = entry.map_err(map_git_err)?;
        let name = entry.filename().to_string();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };

        let mode = entry.mode();
        if mode.is_tree() {
            read_tree_recursive(repo, entry.id().detach(), &path, out)?;
        } else if mode.is_blob() || mode.is_executable() || mode.is_link() {
            let oak_mode = if mode.is_executable() {
                FileMode::Executable
            } else if mode.is_link() {
                FileMode::Symlink
            } else {
                FileMode::Regular
            };
            out.push(ManifestEntry {
                path,
                blob_hash: Hash(entry.id().detach().to_string()),
                mode: oak_mode,
            });
        }
        // Submodules (gitlinks) and other entry kinds are intentionally dropped.
    }
    Ok(())
}

impl Repository for GitRepository {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            branch_parents: true, // stored in the sidecar
            tags: true,
            releases: false,
            chunks: false,
            branch_metadata: true, // stored in the sidecar
        }
    }

    // --- Blobs ---

    /// `store_blob` is unsupported because Oak's `Blob` carries a precomputed BLAKE3
    /// hash, and writing it as a git blob would assign a different OID. Callers must
    /// use [`Repository::put_blob`] instead, which returns the canonical git OID.
    fn store_blob(&self, _blob: &Blob) -> Result<()> {
        Err(unsupported("store_blob (use put_blob)"))
    }

    fn put_blob(&self, content: Vec<u8>) -> Result<Hash> {
        let repo = self.repo();
        let id = repo.write_blob(content).map_err(map_git_err)?;
        Ok(Hash(id.detach().to_string()))
    }

    fn get_blob(&self, hash: &Hash) -> Result<Option<Blob>> {
        let oid = Self::oid(hash)?;
        let repo = self.repo();
        let obj = repo.try_find_object(oid).map_err(map_git_err)?;
        match obj {
            Some(obj) => {
                let content = obj.data.to_vec();
                let size = content.len() as u64;
                Ok(Some(Blob {
                    hash: hash.clone(),
                    content,
                    size,
                }))
            }
            None => Ok(None),
        }
    }

    fn has_blob(&self, hash: &Hash) -> Result<bool> {
        let oid = Self::oid(hash)?;
        Ok(self
            .repo()
            .try_find_object(oid)
            .map_err(map_git_err)?
            .is_some())
    }

    // --- Manifests (git trees) ---

    /// Unsupported for the same reason as `store_blob` — Oak's manifest hash is
    /// BLAKE3 over a flat encoding, but git trees are nested with their own OID
    /// derivation. Callers must use [`Repository::put_manifest`].
    fn store_manifest(&self, _manifest: &Manifest) -> Result<()> {
        Err(unsupported("store_manifest (use put_manifest)"))
    }

    fn put_manifest(&self, entries: Vec<ManifestEntry>) -> Result<Hash> {
        let repo = self.repo();
        let mut root = TreeNode::new_dir();
        for entry in entries {
            let oid = ObjectId::from_hex(entry.blob_hash.as_str().as_bytes())
                .map_err(|_| OakError::InvalidHash(entry.blob_hash.as_str().to_string()))?;
            root.insert(&entry.path, oid, entry.mode);
        }
        let tree_oid = write_tree_recursive(&repo, &root)?;
        Ok(Hash(tree_oid.to_string()))
    }

    fn get_manifest(&self, hash: &Hash) -> Result<Option<Manifest>> {
        let oid = Self::oid(hash)?;
        let repo = self.repo();
        let obj = repo.try_find_object(oid).map_err(map_git_err)?;
        let is_tree = matches!(&obj, Some(o) if o.kind == gix::object::Kind::Tree);
        drop(obj);
        if !is_tree {
            return Ok(None);
        }
        let mut entries = Vec::new();
        read_tree_recursive(&repo, oid, "", &mut entries)?;
        Ok(Some(Manifest {
            hash: hash.clone(),
            entries,
        }))
    }

    // --- Trees (git's native tree objects) ---

    /// Read the direct children of one git tree as an Oak `Tree`.
    /// Submodules are skipped (matching `read_tree_recursive`).
    fn get_tree(&self, hash: &Hash) -> Result<Option<crate::Tree>> {
        let oid = Self::oid(hash)?;
        let repo = self.repo();
        let obj = match repo.try_find_object(oid).map_err(map_git_err)? {
            Some(o) if o.kind == gix::object::Kind::Tree => o,
            _ => return Ok(None),
        };
        let tree = obj
            .try_into_tree()
            .map_err(|e| OakError::Git(e.to_string()))?;

        let mut entries = Vec::new();
        for entry in tree.iter() {
            let entry = entry.map_err(map_git_err)?;
            let mode = entry.mode();
            if mode.is_tree() {
                entries.push(crate::TreeEntry {
                    name: entry.filename().to_string(),
                    kind: crate::TreeEntryKind::Tree,
                    hash: Hash(entry.id().detach().to_string()),
                    mode: FileMode::Regular,
                });
            } else if mode.is_blob() || mode.is_executable() || mode.is_link() {
                let oak_mode = if mode.is_executable() {
                    FileMode::Executable
                } else if mode.is_link() {
                    FileMode::Symlink
                } else {
                    FileMode::Regular
                };
                entries.push(crate::TreeEntry {
                    name: entry.filename().to_string(),
                    kind: crate::TreeEntryKind::Blob,
                    hash: Hash(entry.id().detach().to_string()),
                    mode: oak_mode,
                });
            }
        }
        Ok(Some(crate::Tree {
            hash: hash.clone(),
            entries,
        }))
    }

    /// Unsupported for the same reason as `store_blob`: the Tree's hash is Oak's
    /// BLAKE3, but a git tree object would assign its own OID. Use [`put_tree`]
    /// instead, which returns the git OID.
    fn store_tree(&self, _tree: &crate::Tree) -> Result<()> {
        Err(unsupported("store_tree (use put_tree)"))
    }

    fn put_tree(&self, entries: Vec<ManifestEntry>) -> Result<Hash> {
        // Same algorithm as `put_manifest`: build a nested TreeNode and write
        // it as git trees. Returns the root tree OID.
        let repo = self.repo();
        let mut root = TreeNode::new_dir();
        for entry in entries {
            let oid = ObjectId::from_hex(entry.blob_hash.as_str().as_bytes())
                .map_err(|_| OakError::InvalidHash(entry.blob_hash.as_str().to_string()))?;
            root.insert(&entry.path, oid, entry.mode);
        }
        let tree_oid = write_tree_recursive(&repo, &root)?;
        Ok(Hash(tree_oid.to_string()))
    }

    fn walk_tree(&self, root: &Hash) -> Result<Vec<ManifestEntry>> {
        let oid = Self::oid(root)?;
        let repo = self.repo();
        let mut entries = Vec::new();
        read_tree_recursive(&repo, oid, "", &mut entries)?;
        Ok(entries)
    }

    // --- Branches (git refs) ---

    /// Create the git ref for a new branch, pointing at the current HEAD, and
    /// record Oak's branch metadata (description / parent_branch / status / created_at)
    /// in the sidecar TOML at `<git_dir>/oak/branches.toml`.
    ///
    /// The git ref is what actually makes the branch usable; the sidecar just
    /// preserves Oak-only fields so `oak branch show` / `branch list` aren't lossy.
    fn store_branch(&self, branch: &Branch) -> Result<()> {
        let repo = self.repo();
        let refname = format!("refs/heads/{}", branch.name);

        // Create the ref if missing (don't move it if it already exists).
        if repo
            .try_find_reference(&refname)
            .map_err(map_git_err)?
            .is_none()
        {
            let head = repo.head().map_err(map_git_err)?;
            let head_oid = match head.try_into_peeled_id() {
                Ok(Some(id)) => id.detach(),
                Ok(None) => {
                    return Err(OakError::Git(format!(
                        "cannot create branch '{}': repository has no commits yet",
                        branch.name
                    )));
                }
                Err(e) => return Err(map_git_err(e)),
            };

            repo.reference(
                refname.as_str(),
                head_oid,
                gix::refs::transaction::PreviousValue::MustNotExist,
                BString::from(format!("oak: create branch {}", branch.name)),
            )
            .map_err(map_git_err)?;
        }

        // Persist Oak-only metadata in the sidecar.
        let mut sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        sidecar
            .branches
            .insert(branch.name.clone(), branch_to_meta(branch));
        sidecar.save(&self.oak_sidecar_dir)?;
        Ok(())
    }

    fn get_branch(&self, name: &str) -> Result<Option<Branch>> {
        let repo = self.repo();
        let refname = format!("refs/heads/{name}");
        if repo
            .try_find_reference(&refname)
            .map_err(map_git_err)?
            .is_none()
        {
            return Ok(None);
        }
        let sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        let branch = match sidecar.branches.get(name) {
            Some(meta) => meta_to_branch(name, meta),
            None => Branch {
                name: name.to_string(),
                description: None,
                parent_branch: None,
                status: BranchStatus::Open,
                close_reason: None,
                created_at: chrono::Utc::now(),
            },
        };
        Ok(Some(branch))
    }

    fn list_branches(&self) -> Result<Vec<Branch>> {
        let repo = self.repo();
        let platform = repo.references().map_err(map_git_err)?;
        let iter = platform.local_branches().map_err(map_git_err)?;
        let sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        let mut out = Vec::new();
        for r in iter {
            let r = r.map_err(map_git_err)?;
            let full = r.name().as_bstr().to_string();
            let name = full
                .strip_prefix("refs/heads/")
                .unwrap_or(&full)
                .to_string();
            let branch = match sidecar.branches.get(&name) {
                Some(meta) => meta_to_branch(&name, meta),
                None => Branch {
                    name,
                    description: None,
                    parent_branch: None,
                    status: BranchStatus::Open,
                    close_reason: None,
                    created_at: chrono::Utc::now(),
                },
            };
            out.push(branch);
        }
        Ok(out)
    }

    fn update_branch_status(&self, name: &str, status: BranchStatus) -> Result<()> {
        let mut sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        let entry = sidecar.branches.entry(name.to_string()).or_default();
        entry.status = status.as_str().to_string();
        sidecar.save(&self.oak_sidecar_dir)?;
        Ok(())
    }

    fn close_branch(&self, name: &str, close_reason: Option<CloseReason>) -> Result<()> {
        let mut sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        let entry = sidecar.branches.entry(name.to_string()).or_default();
        entry.status = BranchStatus::Closed.as_str().to_string();
        entry.close_reason = close_reason.map(|r| r.as_str().to_string());
        sidecar.save(&self.oak_sidecar_dir)?;
        Ok(())
    }

    fn update_branch_close_reason(&self, name: &str, close_reason: CloseReason) -> Result<()> {
        let mut sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        let entry = sidecar.branches.entry(name.to_string()).or_default();
        entry.close_reason = Some(close_reason.as_str().to_string());
        sidecar.save(&self.oak_sidecar_dir)?;
        Ok(())
    }

    fn update_branch_description(&self, name: &str, description: &str) -> Result<()> {
        let mut sidecar = BranchSidecar::load(&self.oak_sidecar_dir)?;
        let entry = sidecar.branches.entry(name.to_string()).or_default();
        entry.description = if description.is_empty() {
            None
        } else {
            Some(description.to_string())
        };
        sidecar.save(&self.oak_sidecar_dir)?;
        Ok(())
    }

    fn get_branch_head(&self, name: &str) -> Result<Option<Hash>> {
        let repo = self.repo();
        let refname = format!("refs/heads/{name}");
        let r = repo.try_find_reference(&refname).map_err(map_git_err)?;
        match r {
            Some(mut r) => {
                let id = r.peel_to_id().map_err(map_git_err)?;
                Ok(Some(Hash(id.detach().to_string())))
            }
            None => Ok(None),
        }
    }

    fn set_branch_head(&self, name: &str, hash: &Hash) -> Result<()> {
        let repo = self.repo();
        let oid = ObjectId::from_hex(hash.as_str().as_bytes())
            .map_err(|_| OakError::InvalidHash(hash.as_str().to_string()))?;
        let refname = format!("refs/heads/{name}");
        repo.reference(
            refname.as_str(),
            oid,
            gix::refs::transaction::PreviousValue::Any,
            BString::from(format!("oak: set head of {name}")),
        )
        .map_err(map_git_err)?;
        Ok(())
    }

    // --- Commits ---

    fn store_commit(&self, _commit: &Commit) -> Result<()> {
        Err(unsupported("store_commit (use put_commit)"))
    }

    fn put_commit(
        &self,
        _branch_name: String,
        parent_hash: Option<Hash>,
        merge_parent_hash: Option<Hash>,
        manifest_hash: Hash,
        author: String,
        message: Option<String>,
        timestamp: chrono::DateTime<chrono::Utc>,
        _files: Vec<FileChange>,
    ) -> Result<Hash> {
        // Git commits require a non-empty message string. For oak commits with
        // no message (the new default), fall back to a single space so gix
        // accepts it. Real human-written messages only show up on squash-merge
        // commits to main, which the server creates — not this code path.
        let message = message.unwrap_or_else(|| " ".to_string());
        let repo = self.repo();
        let tree = ObjectId::from_hex(manifest_hash.as_str().as_bytes())
            .map_err(|_| OakError::InvalidHash(manifest_hash.as_str().to_string()))?;

        let mut parents: Vec<ObjectId> = Vec::new();
        if let Some(p) = parent_hash {
            parents.push(
                ObjectId::from_hex(p.as_str().as_bytes())
                    .map_err(|_| OakError::InvalidHash(p.as_str().to_string()))?,
            );
        }
        if let Some(p) = merge_parent_hash {
            parents.push(
                ObjectId::from_hex(p.as_str().as_bytes())
                    .map_err(|_| OakError::InvalidHash(p.as_str().to_string()))?,
            );
        }

        let (name, email) = split_author(&author);
        let sig = gix::actor::Signature {
            name: name.into(),
            email: email.into(),
            time: gix::date::Time::new(timestamp.timestamp(), 0),
        };

        let commit = gix::objs::Commit {
            tree,
            parents: parents.into(),
            author: sig.clone(),
            committer: sig,
            encoding: None,
            message: message.into(),
            extra_headers: vec![],
        };

        let id = repo.write_object(&commit).map_err(map_git_err)?;
        Ok(Hash(id.detach().to_string()))
    }

    fn get_commit(&self, hash: &Hash) -> Result<Option<Commit>> {
        let oid = Self::oid(hash)?;
        let repo = self.repo();
        let obj = repo.try_find_object(oid).map_err(map_git_err)?;
        let is_commit = matches!(&obj, Some(o) if o.kind == gix::object::Kind::Commit);
        drop(obj);
        drop(repo);
        if !is_commit {
            return Ok(None);
        }
        Ok(Some(self.load_commit(oid)?))
    }

    fn has_commit(&self, hash: &Hash) -> Result<bool> {
        let oid = Self::oid(hash)?;
        Ok(self
            .repo()
            .try_find_object(oid)
            .map_err(map_git_err)?
            .is_some())
    }

    fn get_commits_for_branch(&self, branch_name: &str) -> Result<Vec<Commit>> {
        let head = match self.get_branch_head(branch_name)? {
            Some(h) => h,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        let mut current = Some(Self::oid(&head)?);
        while let Some(oid) = current {
            let mut commit = self.load_commit(oid)?;
            // Branch name isn't stored on git commits — stamp it from the caller's context.
            commit.branch_name = branch_name.to_string();
            current = commit.parent_hash.as_ref().and_then(|p| Self::oid(p).ok());
            out.push(commit);
        }
        Ok(out)
    }

    fn get_commits_since(&self, branch_name: &str, since: Option<&Hash>) -> Result<Vec<Commit>> {
        let all = self.get_commits_for_branch(branch_name)?;
        match since {
            None => Ok(all),
            Some(stop) => Ok(all.into_iter().take_while(|c| &c.hash != stop).collect()),
        }
    }

    fn get_all_commits(&self) -> Result<Vec<Commit>> {
        // Walk every local branch tip; dedupe by hash. For huge repos this is expensive
        // but matches the SQLite backend's semantics ("everything reachable").
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for branch in self.list_branches()? {
            for commit in self.get_commits_for_branch(&branch.name)? {
                if seen.insert(commit.hash.clone()) {
                    out.push(commit);
                }
            }
        }
        Ok(out)
    }

    // --- Metadata ---

    fn get_metadata(&self, key: MetadataKey) -> Result<Option<String>> {
        match key {
            MetadataKey::Head => {
                // Resolve HEAD to a commit OID (works for both attached and detached HEAD).
                let repo = self.repo();
                match repo.head().map_err(map_git_err)?.try_into_peeled_id() {
                    Ok(Some(id)) => Ok(Some(id.detach().to_string())),
                    Ok(None) => Ok(None),
                    Err(e) => Err(map_git_err(e)),
                }
            }
            MetadataKey::CurrentBranch => {
                // Symbolic HEAD → return the branch short name.
                let repo = self.repo();
                let head = repo.head().map_err(map_git_err)?;
                Ok(head.referent_name().map(|n| {
                    let full = n.as_bstr().to_string();
                    full.strip_prefix("refs/heads/")
                        .unwrap_or(&full)
                        .to_string()
                }))
            }
            _ => Ok(self
                .scratch_metadata
                .lock()
                .unwrap()
                .get(key.as_str())
                .cloned()),
        }
    }

    fn set_metadata(&self, key: MetadataKey, value: &str) -> Result<()> {
        match key {
            MetadataKey::Head => {
                // Oak's `set_head` is called after every commit to record the new tip.
                // For attached HEAD, gix updates the underlying branch ref automatically
                // when `set_branch_head` is called, so this is usually redundant. We only
                // need to act when HEAD is detached — write the OID directly.
                let repo = self.repo();
                let oid = ObjectId::from_hex(value.as_bytes())
                    .map_err(|_| OakError::InvalidHash(value.to_string()))?;
                let head = repo.head().map_err(map_git_err)?;
                if head.referent_name().is_none() {
                    // Detached HEAD — point HEAD directly at the OID.
                    repo.reference(
                        "HEAD",
                        oid,
                        gix::refs::transaction::PreviousValue::Any,
                        BString::from("oak: set detached HEAD"),
                    )
                    .map_err(map_git_err)?;
                }
                Ok(())
            }
            MetadataKey::CurrentBranch => {
                // Update HEAD to be a symbolic ref pointing at refs/heads/<value>.
                // gix doesn't expose a one-line "make HEAD symbolic" — open the ref
                // file directly via the loose store.
                let repo = self.repo();
                let target = format!("refs/heads/{value}");
                let target_name: gix::refs::FullName =
                    target.as_str().try_into().map_err(map_git_err)?;
                let edit = gix::refs::transaction::RefEdit {
                    change: gix::refs::transaction::Change::Update {
                        log: gix::refs::transaction::LogChange {
                            mode: gix::refs::transaction::RefLog::AndReference,
                            force_create_reflog: false,
                            message: BString::from(format!("oak: switch HEAD to {value}")),
                        },
                        expected: gix::refs::transaction::PreviousValue::Any,
                        new: gix::refs::Target::Symbolic(target_name),
                    },
                    name: "HEAD".try_into().map_err(map_git_err)?,
                    deref: false,
                };
                repo.edit_reference(edit).map_err(map_git_err)?;
                Ok(())
            }
            _ => {
                self.scratch_metadata
                    .lock()
                    .unwrap()
                    .insert(key.as_str().to_string(), value.to_string());
                Ok(())
            }
        }
    }

    // --- Tags ---

    fn create_tag(&self, _name: &str, _commit_hash: &Hash) -> Result<()> {
        Err(unsupported("create_tag"))
    }

    fn list_tags(&self) -> Result<Vec<Tag>> {
        let repo = self.repo();
        let platform = repo.references().map_err(map_git_err)?;
        let iter = platform.tags().map_err(map_git_err)?;
        let mut out = Vec::new();
        for r in iter {
            let mut r = r.map_err(map_git_err)?;
            let full = r.name().as_bstr().to_string();
            let name = full.strip_prefix("refs/tags/").unwrap_or(&full).to_string();
            // Annotated tags peel through a tag object; lightweight tags point straight at a commit.
            let commit_id = r.peel_to_id().map_err(map_git_err)?.detach();
            out.push(Tag {
                name,
                commit_hash: Hash(commit_id.to_string()),
                created_at: chrono::Utc::now(),
            });
        }
        Ok(out)
    }

    fn delete_tag(&self, _name: &str) -> Result<()> {
        Err(unsupported("delete_tag"))
    }

    // --- Chunks ---

    fn store_chunk(&self, _hash: &Hash, _content: &[u8]) -> Result<()> {
        Err(unsupported("store_chunk"))
    }
    fn get_chunk(&self, _hash: &Hash) -> Result<Option<Vec<u8>>> {
        Err(unsupported("get_chunk"))
    }
    fn has_chunk(&self, _hash: &Hash) -> Result<bool> {
        Err(unsupported("has_chunk"))
    }
    fn store_blob_chunks(&self, _blob_hash: &Hash, _chunks: &[ChunkInfo]) -> Result<()> {
        Err(unsupported("store_blob_chunks"))
    }
    fn get_blob_chunks(&self, _blob_hash: &Hash) -> Result<Option<Vec<ChunkInfo>>> {
        Err(unsupported("get_blob_chunks"))
    }
}

#[allow(dead_code)]
fn _silence_unused_field(g: &GitRepository) -> &Path {
    // `oak_sidecar_dir` is held for Phase 2 (branch metadata sidecar, etc.).
    &g.oak_sidecar_dir
}
