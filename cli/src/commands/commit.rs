use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use dialoguer::Confirm;
use oak_core::{
    detect_renames_by_content, dir_in_any_prefix, path_in_any_prefix, split_manifest_by_prefixes,
    BranchStatus, ChangeType, FileChange, FileMode, Hash, IgnorePatterns, Manifest, ManifestEntry,
    MetadataKey, OakError, RenameConfig, Result,
};
use oak_core::{Repository, StatCacheEntry};
use rayon::prelude::*;

use super::hooks;
use super::project as scope;
use super::push;

use crate::output;

/// Get the author name from environment or default
pub fn get_author() -> String {
    std::env::var("OAK_AUTHOR")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Scan the working directory and create manifest entries.
///
/// Each file's content is hashed and stored via `repo.put_blob`, so the resulting
/// `ManifestEntry::blob_hash` is whatever the backend considers canonical (BLAKE3
/// for sqlite, git OID for the git backend).
///
/// When a project scope is active (non-empty `prefixes`), directories outside
/// every prefix are pruned and any stray in-tree files outside the scope are
/// skipped. This keeps the resulting entry list disjoint from the parent
/// commit's out-of-scope entries so the caller can merge the two halves
/// cleanly.
pub fn scan_working_dir(
    root: &Path,
    _work_tree: &Path,
    repo: &dyn Repository,
    ignore: &IgnorePatterns,
) -> Result<Vec<ManifestEntry>> {
    scan_working_dir_with_scope(root, _work_tree, repo, ignore, &[])
}

/// Scan honoring an active project scope, trusting the working-tree stat
/// cache for files whose `(mtime, ctime, size)` are unchanged. This is the
/// fast path used by `oak status`, `oak diff`, and ordinary `oak commit`.
pub fn scan_working_dir_with_scope(
    root: &Path,
    work_tree: &Path,
    repo: &dyn Repository,
    ignore: &IgnorePatterns,
    prefixes: &[String],
) -> Result<Vec<ManifestEntry>> {
    scan_working_dir_inner(root, work_tree, repo, ignore, prefixes, true)
}

/// Scan WITHOUT trusting the stat cache: every file is re-read and re-hashed,
/// and the cache is rebuilt from what's actually on disk. Used by the
/// conflict-resolution scans (`oak pull --continue` / `oak merge --continue`),
/// which run rarely but at high stakes. Re-hashing there guarantees the
/// recorded manifest reflects on-disk content and can never echo a stale
/// cache row left behind by a branch-switch/sync that materialized another
/// branch's version of a path — the foreign-blob data-integrity bug that the
/// materializer cache-refresh (`refresh_stat_cache_after_materialize`) also
/// guards against from the other direction.
pub fn scan_working_dir_no_cache(
    root: &Path,
    work_tree: &Path,
    repo: &dyn Repository,
    ignore: &IgnorePatterns,
) -> Result<Vec<ManifestEntry>> {
    scan_working_dir_inner(root, work_tree, repo, ignore, &[], false)
}

/// Shared implementation. `use_cache` gates only whether an unchanged
/// `(mtime, ctime, size)` may *reuse* a cached blob hash; cache writes (fresh
/// rows for re-hashed files, pruning of vanished paths) happen in both modes,
/// so a `use_cache = false` scan also repairs a stale cache as a side effect.
fn scan_working_dir_inner(
    root: &Path,
    _work_tree: &Path,
    repo: &dyn Repository,
    ignore: &IgnorePatterns,
    prefixes: &[String],
    use_cache: bool,
) -> Result<Vec<ManifestEntry>> {
    // One file the walk found: enough metadata to consult the stat cache
    // without touching contents. `mtime_ns` is `None` when the platform can't
    // report a modification time, in which case the file is always re-hashed
    // and never cached. `ctime_ns` (inode change time) is the forgery guard on
    // top of `(mtime, size)` — see `StatCacheEntry`; it's 0 where the platform
    // has no POSIX ctime.
    struct WalkedFile {
        rel_path: String,
        abs_path: PathBuf,
        size: i64,
        mtime_ns: Option<i64>,
        ctime_ns: i64,
        mode: FileMode,
    }

    // Phase 1: walk the directory tree (single-threaded, no file reads). This
    // is cheap relative to hashing — it only stats entries — so it stays serial
    // and feeds the parallel hashing phase below.
    fn walk_dir(
        dir: &Path,
        root: &Path,
        ignore: &IgnorePatterns,
        prefixes: &[String],
        files: &mut Vec<WalkedFile>,
    ) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let relative_path = path.strip_prefix(root).unwrap();

            // Skip ignored files
            if ignore.is_ignored(relative_path, path.is_dir()) {
                continue;
            }

            let rel_str = relative_path.to_string_lossy();
            // Apply the scope filter as a second filter. Directories use
            // `dir_in_any_prefix` (descend if any prefix is inside or below
            // the dir); files use `path_in_any_prefix` (definitive).
            if !prefixes.is_empty() {
                let normalized = rel_str.replace('\\', "/");
                if path.is_dir() {
                    if !dir_in_any_prefix(prefixes, &normalized) {
                        continue;
                    }
                } else if !path_in_any_prefix(prefixes, &normalized) {
                    continue;
                }
            }

            if path.is_dir() {
                walk_dir(&path, root, ignore, prefixes, files)?;
            } else if path.is_file() {
                let md = fs::metadata(&path)?;
                // Stat for the cache: size + mtime + ctime. ctime is the
                // forgery guard (bumps on any content write or mtime reset and
                // can't be moved backwards from userspace, so it catches an
                // edit that forges a matching `(mtime, size)`). Shared with the
                // materializer cache-refresh path via `file_stat_for_cache` so
                // the two can't drift.
                let (size, mtime_ns, ctime_ns) = file_stat_for_cache(&md);

                // Determine file mode
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    if md.permissions().mode() & 0o111 != 0 {
                        FileMode::Executable
                    } else {
                        FileMode::Regular
                    }
                };

                #[cfg(not(unix))]
                let mode = FileMode::Regular;

                files.push(WalkedFile {
                    // Forward-slashes are oak's canonical manifest separator.
                    // On Windows `to_string_lossy()` produces `\`, which would
                    // make `oak status` after a fresh clone report every file
                    // as renamed (`foo/bar` -> `foo\bar`) and break Linux ↔
                    // Windows interop on commits authored from Windows.
                    rel_path: relative_path.to_string_lossy().replace('\\', "/"),
                    abs_path: path,
                    size,
                    mtime_ns,
                    ctime_ns,
                    mode,
                });
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    walk_dir(root, root, ignore, prefixes, &mut files)?;

    // Load the working-tree stat cache once. A file whose (mtime, ctime, size)
    // all match a cached row reuses that row's blob hash — no read, no re-hash.
    // This is what makes `oak status` cheap on huge trees where almost nothing
    // changed; ctime is what keeps it from being fooled by a forged (mtime,
    // size) (see `StatCacheEntry`).
    let cache = repo.load_stat_cache()?;

    // Phase 2: resolve each file to a manifest entry. Cache hits are pure
    // lookups; misses read + hash + store the blob. Misses run in parallel
    // (rayon) since reading and BLAKE3-hashing dominate the cost. Each miss also
    // emits a fresh cache row to persist.
    type ScanItem = (ManifestEntry, Option<(String, StatCacheEntry)>);
    let scanned: Vec<ScanItem> = files
        .par_iter()
        .map(|f| -> Result<ScanItem> {
            // Trust a cached hash only in cache mode, and only when mtime,
            // ctime, AND size all match. ctime is the forgery guard: an edit
            // that reuses an old mtime at the same size still bumps ctime, so
            // it misses here and gets re-hashed below (see `StatCacheEntry`).
            // `oak pull/merge --continue` pass `use_cache = false` to force a
            // re-hash regardless (see `scan_working_dir_no_cache`).
            if use_cache {
                if let (Some(mtime_ns), Some(prev)) = (f.mtime_ns, cache.get(&f.rel_path)) {
                    if prev.mtime_ns == mtime_ns
                        && prev.ctime_ns == f.ctime_ns
                        && prev.size == f.size
                    {
                        return Ok((
                            ManifestEntry {
                                path: f.rel_path.clone(),
                                blob_hash: prev.blob_hash.clone(),
                                mode: f.mode,
                            },
                            None,
                        ));
                    }
                }
            }

            // Hash and store via the backend so we get its canonical OID.
            let content = fs::read(&f.abs_path)?;
            let blob_hash = repo.put_blob(content)?;
            let upsert = f.mtime_ns.map(|mtime_ns| {
                (
                    f.rel_path.clone(),
                    StatCacheEntry {
                        mtime_ns,
                        ctime_ns: f.ctime_ns,
                        size: f.size,
                        blob_hash: blob_hash.clone(),
                    },
                )
            });
            Ok((
                ManifestEntry {
                    path: f.rel_path.clone(),
                    blob_hash,
                    mode: f.mode,
                },
                upsert,
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut entries = Vec::with_capacity(scanned.len());
    let mut upserts = Vec::new();
    for (entry, upsert) in scanned {
        entries.push(entry);
        if let Some(u) = upsert {
            upserts.push(u);
        }
    }

    // Phase 3: persist cache changes. Prune rows for files that no longer exist,
    // but under an active scope only prune in-scope paths — we never walked the
    // out-of-scope ones, so they aren't "missing", just unseen.
    let seen: HashSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    let removed: Vec<String> = cache
        .keys()
        .filter(|k| !seen.contains(k.as_str()))
        .filter(|k| prefixes.is_empty() || path_in_any_prefix(prefixes, k))
        .cloned()
        .collect();
    repo.update_stat_cache(&upserts, &removed)?;

    Ok(entries)
}

/// Extract the stat-cache triple `(size, mtime_ns, ctime_ns)` from a file's
/// metadata. `mtime_ns` is `None` when the platform can't report a
/// modification time (then the file is never cached and always re-hashed).
/// `ctime_ns` (inode change time) is the forgery guard on top of
/// `(mtime, size)`: it bumps on any content write or mtime reset and can't be
/// moved backwards from userspace; it's 0 where there's no POSIX ctime (e.g.
/// Windows), degrading the hit check to `(mtime, size)`. Shared by the
/// working-dir walk and by [`stat_cache_upsert`] so the two never drift.
pub(crate) fn file_stat_for_cache(md: &fs::Metadata) -> (i64, Option<i64>, i64) {
    let size = md.len() as i64;
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64);
    #[cfg(unix)]
    let ctime_ns = {
        use std::os::unix::fs::MetadataExt;
        md.ctime()
            .saturating_mul(1_000_000_000)
            .saturating_add(md.ctime_nsec())
    };
    #[cfg(not(unix))]
    let ctime_ns = 0i64;
    (size, mtime_ns, ctime_ns)
}

/// Build a stat-cache upsert for a file a materializer just wrote to
/// `abs_path` with known content hash `blob_hash`. Returns `None` — meaning
/// "don't cache this path, let the next scan re-hash it" — when the file can't
/// be stat'd or the platform reports no mtime.
///
/// Callers that materialize the working tree (branch switch, sync/merge reset,
/// conflict write-out, pull) MUST refresh the cache for what they wrote: the
/// `stat_cache` table is keyed by path only and shared across every branch in
/// the working dir, so a row left over from another branch's version of a path
/// would otherwise be trusted on the next scan and silently record a foreign
/// blob — the data-integrity bug this guards. Stat after the write *and* any
/// permission change, since chmod also bumps ctime.
pub(crate) fn stat_cache_upsert(
    rel_path: &str,
    abs_path: &Path,
    blob_hash: &Hash,
) -> Option<(String, StatCacheEntry)> {
    let md = fs::metadata(abs_path).ok()?;
    let (size, mtime_ns, ctime_ns) = file_stat_for_cache(&md);
    Some((
        rel_path.to_string(),
        StatCacheEntry {
            mtime_ns: mtime_ns?,
            ctime_ns,
            size,
            blob_hash: blob_hash.clone(),
        },
    ))
}

/// Refresh the stat cache after a materialization wrote `upserts` to the
/// working tree, dropping rows for paths no longer present so the cache stays
/// a faithful mirror of the on-disk tree. Under an active scope (`prefixes`)
/// only in-scope rows are pruned — out-of-scope files were never touched.
/// Mirrors [`scan_working_dir`]'s phase-3 pruning.
///
/// Pass one upsert per file the materializer actually wrote; any cached path
/// not in `upserts` and in scope is treated as vanished and pruned. Callers
/// that only *add* files without deleting (the conflict write-out) should
/// instead upsert directly with an empty `removed` set.
pub(crate) fn refresh_stat_cache_after_materialize(
    repo: &dyn Repository,
    upserts: Vec<(String, StatCacheEntry)>,
    prefixes: &[String],
) -> Result<()> {
    let upserted: HashSet<&str> = upserts.iter().map(|(p, _)| p.as_str()).collect();
    let removed: Vec<String> = repo
        .load_stat_cache()?
        .into_keys()
        .filter(|k| !upserted.contains(k.as_str()))
        .filter(|k| prefixes.is_empty() || path_in_any_prefix(prefixes, k))
        .collect();
    repo.update_stat_cache(&upserts, &removed)
}

/// Options that can tweak commit behavior beyond message + path.
///
/// Kept as a struct (not extra `run_*` variants) so callsites that don't care
/// — primarily tests — stay on the simpler [`run`] entry point and aren't
/// disturbed when we add new toggles.
#[derive(Debug, Default, Clone, Copy)]
pub struct CommitOptions {
    /// Skip both `pre-commit` and `post-commit` hooks. Mirrors `git commit
    /// --no-verify`.
    pub no_verify: bool,
}

/// Create a commit within the current branch using default options.
///
/// Local commits no longer carry messages — the source of truth for "what
/// happened" is the branch description, which becomes the (single) main
/// commit's message at squash-merge time on the server.
pub fn run(path: &Path) -> Result<()> {
    run_with_options(path, CommitOptions::default())
}

/// Variant of [`run`] that accepts behavioral toggles.
pub fn run_with_options(path: &Path, opts: CommitOptions) -> Result<()> {
    let no_verify = opts.no_verify;
    output::vlog("commit: start");
    // `oak commit` somewhere with no repository: rather than erroring out,
    // offer to initialize one right here (interactive only). This is the same
    // courtesy `oak status` extends, and it lets a brand-new project go from
    // `oak commit` straight to a tracked repo — and, after the commit, an
    // offer to push it up as a new repo on the server.
    let ctx = match crate::resolve::resolve(path) {
        Ok(ctx) => ctx,
        Err(OakError::RepoNotFound) => match offer_init_here(path)? {
            Some(ctx) => ctx,
            None => return Err(OakError::RepoNotFound),
        },
        Err(e) => return Err(e),
    };
    let work_tree = ctx.work_tree.clone();
    let _lock = crate::workdir_lock::WorkdirLock::acquire(&ctx.oak_dir)?;

    // Run pre-commit hook BEFORE opening the repo so a hook that mutates
    // working-tree files (e.g. `cargo fmt`) is reflected in the scan below.
    // We hold the workdir lock across the hook so two parallel commits can't
    // race on the same scripts.
    if !no_verify {
        hooks::run_hook(&work_tree, "pre-commit", true)?;
    }

    let repo = ctx.open()?;
    output::vlog("commit: opened repo + acquired workdir lock");

    // Get branch name from DB / git HEAD
    let branch_name = repo
        .get_current_branch_name()?
        .ok_or_else(|| OakError::BranchNotFound("no current branch set".to_string()))?;

    // Verify the branch exists and is open. On the git backend `get_branch` always
    // returns Open (no metadata sidecar yet) so this never trips there.
    let branch = repo
        .get_branch(&branch_name)?
        .ok_or_else(|| OakError::BranchNotFound(branch_name.clone()))?;

    if branch.status == BranchStatus::Closed {
        return Err(OakError::BranchClosed(branch_name.clone()));
    }

    let ignore = IgnorePatterns::new(&work_tree)?;

    // Resolve the active scope (if any). When set, the working-tree scan
    // only produces entries inside the scope; the parent's out-of-scope
    // entries are merged back in below before storing the new manifest.
    let prefixes = scope::active_prefixes(repo.as_ref())?;
    if !prefixes.is_empty() {
        output::vlog(&format!(
            "commit: active scope ({} prefix(es))",
            prefixes.len()
        ));
    }

    // Scan working directory (filtered by active scope if any)
    let entries =
        scan_working_dir_with_scope(&work_tree, &work_tree, repo.as_ref(), &ignore, &prefixes)?;
    output::vlog(&format!(
        "commit: scanned working dir ({} entries)",
        entries.len()
    ));

    // Get branch head as parent hash. If this branch has no commits yet,
    // inherit the parent branch's head so the first commit diffs against it
    // and is recorded as its child.
    let parent_hash = resolve_effective_head(repo.as_ref(), &branch_name)?;

    // Whether this is the first commit reachable from the current branch.
    // Used below to offer a one-time "push this to a new repo?" when the repo
    // isn't linked to a remote yet.
    let is_first_commit = parent_hash.is_none();

    // Get old manifest for comparison
    let old_manifest = if let Some(ref head_hash) = parent_hash {
        let head_commit = repo
            .get_commit(head_hash)?
            .ok_or_else(|| OakError::CommitNotFound(head_hash.to_string()))?;
        repo.get_manifest(&head_commit.manifest_hash)?
            .unwrap_or_else(Manifest::empty)
    } else {
        Manifest::empty()
    };

    // Build the diff manifest: when a scope is active, restrict diffing to
    // in-scope entries. Out-of-scope entries are preserved verbatim from
    // the parent commit, so a partial-scope commit can never delete or
    // modify anything the user can't see.
    let (diff_old, preserved_out_of_scope) = if !prefixes.is_empty() {
        let (inside, outside) = split_manifest_by_prefixes(&old_manifest, &prefixes);
        let in_scope: Vec<ManifestEntry> = inside.into_iter().cloned().collect();
        let out_of_scope: Vec<ManifestEntry> = outside.into_iter().cloned().collect();
        (Manifest::new(in_scope), out_of_scope)
    } else {
        (old_manifest.clone(), Vec::new())
    };

    let new_manifest = Manifest::new(entries);

    // Compute file changes. The manifest-level diff catches exact-hash
    // renames (pure moves with no edit); content-similarity below promotes
    // any rename-with-edit pairs in the same pass.
    let mut changes = diff_old.diff(&new_manifest);
    detect_renames_by_content(
        &mut changes,
        |h| Ok(repo.get_blob(h)?.map(|b| b.content)),
        &RenameConfig::default(),
    )?;
    output::vlog(&format!(
        "commit: diffed manifest ({} changes)",
        changes.len()
    ));

    // Defensive: if a scope is active, no recorded change may name an
    // out-of-scope path. The `scan_working_dir_with_scope` call already
    // prunes working-tree entries; this catches edge cases (e.g. a
    // rename whose target falls outside the scope).
    if !prefixes.is_empty() {
        let touched: Vec<&str> = changes
            .iter()
            .map(|c| c.path.as_str())
            .chain(changes.iter().filter_map(|c| c.old_path.as_deref()))
            .collect();
        scope::reject_out_of_scope(&prefixes, touched.into_iter())?;
    }

    // Check if there are any changes
    if changes.is_empty() {
        output::info("Nothing to commit, working directory clean");
        return Ok(());
    }

    // No commit message: under the new model, branch descriptions are the
    // source of truth for "what happened." A real message only gets
    // attached when the server squash-merges a feature branch into main.

    // Store the manifest using the backend's canonical hash (BLAKE3 for sqlite,
    // tree OID for git). Pulling out the entries for `put_manifest` lets the git
    // backend rebuild a nested tree; the diff above used a pre-built `new_manifest`
    // for change detection only.
    //
    // When a scope is active, append the parent's out-of-scope entries so
    // the stored manifest reflects the full repo state, not just the in-
    // scope subset.
    let mut entries_to_store = new_manifest.entries.clone();
    if !preserved_out_of_scope.is_empty() {
        entries_to_store.extend(preserved_out_of_scope.iter().cloned());
    }
    let manifest_hash = repo.put_manifest(entries_to_store)?;
    output::vlog("commit: stored manifest");

    let files: Vec<FileChange> = changes
        .iter()
        .map(|c| FileChange {
            path: c.path.clone(),
            change_type: c.change_type,
            old_blob_hash: c.old_blob_hash.clone(),
            new_blob_hash: c.new_blob_hash.clone(),
            old_path: c.old_path.clone(),
            old_mode: None,
            new_mode: None,
        })
        .collect();

    let commit_hash = repo.put_commit(
        branch_name.clone(),
        parent_hash,
        None,
        manifest_hash,
        get_author(),
        None,
        chrono::Utc::now(),
        files,
    )?;
    output::vlog("commit: stored commit object");

    repo.set_branch_head(&branch_name, &commit_hash)?;
    repo.set_head(&commit_hash)?;
    output::vlog("commit: updated branch head + HEAD");

    // Print results
    output::success(&format!(
        "Created commit {}{}{} on branch {}{}{}",
        output::colors::CYAN,
        commit_hash.short(),
        output::colors::RESET,
        output::colors::CYAN,
        branch_name,
        output::colors::RESET,
    ));

    for change in &changes {
        if change.change_type == ChangeType::Renamed {
            if let Some(ref old_path) = change.old_path {
                output::item(&output::format_rename(old_path, &change.path));
            } else {
                output::item(&output::format_change_type(
                    change.change_type,
                    &change.path,
                ));
            }
        } else {
            output::item(&output::format_change_type(
                change.change_type,
                &change.path,
            ));
        }
    }

    // Post-commit hook (non-blocking — the commit already landed, so a hook
    // failure here only warns).
    if !no_verify {
        let _ = hooks::run_hook(&work_tree, "post-commit", false);
    }

    // Auto-push if remote is configured
    if let Ok(Some(remote_url)) = repo.get_metadata(MetadataKey::RemoteUrl) {
        output::print_line("");
        output::vlog(&format!("commit: starting auto-push to {remote_url}"));
        let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
        match rt.block_on(push::run(&work_tree, &remote_url, false, None)) {
            Ok(()) => {}
            Err(OakError::ConflictDetected) => {
                output::warning("Push failed: conflict detected. Run 'oak pull' to resolve.");
            }
            Err(e) => {
                output::warning(&format!("Push failed: {e}. Commit saved locally."));
            }
        }
    } else if is_first_commit && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        // First commit in a repo with no remote linked yet: offer to publish it
        // as a new repo on the server. `push::run` walks the user through
        // picking/creating an organization and creating the repo, persists the
        // remote (so later commits auto-push via the branch above), then offers
        // to land the branch on main.
        offer_push_to_new_repo(&work_tree)?;
    }

    Ok(())
}

/// `oak commit` was run somewhere with no Oak repository. Explain the situation
/// and (interactive only) offer to initialize one right here, returning the
/// resolved context when the user accepts. Returns `Ok(None)` when declined or
/// when stdin isn't a terminal, so the caller surfaces the normal
/// `RepoNotFound` error (whose message points at `oak init` / `oak clone`).
fn offer_init_here(path: &Path) -> Result<Option<crate::resolve::RepoContext>> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Ok(None);
    }
    output::warning(&format!(
        "Not an Oak repository (no .oak found in {} or any parent).",
        path.display()
    ));
    let create = Confirm::new()
        .with_prompt("Initialize an Oak repository here so you can commit?")
        .default(true)
        .interact()
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
    if !create {
        return Ok(None);
    }
    output::blank();
    // Reached only past the stdin-terminal guard above, so init runs interactively.
    super::init::run(path, true)?;
    output::blank();
    Ok(Some(crate::resolve::resolve(path)?))
}

/// Prompt (interactive) to publish a freshly-committed, not-yet-linked repo to
/// the server as a new repository. Declining is fine — the work is committed
/// locally and `oak push` can publish it later; a push failure only warns, for
/// the same reason.
fn offer_push_to_new_repo(work_tree: &Path) -> Result<()> {
    let remote = std::env::var("OAK_REMOTE").unwrap_or_else(|_| "https://oakvcs.com".to_string());
    output::blank();
    let confirm = Confirm::new()
        .with_prompt(format!("Push this to a new repo on {remote}?"))
        .default(true)
        .interact()
        .unwrap_or(false);
    if !confirm {
        output::info("Run `oak push` whenever you're ready to publish this to a server.");
        return Ok(());
    }
    let rt = tokio::runtime::Runtime::new().map_err(|e| OakError::Http(e.to_string()))?;
    if let Err(e) = rt.block_on(push::run(work_tree, &remote, false, None)) {
        output::warning(&format!(
            "Push failed: {e}. Commit saved locally — run `oak push` to retry."
        ));
    }
    Ok(())
}

/// Get the current working directory status (for status command)
///
/// Tries to use the current branch head. If no branch is set,
/// falls back to the legacy get_head() for backward compatibility.
/// Resolve the effective head for a branch: its own head if present,
/// otherwise walk parent_branch until one has a head. Guards against cycles.
fn resolve_effective_head(repo: &dyn Repository, name: &str) -> Result<Option<Hash>> {
    let mut seen = std::collections::HashSet::new();
    let mut current = name.to_string();
    loop {
        if !seen.insert(current.clone()) {
            return Ok(None);
        }
        if let Some(h) = repo.get_branch_head(&current)? {
            return Ok(Some(h));
        }
        match repo.get_branch(&current)? {
            Some(b) => match b.parent_branch {
                Some(p) => current = p,
                None => return Ok(None),
            },
            None => return Ok(None),
        }
    }
}

/// How many commits on `branch_name` are waiting to be merged into its parent
/// — the branch's local work that a squash-merge onto `main` would carry.
///
/// This is just the number of commits authored on the branch (see
/// [`Repository::count_commits_for_branch`]): a branch starts seeded at its
/// parent's head with zero commits of its own, each `oak commit` adds one, and
/// merging squashes them onto the parent and drops you on a fresh branch with
/// zero again. It's deliberately offline — we don't diff against the local
/// `main` head, which is treated as unreliable/stale elsewhere (see `sync.rs`)
/// and would also force a network round-trip on the prompt's hot path.
pub(crate) fn unmerged_commit_count(repo: &dyn Repository, branch_name: &str) -> Result<usize> {
    repo.count_commits_for_branch(branch_name)
}

pub fn get_status(path: &Path) -> Result<(Vec<FileChange>, Option<Hash>, Option<String>)> {
    let ctx = crate::resolve::resolve(path)?;
    let work_tree = ctx.work_tree.clone();
    let repo = ctx.open()?;
    compute_changes(repo.as_ref(), &work_tree)
}

/// Compute the working-tree change set against the effective head, honoring the
/// active project/team scope.
///
/// This is the single source of truth for "what changed" — `oak status` and
/// `oak diff` both call it, so the two can never report different path sets. It
/// mirrors the scan + manifest-diff + rename-detection that [`run_with_options`]
/// performs before committing, so all three commands agree by construction
/// (the regression test in `tests/` pins this down). Returns `(changes,
/// effective_head, current_branch_name)`.
pub fn compute_changes(
    repo: &dyn Repository,
    work_tree: &Path,
) -> Result<(Vec<FileChange>, Option<Hash>, Option<String>)> {
    let ignore = IgnorePatterns::new(work_tree)?;

    // Active scope (if any) — same filtering rule as commit so status/diff
    // reflect exactly what the next commit would record.
    let prefixes = scope::active_prefixes(repo)?;

    // Scan working directory
    let entries = scan_working_dir_with_scope(work_tree, work_tree, repo, &ignore, &prefixes)?;

    // Determine the current head: prefer DB branch, fall back to legacy head.
    // For a branch with no commits of its own, walk parent_branch so status
    // diffs against the parent's manifest rather than an empty one.
    let branch_name = repo.get_current_branch_name().ok().flatten();
    let head = match &branch_name {
        Some(name) => resolve_effective_head(repo, name)?,
        None => repo.get_head()?,
    };

    // Get old manifest for comparison
    let old_manifest = if let Some(ref head_hash) = head {
        // Try loading as a commit first (new model)
        if let Some(head_commit) = repo.get_commit(head_hash)? {
            repo.get_manifest(&head_commit.manifest_hash)?
                .unwrap_or_else(Manifest::empty)
        } else {
            // No commit found -- no manifest to compare against
            Manifest::empty()
        }
    } else {
        Manifest::empty()
    };

    // When a scope is active, the diff baseline is the parent manifest
    // restricted to in-scope entries (matches how commit will store it).
    let diff_old = if prefixes.is_empty() {
        old_manifest
    } else {
        oak_core::filter_manifest_by_prefixes(&old_manifest, &prefixes)
    };

    let new_manifest = Manifest::new(entries);

    // Compute file changes. Same two-pass logic as commit: exact-hash
    // renames here, then content-similarity to catch rename-with-edit.
    let mut changes = diff_old.diff(&new_manifest);
    detect_renames_by_content(
        &mut changes,
        |h| Ok(repo.get_blob(h)?.map(|b| b.content)),
        &RenameConfig::default(),
    )?;

    Ok((changes, head, branch_name))
}
