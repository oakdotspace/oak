//! `oak mount` — mount a remote repository as a virtual filesystem.
//!
//! See `crates/oak-cli/src/commands/mount/fs.rs` for the FUSE layer and
//! `state.rs` for the on-disk layout.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use chrono::Utc;
use oak_core::{Blob, FileChange, FileMode, Hash, Manifest, ManifestEntry, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::output;

// Platform-specific virtual filesystem backends. Each provides a
// `start_virtualizing` / `stop_virtualizing` pair that the shared `start()`
// function dispatches into. Both wear the same overlay/state contract, so
// the rest of the mount module is platform-neutral.
#[cfg(all(feature = "mount", any(target_os = "macos", target_os = "linux")))]
pub mod fuse_fs;
#[cfg(all(feature = "mount", target_os = "windows"))]
pub mod projfs_fs;
pub mod pull;
pub mod remote;
pub mod state;
pub mod worktree;

use state::{
    cache_db_path, load_config, load_index, load_overlay_meta, load_sync_state, lookup_id_for,
    overlay_dir, save_overlay_meta, state_dir_for, unregister_mount, MountConfig, OverlayMeta,
};

/// Token / API-key resolution that mirrors the rest of the CLI.
pub(super) fn token_for(remote: &str) -> Option<String> {
    std::env::var("OAK_API_KEY")
        .ok()
        .or_else(|| super::credentials::get_token_for_server(remote))
}

/// Generate the short id used in the virtual branch name.
#[cfg(feature = "mount")]
fn short_id(id: &str) -> &str {
    let end = id.len().min(8);
    &id[..end]
}

/// Derive a human-readable slug from the mount destination's leaf
/// directory name. Used as the virtual branch name prefix and as the
/// default branch description, so the web UI shows something like
/// `improve-readme--75971610` instead of `main--mount-75971610` and
/// the squash-merge commit message defaults to `improve-readme`
/// instead of the opaque branch name.
///
/// Lowercases, replaces non-`[a-z0-9-_]` runs with a single `-`,
/// strips leading/trailing dashes. Falls back to `"mount"` if the
/// resulting slug is empty (e.g. dest is `/`, `.`, or all-symbol).
#[cfg(feature = "mount")]
fn dest_slug(dest: &Path) -> String {
    let leaf = dest
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let cleaned: String = leaf
        .chars()
        .map(|c| match c {
            'A'..='Z' => c.to_ascii_lowercase(),
            'a'..='z' | '0'..='9' | '-' | '_' => c,
            _ => '-',
        })
        .collect();
    let collapsed = cleaned
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.is_empty() {
        "mount".to_string()
    } else {
        collapsed
    }
}

/// Look up the mount config from a destination directory; errors with a
/// helpful message if the destination isn't a known mount.
pub(super) fn config_for_dest(dest: &Path) -> Result<(MountConfig, std::path::PathBuf)> {
    let id = lookup_id_for(dest)?.ok_or_else(|| {
        OakError::Server(format!(
            "no mount registered for '{}'. Run `oak mount start <organization>/<repo> <dest>` first.",
            dest.display()
        ))
    })?;
    let state_dir = state_dir_for(&id)?;
    let cfg = load_config(&state_dir)?;
    Ok((cfg, state_dir))
}

/// Build the manifest representing the user's current state in a mount —
/// base manifest, plus overlay edits, minus deletions, with renames applied.
pub(super) fn build_committed_manifest(
    base_manifest: &Manifest,
    overlay: &OverlayMeta,
    dirty_blobs: &HashMap<String, Hash>,
) -> Manifest {
    let deleted: std::collections::HashSet<&str> =
        overlay.deletions.iter().map(|s| s.as_str()).collect();

    let mut entries: Vec<ManifestEntry> = Vec::new();
    for entry in &base_manifest.entries {
        if deleted.contains(entry.path.as_str()) {
            continue;
        }
        // If the path was renamed, the entry should appear under the new path
        // unless the new path is also dirty (in which case the dirty entry
        // wins below).
        let new_path = overlay
            .renames
            .get(&entry.path)
            .cloned()
            .unwrap_or_else(|| entry.path.clone());
        if overlay.dirty.contains_key(&new_path) {
            // The dirty entry below will own this path.
            continue;
        }
        entries.push(ManifestEntry {
            path: new_path,
            blob_hash: entry.blob_hash.clone(),
            mode: entry.mode,
        });
    }

    for (path, dirty) in &overlay.dirty {
        let Some(hash) = dirty_blobs.get(path) else {
            continue;
        };
        let mode = match dirty.mode.as_str() {
            "executable" => FileMode::Executable,
            "symlink" => FileMode::Symlink,
            _ => FileMode::Regular,
        };
        entries.push(ManifestEntry {
            path: path.clone(),
            blob_hash: hash.clone(),
            mode,
        });
    }

    Manifest::new(entries)
}

// ---------------------------------------------------------------------------
// Mount-context detection (for top-level commands like `oak commit/status/log`)
// ---------------------------------------------------------------------------

/// If `path` (or any ancestor) is a registered mount point, return the
/// canonical mount-point path. Lets top-level commands ask "am I inside a
/// mount?" without poking at FUSE internals.
///
/// The mount index records absolute, canonicalized paths. We canonicalize the
/// caller's path the same way and walk up from there; ancestor lookup means
/// `oak commit` works from any subdirectory of the mount, mirroring how
/// `oak commit` works inside a regular repo's nested directories.
pub fn mount_dest_for(path: &Path) -> Result<Option<std::path::PathBuf>> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut current: &Path = &canonical;
    loop {
        if let Some(_id) = state::lookup_id_for(current)? {
            return Ok(Some(current.to_path_buf()));
        }
        match current.parent() {
            Some(p) if p != current => current = p,
            _ => return Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Read-only views of mount state for top-level commands
// ---------------------------------------------------------------------------

/// `oak log` inside a mount: print commits on the virtual branch, plus a
/// "(active commit)" line when the overlay has uncommitted edits to surface
/// the lazy-amend model in the log output.
pub fn log(dest: &Path, limit: Option<usize>) -> Result<()> {
    let (cfg, state_dir) = config_for_dest(dest)?;
    let cache = SqliteRepository::open_relaxed(&cache_db_path(&state_dir))?;
    let overlay = load_overlay_meta(&state_dir)?;
    let dirty =
        !overlay.dirty.is_empty() || !overlay.deletions.is_empty() || !overlay.renames.is_empty();

    if dirty {
        // Synthesize a row for the active (uncommitted) commit so users can
        // see they're working "on top of" the latest persisted commit. The
        // hash placeholder makes it clear this is in-flight, not a stored
        // commit.
        output::info(&format!(
            "(active) [overlay] dirty: {} modified, {} deleted, {} renamed — run `oak commit` to finalize",
            overlay.dirty.len(),
            overlay.deletions.len(),
            overlay.renames.len(),
        ));
    }

    let commits = cache.get_commits_for_branch(&cfg.virtual_branch)?;
    let limit = limit.unwrap_or(commits.len());
    for commit in commits.iter().rev().take(limit) {
        println!(
            "{} {} ({}){}",
            commit.hash.short(),
            commit.timestamp.format("%Y-%m-%d %H:%M:%S"),
            commit.author,
            commit
                .message
                .as_deref()
                .map(|m| format!(" {m}"))
                .unwrap_or_default(),
        );
    }
    Ok(())
}

/// `oak hash` inside a mount: print the current head of the virtual branch.
pub fn hash(dest: &Path) -> Result<()> {
    let (cfg, state_dir) = config_for_dest(dest)?;
    let cache = SqliteRepository::open_relaxed(&cache_db_path(&state_dir))?;
    match cache.get_branch_head(&cfg.virtual_branch)? {
        Some(h) => {
            println!("{}", h.as_str());
            Ok(())
        }
        None => {
            output::info("Virtual branch has no head yet.");
            Ok(())
        }
    }
}

/// `oak diff` inside a mount: render per-file unified hunks of the active
/// commit (the overlay) against the virtual-branch head, in the exact same
/// format as the regular `oak diff` so tools and agents can parse both the
/// same way.
///
/// The change set is computed identically to `oak commit` / `oak status`
/// (build the overlay-applied manifest, diff it against the parent), so the
/// three commands always agree on *which* files changed. Rendering reuses
/// [`super::diff::render_change`]: the pre-image comes from the local cache
/// (the parent manifest's blobs) and the post-image is read back through the
/// live mount point, where the FUSE layer serves current overlay content.
pub fn diff(dest: &Path) -> Result<()> {
    let (cfg, state_dir) = config_for_dest(dest)?;
    let overlay = load_overlay_meta(&state_dir)?;

    if overlay.dirty.is_empty() && overlay.deletions.is_empty() && overlay.renames.is_empty() {
        output::info("No differences");
        return Ok(());
    }

    let cache = SqliteRepository::open_relaxed(&cache_db_path(&state_dir))?;
    let parent_hash = cache
        .get_branch_head(&cfg.virtual_branch)?
        .ok_or_else(|| OakError::Server("virtual branch has no head".into()))?;
    let parent_commit = cache
        .get_commit(&parent_hash)?
        .ok_or_else(|| OakError::Server("virtual branch head commit missing".into()))?;
    let parent_manifest = cache
        .get_manifest(&parent_commit.manifest_hash)?
        .ok_or_else(|| OakError::Server("parent manifest missing".into()))?;

    // Hash (but don't store) every dirty file so we can build the same
    // overlay-applied manifest `oak commit` would, and diff it against the
    // parent. Mirrors the read in `commit`: FUSE keeps dirty content in the
    // flat overlay dir; ProjFS persists in place in the mount tree.
    let overlay_root = overlay_dir(&state_dir);
    let mut dirty_blobs: HashMap<String, Hash> = HashMap::new();
    for (path, dirty) in &overlay.dirty {
        let read_from = if dirty.in_place {
            cfg.mount_point.join(path)
        } else {
            overlay_root.join(&dirty.overlay_file)
        };
        let content = fs::read(&read_from).map_err(OakError::Io)?;
        dirty_blobs.insert(path.clone(), Blob::new(content).hash);
    }

    let new_manifest = build_committed_manifest(&parent_manifest, &overlay, &dirty_blobs);
    let changes = parent_manifest.diff(&new_manifest);
    if changes.is_empty() {
        output::info("No differences");
        return Ok(());
    }

    // Pre-image lookup table, same shape the regular diff builds from HEAD.
    let head_files: HashMap<&str, &ManifestEntry> = parent_manifest
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();

    let mut formatted = String::new();
    for change in &changes {
        formatted.push_str(&super::diff::render_change(
            &cache,
            &cfg.mount_point,
            &head_files,
            change,
        )?);
        formatted.push('\n');
    }

    super::diff::print_formatted(&formatted);
    Ok(())
}

// ---------------------------------------------------------------------------
// `oak mount <spec> <branch>` — agent-space shorthand
// ---------------------------------------------------------------------------

/// Resolve a space directory for the shorthand `oak mount <spec> <branch>`.
///
/// Default is `<cwd>/<repo-leaf>`. Empty directories and already-scaffolded
/// Oak spaces are reusable. If that path is unrelated and non-empty we fall
/// back to `<cwd>/<repo-leaf>-<branch>` to avoid silently taking over
/// unrelated work.
pub fn shorthand_space(cwd: &Path, repo_leaf: &str, branch: &str) -> Result<std::path::PathBuf> {
    fn is_empty_dir(path: &Path) -> bool {
        if !path.exists() {
            return true;
        }
        match fs::read_dir(path) {
            Ok(mut iter) => iter.next().is_none(),
            Err(_) => false,
        }
    }
    fn is_existing_space(path: &Path) -> bool {
        path.is_dir() && path.join("CLAUDE.md").exists() && path.join(".claude").is_dir()
    }

    let primary = cwd.join(repo_leaf);
    if is_empty_dir(&primary) || is_existing_space(&primary) {
        return Ok(primary);
    }
    let branch_slug = branch_path_slug(branch);
    let suffixed = cwd.join(format!("{repo_leaf}-{branch_slug}"));
    if is_empty_dir(&suffixed) || is_existing_space(&suffixed) {
        return Ok(suffixed);
    }
    Err(OakError::Server(format!(
        "default space directories '{}' and '{}' are both occupied; pass an explicit dest via `oak mount start`",
        primary.display(),
        suffixed.display()
    )))
}

fn branch_path_slug(branch: &str) -> String {
    branch
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            _ => c,
        })
        .collect()
}

#[cfg(feature = "mount")]
pub async fn shorthand_mount(
    remote: &str,
    spec: &str,
    branch: Option<&str>,
    team: Option<&str>,
    project: Option<&str>,
    cwd: &Path,
) -> Result<()> {
    let (owner, repo_leaf) = super::parse_owner_repo(spec)?;
    // When no branch is given, resolve the repo's default head branch up
    // front so we can label the dest path with it (matching the explicit
    // form) and pass a concrete branch name to `start`.
    let resolved_branch = match branch {
        Some(b) => b.to_string(),
        None => {
            let token = token_for(remote);
            output::info(&format!(
                "Resolving default branch for '{}/{}' on {}...",
                owner, repo_leaf, remote
            ));
            let (name, _) =
                remote::resolve_branch_head(remote, &owner, &repo_leaf, None, token.as_deref())
                    .await?;
            name
        }
    };
    let space = shorthand_space(cwd, &repo_leaf, &resolved_branch)?;
    let wrote = super::spaces::ensure(&owner, &repo_leaf, &space)?;
    if wrote.is_empty() {
        output::info(&format!("Using Oak space at {}", space.display()));
    } else {
        output::success(&format!(
            "Set up Oak space for {}/{} at {}",
            owner,
            repo_leaf,
            space.display()
        ));
        for path in wrote {
            output::item(&format!("wrote {}", path.display()));
        }
    }

    let dest = space.join(branch_path_slug(&resolved_branch));
    output::info(&format!("Mounting at {}", dest.display()));
    start(remote, spec, &dest, Some(&resolved_branch), team, project).await
}

// ---------------------------------------------------------------------------
// `oak mount start`
// ---------------------------------------------------------------------------

#[cfg(feature = "mount")]
pub async fn start(
    remote: &str,
    spec: &str,
    dest: &Path,
    branch: Option<&str>,
    team: Option<&str>,
    project: Option<&str>,
) -> Result<()> {
    use oak_core::{Branch, BranchStatus, MetadataKey};
    use state::{register_mount, save_config};
    use std::sync::Arc;

    if team.is_some() && project.is_some() {
        return Err(OakError::Server(
            "--team and --project are mutually exclusive".to_string(),
        ));
    }

    let (owner, name) = super::parse_owner_repo(spec)?;
    let token = token_for(remote);

    // Verify dest is empty or doesn't exist.
    if dest.exists() {
        let meta = fs::metadata(dest)?;
        if !meta.is_dir() {
            return Err(OakError::Io(std::io::Error::other(format!(
                "mount destination '{}' is not a directory",
                dest.display()
            ))));
        }
        if fs::read_dir(dest)?.next().is_some() {
            return Err(OakError::Io(std::io::Error::other(format!(
                "mount destination '{}' is not empty",
                dest.display()
            ))));
        }
    } else {
        fs::create_dir_all(dest)?;
    }

    // Reject a remount over an existing mount registration.
    if let Some(existing) = lookup_id_for(dest)? {
        return Err(OakError::Server(format!(
            "mount '{}' already exists (id={}). Run `oak mount forget` first or pick a new destination.",
            dest.display(),
            existing
        )));
    }

    output::info(&format!(
        "Resolving HEAD for '{}/{}' on {}...",
        owner, name, remote
    ));
    // Flat branching model: a mount's writable virtual branch is always
    // parented onto the trunk. Mounting another branch to stack new work on top
    // of it is exactly the relationship we forbid, so reject an explicit
    // `-b <branch>` that names anything but the trunk. (Omitting `-b` mounts the
    // repo's default branch, which is the trunk.)
    if let Some(requested) = branch {
        if requested != oak_core::DEFAULT_BRANCH {
            return Err(OakError::InvalidArgument(format!(
                "mounts must be based on the trunk ('{trunk}'); mounting another \
                 branch ('{requested}') to build on top of it isn't supported — \
                 Oak uses a flat branch-per-task model where every branch merges \
                 back into '{trunk}'. Mount '{trunk}' and start a fresh task instead.",
                trunk = oak_core::DEFAULT_BRANCH,
            )));
        }
    }

    let (base_branch, base_commit) =
        remote::resolve_branch_head(remote, &owner, &name, branch, token.as_deref()).await?;

    // Allocate a mount id + state dir.
    let id = uuid::Uuid::new_v4().simple().to_string();
    let state_dir = state_dir_for(&id)?;
    fs::create_dir_all(&state_dir)?;
    fs::create_dir_all(overlay_dir(&state_dir))?;

    let cache = Arc::new(SqliteRepository::open_relaxed(&cache_db_path(&state_dir))?);
    cache.set_metadata(MetadataKey::RemoteUrl, remote)?;
    cache.set_metadata(MetadataKey::RepoOwner, &owner)?;
    cache.set_metadata(MetadataKey::RepoName, &name)?;

    // Persist the source branch first — `store_commit` enforces a FK on
    // `commits.branch_name → branches.name`, so the branch row must exist
    // before we can fold the commit into the cache.
    let base_branch_obj = Branch {
        name: base_branch.clone(),
        description: None,
        parent_branch: None,
        status: BranchStatus::Open,
        created_at: Utc::now(),
    };
    cache.store_branch(&base_branch_obj)?;

    // Fetch the head commit + manifest into the cache (no blobs yet).
    output::info("Fetching head commit + manifest...");
    let head_hash = Hash(base_commit.clone());
    remote::fetch_commit_into_cache(
        cache.as_ref(),
        remote,
        &owner,
        &name,
        &head_hash,
        token.as_deref(),
    )
    .await?;

    cache.set_branch_head(&base_branch, &head_hash)?;

    // Virtual branch — parented onto the trunk (never the mounted base, which
    // is guaranteed to be the trunk above anyway), pointing at the base commit
    // until the user makes their first commit in the mount. Name is
    // `<dest-slug>--<id8>` so the web UI shows something tied to the task (the
    // mount dir's leaf) instead of the opaque mount id. The description defaults
    // to the slug too — that becomes the squash-merge commit message if the
    // user doesn't edit it post-push.
    let slug = dest_slug(dest);
    let virtual_branch = format!("{}--{}", slug, short_id(&id));
    let v_branch = Branch::new(
        virtual_branch.clone(),
        Some(slug.clone()),
        Some(oak_core::DEFAULT_BRANCH.to_string()),
    );
    cache.store_branch(&v_branch)?;
    cache.set_branch_head(&virtual_branch, &head_hash)?;
    cache.set_current_branch(&virtual_branch)?;
    cache.set_head(&head_hash)?;

    // Resolve the team/project scope to a list of path_prefixes by asking
    // the server. Empty list = whole-repo mount.
    let prefixes = if team.is_some() || project.is_some() {
        remote::resolve_scope_prefixes(remote, &owner, &name, team, project, token.as_deref())
            .await?
    } else {
        Vec::new()
    };

    // Save mount config + register.
    let cfg = MountConfig {
        id: id.clone(),
        mount_point: dest.to_path_buf(),
        remote_url: remote.to_string(),
        owner: owner.clone(),
        repo: name.clone(),
        base_branch: base_branch.clone(),
        base_commit: base_commit.clone(),
        virtual_branch: virtual_branch.clone(),
        team: team.map(|s| s.to_string()),
        project: project.map(|s| s.to_string()),
        path_prefixes: prefixes.clone(),
    };
    save_config(&state_dir, &cfg)?;
    register_mount(dest, &id)?;

    // Load the head commit + its manifest. We need the commit's timestamp
    // (not just the manifest) so the FUSE layer can report a stable mtime
    // for clean files — otherwise editors like vim see mtime drift between
    // open and write and complain that the file changed under them.
    let head_commit_obj = cache
        .get_commit(&head_hash)?
        .ok_or_else(|| OakError::Server("head commit missing after fetch".into()))?;
    let base_mtime: std::time::SystemTime = head_commit_obj.timestamp.into();
    let manifest = cache
        .get_manifest(&head_commit_obj.manifest_hash)?
        .ok_or_else(|| OakError::Server("manifest missing after fetch".into()))?;

    // With a project scope active, skip stat requests for out-of-scope
    // paths. On a large monorepo plus a narrow scope this is the
    // difference between O(repo) and O(scope) HTTP work at mount time.
    let unique_hashes: Vec<String> = {
        let mut set = std::collections::HashSet::new();
        for e in &manifest.entries {
            if !prefixes.is_empty() && !oak_core::path_in_any_prefix(&prefixes, &e.path) {
                continue;
            }
            set.insert(e.blob_hash.as_str().to_string());
        }
        set.into_iter().collect()
    };
    output::info(&format!(
        "Fetching metadata for {} blob(s) (sizes only, no content)...",
        unique_hashes.len()
    ));
    let sizes_vec =
        remote::fetch_blob_sizes(remote, &owner, &name, &unique_hashes, token.as_deref()).await?;
    let sizes: HashMap<String, u64> = sizes_vec.into_iter().collect();

    // Hand off to the FUSE layer (blocks until unmount).
    output::success(&format!(
        "Mounting {}/{}@{} at {} (virtual branch {}). Press Ctrl-C to unmount.",
        owner,
        name,
        &base_commit[..12.min(base_commit.len())],
        dest.display(),
        virtual_branch,
    ));
    output::info(&format!("  state dir: {}", state_dir.display()));

    // The scope prefixes were resolved earlier (before blob-size prefetch)
    // so we could filter the stat list. Hand them to the platform-specific
    // virtual filesystem layer below — each backend takes the same prepared
    // state (cache, manifest, sizes, prefixes) and runs its own event loop
    // until the user signals shutdown.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let rt_handle = tokio::runtime::Handle::current();
        let mfs = fuse_fs::MountFs::new(
            cfg.clone(),
            cache,
            &manifest,
            &sizes,
            token,
            rt_handle,
            state_dir.clone(),
            &prefixes,
            base_mtime,
        )?;

        // `mount2` blocks the caller's thread; spawn it on a blocking task so
        // the tokio runtime can keep servicing async fetches inside the FUSE
        // callbacks.
        let dest_owned = dest.to_path_buf();
        let mount_result = tokio::task::spawn_blocking(move || fuse_fs::mount_fs(&dest_owned, mfs))
            .await
            .map_err(|e| {
                OakError::Io(std::io::Error::other(format!("mount task panicked: {}", e)))
            })?;

        if let Err(e) = mount_result {
            // Best-effort cleanup of the registration so a stale entry doesn't
            // prevent re-mounting.
            let _ = unregister_mount(dest);
            return Err(e);
        }
    }

    #[cfg(target_os = "windows")]
    {
        // ProjFS doesn't block — it spins up its own threadpool to service
        // callbacks. We start it, then park here until Ctrl-C, then stop.
        let rt_handle = tokio::runtime::Handle::current();
        let pfs = match projfs_fs::ProjFsMount::start(
            cfg.clone(),
            cache,
            &manifest,
            &sizes,
            token,
            rt_handle,
            state_dir.clone(),
            prefixes.clone(),
        ) {
            Ok(p) => p,
            Err(e) => {
                let _ = unregister_mount(dest);
                return Err(e);
            }
        };

        if let Err(e) = tokio::signal::ctrl_c().await {
            output::warning(&format!("ctrl-c handler failed: {e}"));
        }
        // stop() is best-effort — even if it errors we want to keep the
        // registration so the user can retry `mount forget` cleanly.
        if let Err(e) = pfs.stop() {
            output::warning(&format!("stopping ProjFS failed: {e}"));
        }
    }

    output::info("Unmounted.");
    Ok(())
}

