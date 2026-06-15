use std::collections::HashMap;

use crate::{
    Blob, Branch, BranchStatus, Capabilities, ChunkInfo, Commit, FileChange, Hash, Manifest,
    ManifestEntry, MetadataKey, Result, Tag, Tree, TreeEntry,
};

/// A working-tree stat-cache row: the blob hash a file produced the last time
/// it was scanned, keyed (by the caller) on its path and validated by `(mtime_ns,
/// ctime_ns, size)`. If a file's mtime, ctime, and size all match a cached row,
/// the scan can reuse `blob_hash` instead of re-reading and re-hashing the file —
/// the same trick git's index uses to keep `status` cheap on large trees.
///
/// `ctime_ns` (inode change time) is the guard against a forged `(mtime, size)`:
/// any content write bumps ctime, and so does an mtime reset (`touch`, `utime`,
/// a checkout), because the metadata-update syscall itself touches ctime — and
/// ctime can't be set backwards from userspace. Without it, an edit that keeps
/// the size and reuses an old mtime (same-second clock, rapid edits, explicit
/// reset) looks unchanged and the change is silently dropped by status, diff,
/// AND commit. `ctime_ns` is 0 on platforms with no POSIX ctime (e.g. Windows),
/// where the hit check degrades to the previous `(mtime, size)` behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatCacheEntry {
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Inode change time (ctime) in nanoseconds since the Unix epoch, or 0 when
    /// the platform can't report one.
    pub ctime_ns: i64,
    /// File size in bytes.
    pub size: i64,
    /// The blob hash this file content produced when last hashed.
    pub blob_hash: Hash,
}

/// A deferred blob content producer for [`Repository::put_blob_sources`]:
/// `read` is invoked exactly once — possibly on a worker thread — to obtain
/// the bytes to store (e.g. by reading a file). Boxed so the trait stays
/// object-safe.
pub struct BlobSource {
    /// Content size in bytes when known in advance (e.g. from the directory
    /// walk's stat), letting a backend bound how much content it keeps in
    /// flight at once. `None` is treated as "could be huge" and admitted with
    /// the most conservative charge.
    pub size_hint: Option<u64>,
    /// Produces the content. Invoked exactly once.
    pub read: Box<dyn FnOnce() -> Result<Vec<u8>> + Send>,
}

impl BlobSource {
    pub fn new(
        size_hint: Option<u64>,
        read: impl FnOnce() -> Result<Vec<u8>> + Send + 'static,
    ) -> Self {
        Self {
            size_hint,
            read: Box::new(read),
        }
    }

    /// Wrap content that's already in memory (its size hint is exact).
    pub fn from_content(content: Vec<u8>) -> Self {
        let size = content.len() as u64;
        Self::new(Some(size), move || Ok(content))
    }
}

/// Trait defining the storage interface for Oak repositories
pub trait Repository: Send + Sync {
    /// Advertise which Oak features this backend supports.
    ///
    /// CLI commands should check this before invoking feature-specific methods so they
    /// can fail fast with a clear message. Methods for unsupported features may also
    /// return `OakError::Unsupported` as a backstop.
    ///
    /// Default is `Capabilities::full()` — backends that don't support every feature
    /// (e.g. the git backend) override this.
    fn capabilities(&self) -> Capabilities {
        Capabilities::full()
    }

    // Blob operations
    fn store_blob(&self, blob: &Blob) -> Result<()>;
    fn get_blob(&self, hash: &Hash) -> Result<Option<Blob>>;
    fn has_blob(&self, hash: &Hash) -> Result<bool>;

    // Manifest operations
    fn store_manifest(&self, manifest: &Manifest) -> Result<()>;
    fn get_manifest(&self, hash: &Hash) -> Result<Option<Manifest>>;

    // Tree operations (content-addressed nested manifests).
    //
    // Defaults are intentionally inefficient — they emulate trees on top of the
    // flat manifest tables so backends can opt in incrementally. SQLite/Postgres
    // override these with native tree-table implementations; the git backend
    // overrides them to use git's native tree objects.
    fn store_tree(&self, tree: &Tree) -> Result<()> {
        let _ = tree;
        Err(crate::OakError::Unsupported {
            backend: "default",
            op: "store_tree",
        })
    }

    fn get_tree(&self, hash: &Hash) -> Result<Option<Tree>> {
        let _ = hash;
        Err(crate::OakError::Unsupported {
            backend: "default",
            op: "get_tree",
        })
    }

    fn has_tree(&self, hash: &Hash) -> Result<bool> {
        Ok(self.get_tree(hash)?.is_some())
    }

    /// Build a nested tree from a flat list of entries and store every tree
    /// object produced. Returns the root tree hash. Default impl uses
    /// [`crate::build_tree`] and the per-tree `store_tree`.
    fn put_tree(&self, entries: Vec<ManifestEntry>) -> Result<Hash> {
        let built = crate::build_tree(&entries)?;
        for tree in &built.trees {
            self.store_tree(tree)?;
        }
        Ok(built.root_hash)
    }

    /// Walk the tree rooted at `root` and return its flat entry list.
    fn walk_tree(&self, root: &Hash) -> Result<Vec<ManifestEntry>> {
        let mut fetch = |h: &Hash| -> Result<Tree> {
            self.get_tree(h)?
                .ok_or_else(|| crate::OakError::ManifestNotFound(h.to_string()))
        };
        crate::flatten_tree(root, &mut fetch)
    }

    /// Resolve a `/`-separated path to a TreeEntry within the tree at `root`.
    fn resolve_path_in_tree(&self, root: &Hash, path: &str) -> Result<Option<TreeEntry>> {
        let mut fetch = |h: &Hash| -> Result<Tree> {
            self.get_tree(h)?
                .ok_or_else(|| crate::OakError::ManifestNotFound(h.to_string()))
        };
        crate::resolve_path(root, path, &mut fetch)
    }

    /// Return the hash of the subtree at `path` (must be a directory).
    /// An empty `path` returns `Some(root)`.
    fn subtree_at_path(&self, root: &Hash, path: &str) -> Result<Option<Hash>> {
        let mut fetch = |h: &Hash| -> Result<Tree> {
            self.get_tree(h)?
                .ok_or_else(|| crate::OakError::ManifestNotFound(h.to_string()))
        };
        crate::subtree_at_path(root, path, &mut fetch)
    }

    // Branch operations
    fn store_branch(&self, branch: &Branch) -> Result<()>;
    fn get_branch(&self, name: &str) -> Result<Option<Branch>>;
    fn list_branches(&self) -> Result<Vec<Branch>>;
    fn update_branch_status(&self, name: &str, status: BranchStatus) -> Result<()>;
    /// Close a branch and optionally record an explicit close reason.
    fn close_branch(&self, name: &str, close_reason: Option<crate::CloseReason>) -> Result<()> {
        self.update_branch_status(name, BranchStatus::Closed)?;
        if let Some(reason) = close_reason {
            self.update_branch_close_reason(name, reason)?;
        }
        Ok(())
    }
    fn update_branch_close_reason(
        &self,
        name: &str,
        close_reason: crate::CloseReason,
    ) -> Result<()> {
        let _ = (name, close_reason);
        Err(crate::OakError::Unsupported {
            backend: "default",
            op: "update_branch_close_reason",
        })
    }
    fn update_branch_description(&self, name: &str, description: &str) -> Result<()>;
    fn rename_branch(&self, old_name: &str, new_name: &str) -> Result<()> {
        let _ = (old_name, new_name);
        Err(crate::OakError::Unsupported {
            backend: "default",
            op: "rename_branch",
        })
    }
    /// Remove a branch row and its head pointer from local storage. Commits
    /// authored on the branch are untouched — `commits.branch_name` is a soft
    /// label. Deleting a branch that doesn't exist is a no-op.
    fn delete_branch(&self, name: &str) -> Result<()> {
        let _ = name;
        Err(crate::OakError::Unsupported {
            backend: "default",
            op: "delete_branch",
        })
    }
    fn get_branch_head(&self, name: &str) -> Result<Option<Hash>>;
    fn set_branch_head(&self, name: &str, hash: &Hash) -> Result<()>;

    /// Atomically close a remotely-merged branch, create the fresh current
    /// branch, and point branch/HEAD metadata at `main_head`.
    ///
    /// Default impl calls individual methods (not atomic across calls). SQLite
    /// overrides with a true single-transaction impl.
    fn reconcile_remote_merge_branch_state(
        &self,
        closed_branch: &str,
        new_branch: &Branch,
        main_head: &Hash,
    ) -> Result<()> {
        self.update_branch_status(closed_branch, BranchStatus::Closed)?;
        self.store_branch(new_branch)?;
        self.set_branch_head(&new_branch.name, main_head)?;
        self.set_head(main_head)?;
        self.set_current_branch(&new_branch.name)
    }