// ---------------------------------------------------------------------------
// `oak mount list`
// ---------------------------------------------------------------------------

pub fn list() -> Result<()> {
    let idx = load_index()?;
    if idx.mounts.is_empty() {
        output::info("No active mounts.");
        return Ok(());
    }
    output::info("Active mounts:");
    println!();
    for (mount_point, id) in &idx.mounts {
        let state_dir = state_dir_for(id)?;
        let cfg = load_config(&state_dir).ok();
        match cfg {
            Some(cfg) => {
                println!(
                    "  {}\n    -> {}/{}@{} (virtual branch: {})",
                    mount_point,
                    cfg.owner,
                    cfg.repo,
                    &cfg.base_commit[..12.min(cfg.base_commit.len())],
                    cfg.virtual_branch,
                );
            }
            None => {
                println!("  {mount_point} (id={id}, config missing)");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `oak mount status`
// ---------------------------------------------------------------------------

pub fn status(dest: &Path) -> Result<()> {
    let (cfg, state_dir) = config_for_dest(dest)?;
    let overlay = load_overlay_meta(&state_dir)?;
    output::info(&format!(
        "Mount: {}/{}@{} (virtual branch {})",
        cfg.owner,
        cfg.repo,
        &cfg.base_commit[..12.min(cfg.base_commit.len())],
        cfg.virtual_branch,
    ));
    println!();

    if overlay.dirty.is_empty() && overlay.deletions.is_empty() && overlay.renames.is_empty() {
        output::info("Working tree clean.");
        return Ok(());
    }

    if !overlay.dirty.is_empty() {
        output::info("Modified / created files:");
        let mut paths: Vec<&String> = overlay.dirty.keys().collect();
        paths.sort();
        for p in paths {
            println!("  M {p}");
        }
    }
    if !overlay.deletions.is_empty() {
        output::info("Deleted files:");
        let mut paths = overlay.deletions.clone();
        paths.sort();
        for p in &paths {
            println!("  D {p}");
        }
    }
    if !overlay.renames.is_empty() {
        output::info("Renames:");
        let mut renames: Vec<(&String, &String)> = overlay.renames.iter().collect();
        renames.sort();
        for (old, new) in renames {
            println!("  R {old} -> {new}");
        }
    }
    Ok(())
}

/// Lightweight status for `oak prompt`: the virtual-branch name plus change
/// counts read straight from the overlay metadata — no working-tree scan, so
/// it's cheap enough to run on every prompt render. Returns
/// `(branch, added, modified, deleted, renamed)`. `added` is always 0: the
/// overlay folds created and modified files into one `dirty` map, so both
/// count as modified here.
pub fn prompt_counts(dest: &Path) -> Result<(String, usize, usize, usize, usize)> {
    let (cfg, state_dir) = config_for_dest(dest)?;
    let overlay = load_overlay_meta(&state_dir)?;
    Ok((
        cfg.virtual_branch,
        0,
        overlay.dirty.len(),
        overlay.deletions.len(),
        overlay.renames.len(),
    ))
}

// ---------------------------------------------------------------------------
// `oak commit` inside a mount
// ---------------------------------------------------------------------------

pub fn commit(dest: &Path) -> Result<()> {
    let (cfg, state_dir) = config_for_dest(dest)?;

    // A pull mid conflict-resolution owns the overlay; committing it directly
    // would drop the `merge_parent` linkage (and could land conflict markers).
    if load_sync_state(&state_dir)?.is_some() {
        return Err(OakError::Server(
            "a pull is in progress — resolve the conflicts and run `oak pull --continue`, \
             or `oak pull --abort` to discard it, before committing."
                .into(),
        ));
    }

    let overlay = load_overlay_meta(&state_dir)?;

    if overlay.dirty.is_empty() && overlay.deletions.is_empty() && overlay.renames.is_empty() {
        output::info("Nothing to commit, mount is clean.");
        return Ok(());
    }

    let cache = SqliteRepository::open_relaxed(&cache_db_path(&state_dir))?;

    // Resolve the current head of the virtual branch (which is what we'll
    // build the new commit on top of).
    let parent_hash = cache
        .get_branch_head(&cfg.virtual_branch)?
        .ok_or_else(|| OakError::Server("virtual branch has no head".into()))?;
    let parent_commit = cache
        .get_commit(&parent_hash)?
        .ok_or_else(|| OakError::Server("virtual branch head commit missing".into()))?;
    let parent_manifest = cache
        .get_manifest(&parent_commit.manifest_hash)?
        .ok_or_else(|| OakError::Server("parent manifest missing".into()))?;

    // Hash + store every dirty file as a blob. Two storage modes:
    //   - FUSE backend writes dirty content to a flat overlay dir under the
    //     mount state, so `dirty.overlay_file` names a file there.
    //   - ProjFS persists modifications in-place in the mount tree (it
    //     hydrates placeholders to full files on disk), so `dirty.in_place`
    //     is set and we read directly from `<mount_point>/<path>` to avoid
    //     copying potentially-large binary assets twice.
    let overlay_root = overlay_dir(&state_dir);
    let mut dirty_blobs: HashMap<String, Hash> = HashMap::new();
    for (path, dirty) in &overlay.dirty {
        let read_from = if dirty.in_place {
            cfg.mount_point.join(path)
        } else {
            overlay_root.join(&dirty.overlay_file)
        };
        let content = fs::read(&read_from).map_err(OakError::Io)?;
        let blob = Blob::new(content);
        cache.store_blob(&blob)?;
        dirty_blobs.insert(path.clone(), blob.hash);
    }

    let new_manifest = build_committed_manifest(&parent_manifest, &overlay, &dirty_blobs);
    let changes = parent_manifest.diff(&new_manifest);
    if changes.is_empty() {
        output::info("Nothing to commit, working tree matches the virtual branch head.");
        return Ok(());
    }

    let manifest_hash = cache.put_manifest(new_manifest.entries.clone())?;

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

    let commit_hash = cache.put_commit(
        cfg.virtual_branch.clone(),
        Some(parent_hash),
        None,
        manifest_hash,
        super::commit::get_author(),
        None,
        Utc::now(),
        files,
    )?;
    cache.set_branch_head(&cfg.virtual_branch, &commit_hash)?;
    cache.set_head(&commit_hash)?;

    // Clear the overlay now that the changes are folded into the branch.
    save_overlay_meta(&state_dir, &OverlayMeta::default())?;
    // Best-effort: wipe overlay files so the next read flows from the cache.
    if let Ok(rd) = fs::read_dir(&overlay_root) {
        for ent in rd.flatten() {
            let _ = fs::remove_file(ent.path());
        }
    }

    output::success(&format!(
        "Created commit {} on virtual branch '{}'",
        commit_hash.short(),
        cfg.virtual_branch
    ));
    output::info("(Run `oak push` to publish, or remount the dest to see the change reflected.)");

    Ok(())
}

// ---------------------------------------------------------------------------
// `oak mount push`
// ---------------------------------------------------------------------------

pub async fn push(dest: &Path) -> Result<()> {
    let (cfg, state_dir) = config_for_dest(dest)?;
    if load_sync_state(&state_dir)?.is_some() {
        return Err(OakError::Server(
            "a pull is in progress — run `oak pull --continue` (after resolving conflicts) \
             or `oak pull --abort` before pushing."
                .into(),
        ));
    }
    let overlay = load_overlay_meta(&state_dir)?;
    if !overlay.dirty.is_empty() || !overlay.deletions.is_empty() || !overlay.renames.is_empty() {
        return Err(OakError::Server(
            "uncommitted changes in mount overlay — run `oak commit` inside the mount first".into(),
        ));
    }

    let cache = SqliteRepository::open_relaxed(&cache_db_path(&state_dir))?;
    let token = token_for(&cfg.remote_url);

    output::info(&format!(
        "Pushing virtual branch '{}' to {}/{}...",
        cfg.virtual_branch, cfg.owner, cfg.repo
    ));

    super::push::push_async(
        &cache,
        dest,
        &cfg.remote_url,
        &cfg.owner,
        &cfg.repo,
        Some(&cfg.virtual_branch),
        false,
        token.as_deref(),
    )
    .await
}

// ---------------------------------------------------------------------------
// `oak desc` inside a mount
// ---------------------------------------------------------------------------

/// Set the virtual branch's description from inside a mount.
///
/// Writes to the mount's cache db so the next `oak push` from the mount
/// carries the new description in its `BranchPushData` payload — and if the
/// branch already exists on the server (i.e. the user already pushed once),
/// also sends a metadata-only push immediately so the server picks it up
/// without requiring a second `oak push`. The server's push handler accepts
/// pushes with no new commits and treats the branch metadata as an update
/// (see `crates/oak-server/src/api/repos.rs` push handler).
///
/// This is what `Commands::Desc` routes to when the cwd is inside a mount;
/// the regular `edit_current_branch` path would error with
/// "Repository not found" because the mount tree has no `.oak/` directory.
pub async fn desc(dest: &Path, description: &str) -> Result<()> {
    let (cfg, state_dir) = config_for_dest(dest)?;
    let cache = SqliteRepository::open_relaxed(&cache_db_path(&state_dir))?;

    // The virtual branch is seeded into the cache db at mount-start time
    // (see `start()`), so the row exists — `update_branch_description` is
    // just a row update.
    cache.update_branch_description(&cfg.virtual_branch, description)?;
    output::success(&format!(
        "Updated description for virtual branch '{}'",
        cfg.virtual_branch
    ));

    // Best-effort metadata-only push so the description lands on the server
    // immediately. We use the dedicated `push_branch_metadata` helper rather
    // than the full `push_async` for two reasons: (a) `push_async`
    // short-circuits with "Already up to date, nothing to push" before
    // sending branch metadata when there are no new commits, so a desc-only
    // update over a clean branch would never reach the server, and (b) we
    // don't want `oak desc` to silently push unpushed commits as a side
    // effect — that's `oak push`'s job. The helper sends just the
    // `BranchPushData`; the server's push handler upserts the branch row
    // regardless of whether commits accompany it. We surface a hint instead
    // of erroring on network / auth failures so `oak desc` still succeeds
    // locally and the next explicit `oak push` can sync.
    let token = token_for(&cfg.remote_url);
    output::info("Syncing description to server...");
    match super::push::push_branch_metadata(
        &cache,
        &cfg.remote_url,
        &cfg.owner,
        &cfg.repo,
        &cfg.virtual_branch,
        token.as_deref(),
    )
    .await
    {
        Ok(()) => {
            output::success("Description synced to server");
            Ok(())
        }
        Err(e) => {
            output::warning(&format!(
                "Saved locally but couldn't sync to server: {e}. Run `oak push` to retry."
            ));
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// `oak mount forget`
// ---------------------------------------------------------------------------

pub fn forget(dest: &Path) -> Result<()> {
    let id = lookup_id_for(dest)?
        .ok_or_else(|| OakError::Server(format!("no mount registered for '{}'", dest.display())))?;
    let state_dir = state_dir_for(&id)?;

    // Sanity: refuse if there are dirty changes — the user asked the system
    // to forget a mount that still has uncommitted work.
    let overlay = load_overlay_meta(&state_dir).unwrap_or_default();
    if !overlay.dirty.is_empty() || !overlay.deletions.is_empty() || !overlay.renames.is_empty() {
        return Err(OakError::Server(format!(
            "mount has uncommitted changes; commit or discard them before `oak mount forget`. \
             State at {}",
            state_dir.display()
        )));
    }

    if state_dir.exists() {
        fs::remove_dir_all(&state_dir)?;
    }
    unregister_mount(dest)?;
    output::success(&format!("Forgot mount '{}'", dest.display()));
    Ok(())
}

// ---------------------------------------------------------------------------
// `oak mount end` — unmount + forget + remove dest dir in one shot.
//
// The "I'm done with this task" command. Designed for branch-per-task
// agent workflows where each task lives in its own mount subdir and the
// final cleanup should leave nothing behind.
// ---------------------------------------------------------------------------

pub fn end(dest: &Path, force: bool) -> Result<()> {
    let id = lookup_id_for(dest)?
        .ok_or_else(|| OakError::Server(format!("no mount registered for '{}'", dest.display())))?;
    let state_dir = state_dir_for(&id)?;

    // Same dirty check as forget — refuse to end a mount with uncommitted
    // work so the user doesn't silently lose changes. `--force` overrides
    // this for the "yes I want to discard" case.
    let overlay = load_overlay_meta(&state_dir).unwrap_or_default();
    let dirty =
        !overlay.dirty.is_empty() || !overlay.deletions.is_empty() || !overlay.renames.is_empty();
    if dirty && !force {
        return Err(OakError::Server(
            "mount has uncommitted changes; run `oak commit` and `oak push` inside the mount first, \
             or pass `--force` to discard.".to_string()
        ));
    }
    if dirty && force {
        output::warning(&format!(
            "Discarding {} dirty / {} deleted / {} renamed entries in overlay.",
            overlay.dirty.len(),
            overlay.deletions.len(),
            overlay.renames.len(),
        ));
    }

    // Best-effort platform unmount. If this fails we still try to forget +
    // remove, so the user gets useful diagnostics rather than a halfway state.
    if let Err(e) = platform_unmount(dest) {
        output::warning(&format!(
            "Couldn't unmount {}: {e}. Continuing; you may need to unmount manually.",
            dest.display()
        ));
    }

    // Give the kernel a beat to release the mount before we try rm-ing the
    // (now-empty) mount point. macFUSE in particular sometimes lags here.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Inline what forget() does — we want a single combined success message
    // at the end instead of forget()'s "Forgot mount" line plus ours.
    if state_dir.exists() {
        fs::remove_dir_all(&state_dir)?;
    }
    unregister_mount(dest)?;

    if dest.exists() {
        if let Err(e) = fs::remove_dir_all(dest) {
            output::warning(&format!(
                "Could not remove {}: {e}. You may need to remove it manually.",
                dest.display()
            ));
        }
    }

    output::success(&format!("Ended mount '{}'", dest.display()));
    Ok(())
}

/// Unmount a FUSE mount on macOS via the `umount` shell command. Macfuse
/// allows the mounting user to unmount their own mounts, so no sudo
/// needed.
#[cfg(target_os = "macos")]
fn platform_unmount(dest: &Path) -> Result<()> {
    use std::process::Command;
    let output = Command::new("umount")
        .arg(dest)
        .output()
        .map_err(|e| OakError::Io(std::io::Error::other(format!("running umount: {e}"))))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    // "not currently mounted" → treat as success.
    if stderr.contains("not currently mounted") || stderr.contains("Invalid argument") {
        return Ok(());
    }
    // Last resort: diskutil unmount force.
    let force = Command::new("diskutil")
        .args(["unmount", "force"])
        .arg(dest)
        .output()
        .map_err(|e| OakError::Io(std::io::Error::other(format!("running diskutil: {e}"))))?;
    if force.status.success() {
        return Ok(());
    }
    Err(OakError::Server(format!(
        "umount failed: {}",
        stderr.trim()
    )))
}

/// Unmount a FUSE mount on Linux via `fusermount -u`, the user-space FUSE
/// helper. Falls back to `umount` if `fusermount` isn't on PATH.
#[cfg(target_os = "linux")]
fn platform_unmount(dest: &Path) -> Result<()> {
    use std::process::Command;
    let output = Command::new("fusermount").arg("-u").arg(dest).output();
    if let Ok(o) = output {
        if o.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&o.stderr);
        if stderr.contains("not mounted") {
            return Ok(());
        }
    }
    // fusermount missing or failed — try plain umount as a fallback.
    let output = Command::new("umount")
        .arg(dest)
        .output()
        .map_err(|e| OakError::Io(std::io::Error::other(format!("running umount: {e}"))))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not mounted") {
        return Ok(());
    }
    Err(OakError::Server(format!(
        "umount failed: {}",
        stderr.trim()
    )))
}

/// Windows ProjFS has no out-of-process unmount API — virtualization is
/// stopped by the foreground `oak mount start` process calling
/// `PrjStopVirtualizing`. From outside that process we can't reach in,
/// so this is a best-effort no-op. The user is expected to terminate
/// the `oak mount start` process (Ctrl-C) before running `oak mount end`.
#[cfg(target_os = "windows")]
fn platform_unmount(_dest: &Path) -> Result<()> {
    output::info(
        "Windows: skipping unmount step. If the dir is still busy, terminate \
         the `oak mount start` process first, then re-run `oak mount end`.",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::Hash;
    use tempfile::TempDir;

    #[test]
    fn shorthand_space_uses_repo_leaf_when_free() {
        let tmp = TempDir::new().unwrap();
        let dest = shorthand_space(tmp.path(), "myrepo", "main").unwrap();
        assert_eq!(dest, tmp.path().join("myrepo"));
    }

    #[test]
    fn shorthand_space_falls_back_when_repo_leaf_taken() {
        let tmp = TempDir::new().unwrap();
        // Make `<cwd>/myrepo` a non-empty dir.
        let primary = tmp.path().join("myrepo");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::write(primary.join("placeholder"), b"x").unwrap();

        let dest = shorthand_space(tmp.path(), "myrepo", "main").unwrap();
        assert_eq!(dest, tmp.path().join("myrepo-main"));
    }

    #[test]
    fn shorthand_space_accepts_existing_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join("myrepo");
        std::fs::create_dir_all(&primary).unwrap();
        let dest = shorthand_space(tmp.path(), "myrepo", "main").unwrap();
        assert_eq!(dest, primary);
    }

    #[test]
    fn shorthand_space_accepts_existing_agent_space() {
        let tmp = TempDir::new().unwrap();
        let primary = tmp.path().join("myrepo");
        std::fs::create_dir_all(primary.join(".claude")).unwrap();
        std::fs::write(primary.join("CLAUDE.md"), b"user edits").unwrap();

        let dest = shorthand_space(tmp.path(), "myrepo", "main").unwrap();
        assert_eq!(dest, primary);
    }

    #[test]
    fn shorthand_space_sanitizes_branch_slashes() {
        let tmp = TempDir::new().unwrap();
        // Block primary so we exercise the branch-suffix path.
        let primary = tmp.path().join("myrepo");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::write(primary.join("placeholder"), b"x").unwrap();

        let dest = shorthand_space(tmp.path(), "myrepo", "feature/x").unwrap();
        // `/` is replaced with `-` so the leaf stays flat.
        assert_eq!(dest, tmp.path().join("myrepo-feature-x"));
    }

    #[test]
    fn shorthand_space_errors_when_both_taken() {
        let tmp = TempDir::new().unwrap();
        for leaf in &["myrepo", "myrepo-main"] {
            let p = tmp.path().join(leaf);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("placeholder"), b"x").unwrap();
        }
        let err = shorthand_space(tmp.path(), "myrepo", "main").unwrap_err();
        assert!(err.to_string().contains("both occupied"), "{}", err);
    }

    fn entry(path: &str, blob: &str, mode: FileMode) -> ManifestEntry {
        ManifestEntry {
            path: path.into(),
            blob_hash: Hash(blob.into()),
            mode,
        }
    }

    #[test]
    fn build_manifest_clean_overlay_returns_base() {
        let base = Manifest::new(vec![
            entry("a.txt", "h1", FileMode::Regular),
            entry("b.txt", "h2", FileMode::Regular),
        ]);
        let overlay = OverlayMeta::default();
        let dirty = HashMap::new();
        let new = build_committed_manifest(&base, &overlay, &dirty);
        assert_eq!(new.entries.len(), 2);
        assert_eq!(new.entries[0].path, "a.txt");
        assert_eq!(new.entries[1].path, "b.txt");
    }

    #[test]
    fn build_manifest_drops_deleted_paths() {
        let base = Manifest::new(vec![
            entry("a.txt", "h1", FileMode::Regular),
            entry("b.txt", "h2", FileMode::Regular),
        ]);
        let mut overlay = OverlayMeta::default();
        overlay.deletions.push("a.txt".into());
        let dirty = HashMap::new();
        let new = build_committed_manifest(&base, &overlay, &dirty);
        assert_eq!(new.entries.len(), 1);
        assert_eq!(new.entries[0].path, "b.txt");
    }

    #[test]
    fn build_manifest_replaces_dirty_paths() {
        let base = Manifest::new(vec![entry("a.txt", "h1", FileMode::Regular)]);
        let mut overlay = OverlayMeta::default();
        overlay.dirty.insert(
            "a.txt".into(),
            crate::commands::mount::state::DirtyEntry {
                overlay_file: "a.txt".into(),
                mode: "regular".into(),
                in_place: false,
            },
        );
        let mut dirty = HashMap::new();
        dirty.insert("a.txt".into(), Hash("h-new".into()));
        let new = build_committed_manifest(&base, &overlay, &dirty);
        assert_eq!(new.entries.len(), 1);
        assert_eq!(new.entries[0].path, "a.txt");
        assert_eq!(new.entries[0].blob_hash.as_str(), "h-new");
    }

    #[test]
    fn build_manifest_adds_new_files() {
        let base = Manifest::empty();
        let mut overlay = OverlayMeta::default();
        overlay.dirty.insert(
            "src/foo.rs".into(),
            crate::commands::mount::state::DirtyEntry {
                overlay_file: "src__foo.rs".into(),
                mode: "executable".into(),
                in_place: false,
            },
        );
        let mut dirty = HashMap::new();
        dirty.insert("src/foo.rs".into(), Hash("h-foo".into()));
        let new = build_committed_manifest(&base, &overlay, &dirty);
        assert_eq!(new.entries.len(), 1);
        assert_eq!(new.entries[0].path, "src/foo.rs");
        assert_eq!(new.entries[0].mode, FileMode::Executable);
    }

    #[test]
    fn build_manifest_applies_renames() {
        let base = Manifest::new(vec![entry("old.txt", "h1", FileMode::Regular)]);
        let mut overlay = OverlayMeta::default();
        overlay.renames.insert("old.txt".into(), "new.txt".into());
        let dirty = HashMap::new();
        let new = build_committed_manifest(&base, &overlay, &dirty);
        assert_eq!(new.entries.len(), 1);
        assert_eq!(new.entries[0].path, "new.txt");
        assert_eq!(new.entries[0].blob_hash.as_str(), "h1");
    }

    #[test]
    fn build_manifest_dirty_wins_over_rename_target() {
        // User renamed old → new, then overwrote `new`. The overwrite wins:
        // the resulting manifest should have `new` with the new blob.
        let base = Manifest::new(vec![entry("old.txt", "h1", FileMode::Regular)]);
        let mut overlay = OverlayMeta::default();
        overlay.renames.insert("old.txt".into(), "new.txt".into());
        overlay.dirty.insert(
            "new.txt".into(),
            crate::commands::mount::state::DirtyEntry {
                overlay_file: "new.txt".into(),
                mode: "regular".into(),
                in_place: false,
            },
        );
        let mut dirty = HashMap::new();
        dirty.insert("new.txt".into(), Hash("h-new".into()));
        let new = build_committed_manifest(&base, &overlay, &dirty);
        // Single entry at "new.txt" with the new hash — the renamed entry
        // is dropped because the dirty version takes the same path.
        assert_eq!(new.entries.len(), 1);
        assert_eq!(new.entries[0].path, "new.txt");
        assert_eq!(new.entries[0].blob_hash.as_str(), "h-new");
    }
}