    // Commit operations
    fn store_commit(&self, commit: &Commit) -> Result<()>;
    fn get_commit(&self, hash: &Hash) -> Result<Option<Commit>>;
    fn has_commit(&self, hash: &Hash) -> Result<bool>;
    fn get_commits_for_branch(&self, branch_name: &str) -> Result<Vec<Commit>>;
    fn get_commits_since(&self, branch_name: &str, since: Option<&Hash>) -> Result<Vec<Commit>>;
    fn get_all_commits(&self) -> Result<Vec<Commit>>;

    /// Fast negative check for "does any local commit claim this commit as its
    /// merge parent?" SQLite backs this with an index on
    /// `commits.merge_parent_hash`; the default implementation is only for
    /// backends without a native query path.
    fn merge_child_exists(&self, branch_head: &Hash) -> Result<bool> {
        Ok(self
            .get_all_commits()?
            .iter()
            .any(|c| c.merge_parent_hash.as_ref() == Some(branch_head)))
    }

    /// Number of commits authored directly on `branch_name` — rows whose
    /// `branch_name` column equals this branch. In Oak's branch-per-session
    /// model a feature branch is seeded at its parent's head with none of its
    /// own commits; each `oak commit` adds one, and a squash-merge moves you
    /// onto a fresh branch with zero again. So this count is exactly the
    /// branch's local work not yet merged into its parent. Purely local — it
    /// never consults the server. Defaults to counting
    /// [`Repository::get_commits_for_branch`]; backends override it with a
    /// cheaper `COUNT(*)`.
    fn count_commits_for_branch(&self, branch_name: &str) -> Result<usize> {
        Ok(self.get_commits_for_branch(branch_name)?.len())
    }

    // Metadata operations
    fn get_metadata(&self, key: MetadataKey) -> Result<Option<String>>;
    fn set_metadata(&self, key: MetadataKey, value: &str) -> Result<()>;

    // --- Working-tree stat cache ----------------------------------------------------
    //
    // Lets the CLI's working-dir scan skip re-reading + re-hashing files whose
    // `(mtime, size)` are unchanged since the last scan. Purely a local
    // working-tree optimization, so the default impls are no-ops: backends that
    // don't materialize a working tree (or haven't opted in) simply re-hash every
    // file as before. SQLite overrides these with a real `stat_cache` table.

    /// Load the entire stat cache as a `path -> entry` map.
    fn load_stat_cache(&self) -> Result<HashMap<String, StatCacheEntry>> {
        Ok(HashMap::new())
    }

    /// Apply a batch of stat-cache changes: upsert `upserts` (path + entry) and
    /// delete the paths in `removed`. Implementations should run this as one
    /// transaction. An empty batch must be a no-op so an unchanged `oak status`
    /// performs zero writes.
    fn update_stat_cache(
        &self,
        upserts: &[(String, StatCacheEntry)],
        removed: &[String],
    ) -> Result<()> {
        let _ = (upserts, removed);
        Ok(())
    }

    // Tag operations
    fn create_tag(&self, name: &str, commit_hash: &Hash) -> Result<()>;
    fn list_tags(&self) -> Result<Vec<Tag>>;
    fn delete_tag(&self, name: &str) -> Result<()>;

    // Chunk operations
    fn store_chunk(&self, hash: &Hash, content: &[u8]) -> Result<()>;
    fn get_chunk(&self, hash: &Hash) -> Result<Option<Vec<u8>>>;
    fn has_chunk(&self, hash: &Hash) -> Result<bool>;
    fn store_blob_chunks(&self, blob_hash: &Hash, chunks: &[ChunkInfo]) -> Result<()>;
    fn get_blob_chunks(&self, blob_hash: &Hash) -> Result<Option<Vec<ChunkInfo>>>;

    /// Atomically store manifest, commit, update branch head, and legacy head
    /// in a single transaction. Default impl calls individual methods (not atomic
    /// across calls). SQLite overrides with a true single-transaction impl.
    fn atomic_commit(&self, manifest: &Manifest, commit: &Commit, branch_name: &str) -> Result<()> {
        self.store_manifest(manifest)?;
        self.store_commit(commit)?;
        self.set_branch_head(branch_name, &commit.hash)?;
        self.set_head(&commit.hash)?;
        Ok(())
    }

    // --- Backend-aware "put" methods ------------------------------------------------
    //
    // These hash and store in one shot, returning the canonical hash assigned by the
    // backend. SQLite/Postgres backends compute BLAKE3 (matches their storage primary
    // key); the git backend writes the object via gix and returns the git OID, which
    // is unrelated to the BLAKE3 a caller might have precomputed.
    //
    // Default impls preserve the existing two-step (Blob::new + store_blob) behavior
    // for backends that haven't overridden them — that means the input data is
    // BLAKE3-hashed even though it could be wasted work on backends that don't use
    // BLAKE3. Backends that care about that cost should override.

    /// Hash and store a blob's content. Returns the backend's canonical hash.
    fn put_blob(&self, content: Vec<u8>) -> Result<Hash> {
        let blob = Blob::new(content);
        let hash = blob.hash.clone();
        self.store_blob(&blob)?;
        Ok(hash)
    }

    /// Hash and store many blobs, returning each content's canonical hash in
    /// input order. Semantically identical to a [`Repository::put_blob`] loop
    /// (which is the default impl); backends with per-call write overhead
    /// override this to batch the stores into one transaction, so callers with
    /// many blobs in hand (e.g. the working-dir scan) should prefer it.
    fn put_blobs(&self, contents: Vec<Vec<u8>>) -> Result<Vec<Hash>> {
        contents.into_iter().map(|c| self.put_blob(c)).collect()
    }

    /// [`Repository::put_blobs`] with the contents produced on demand: each
    /// source's `read` is invoked exactly once, possibly on a worker thread,
    /// to obtain the bytes to store. Semantically identical to draining every
    /// source and calling `put_blobs` (which is the default impl), but it lets
    /// a backend overlap producing one blob's content (e.g. reading a file)
    /// with hashing/encoding/storing another's instead of forcing
    /// all-produce-then-all-hash barriers — which is what makes a snapshot of
    /// a few large files fast.
    fn put_blob_sources(&self, sources: Vec<BlobSource>) -> Result<Vec<Hash>> {
        sources
            .into_iter()
            .map(|source| self.put_blob((source.read)()?))
            .collect()
    }

    /// Build and store a manifest from the given entries. Returns the backend's
    /// canonical hash for the manifest (a tree OID on the git backend).
    fn put_manifest(&self, entries: Vec<ManifestEntry>) -> Result<Hash> {
        let manifest = Manifest::new(entries);
        let hash = manifest.hash.clone();
        self.store_manifest(&manifest)?;
        Ok(hash)
    }

    /// Build and store a commit. Returns the backend's canonical commit hash.
    ///
    /// Note: the `branch_name` and `files` fields are part of Oak's commit model
    /// but have no representation on the git backend (git commits know nothing
    /// about branches and derive file lists from tree diffs). The git backend
    /// will drop them on write.
    #[allow(clippy::too_many_arguments)]
    fn put_commit(
        &self,
        branch_name: String,
        parent_hash: Option<Hash>,
        merge_parent_hash: Option<Hash>,
        manifest_hash: Hash,
        author: String,
        message: Option<String>,
        _timestamp: chrono::DateTime<chrono::Utc>,
        files: Vec<FileChange>,
    ) -> Result<Hash> {
        // Default impl uses `Commit::new` which stamps `Utc::now()` and includes it
        // in the BLAKE3 derivation — so the supplied `_timestamp` is dropped. That's
        // fine for SQLite's existing callers (they compute "now" themselves and don't
        // depend on bit-for-bit timestamp preservation). Backends that need to honor
        // the supplied timestamp (git) should override this method.
        let commit = Commit::new(
            branch_name,
            parent_hash,
            merge_parent_hash,
            manifest_hash,
            author,
            message,
            files,
        )?;
        let hash = commit.hash.clone();
        self.store_commit(&commit)?;
        Ok(hash)
    }

    // Head operations (convenience methods)
    fn get_head(&self) -> Result<Option<Hash>> {
        self.get_metadata(MetadataKey::Head)
            .map(|opt| opt.map(Hash))
    }

    fn set_head(&self, hash: &Hash) -> Result<()> {
        self.set_metadata(MetadataKey::Head, hash.as_str())
    }

    // Remote operations
    fn get_remote_url(&self) -> Result<Option<String>> {
        self.get_metadata(MetadataKey::RemoteUrl)
    }

    fn set_remote_url(&self, url: &str) -> Result<()> {
        self.set_metadata(MetadataKey::RemoteUrl, url)
    }

    // Current branch convenience.
    //
    // The detached-HEAD state is recorded by writing an empty string into the
    // CurrentBranch metadata (see `oak switch -d` / `oak checkout`). All
    // callers want to treat that the same as "no branch", so we collapse
    // `Some("")` to `None` here at the read boundary instead of forcing
    // every caller to remember the convention.
    fn get_current_branch_name(&self) -> Result<Option<String>> {
        Ok(self
            .get_metadata(MetadataKey::CurrentBranch)?
            .filter(|s| !s.is_empty()))
    }

    fn set_current_branch(&self, name: &str) -> Result<()> {
        self.set_metadata(MetadataKey::CurrentBranch, name)
    }
}
