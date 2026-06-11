//! `oak mount` — mount a remote repository as a virtual filesystem.
//!
//! See `crates/oak-cli/src/commands/mount/fs.rs` for the FUSE layer and
//! `state.rs` for the on-disk layout.

use std::collections::HashMap;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use chrono::Utc;
use oak_core::{Blob, FileChange, FileMode, Hash, Manifest, ManifestEntry, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::output;

// Backend-neutral mount engine. Owns the inode tree, overlay, blob hydration,
// and reconciliation. Each platform backend below is a thin adapter over it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod core;

// Platform-specific virtual filesystem backends, all over the same
// `MountCore`, so the rest of this module is platform-neutral:
//   - Linux   → fuser (`fusermount3` helper; no libfuse linked).
//   - macOS   → FSKit extension + daemon IPC (no kernel extension).
//   - Windows → ProjFS.
#[cfg(target_os = "macos")]
pub mod fskit;
#[cfg(target_os = "linux")]
pub mod fuse_fs;
#[cfg(target_os = "windows")]
pub mod projfs_fs;
pub mod pull;
pub mod remote;
pub mod spawn;
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
            "no mount registered for '{}'. Run `oak mount <organization>/<repo>` first.",
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
    // Same windowing as the regular `oak log`: explicit `-n` wins, piped
    // output defaults to a bounded window, a TTY shows everything.
    let piped = !std::io::IsTerminal::is_terminal(&std::io::stdout());
    let limit = match limit {
        Some(n) => n,
        None if piped => super::log::DEFAULT_PIPED_LIMIT,
        None => commits.len(),
    };
    let truncated = commits.len() > limit;
    for commit in commits.iter().rev().take(limit) {
        // Same one-line format as the regular piped `oak log`, so agents can
        // parse both identically.
        output::print_line(&output::format_commit_compact(commit));
    }
    if truncated {
        output::print_line(&output::format_log_more_hint(limit));
    }
    Ok(())
}

/// `oak hash` inside a mount: print the virtual-branch head commit hash.
///
/// This is the latest *finalized* commit on the virtual branch — the active
/// (uncommitted) overlay commit has no hash until `oak commit` materializes it,
/// so a dirty overlay doesn't change the output. Bare stdout line, matching the
/// non-mount `oak hash` so callers can pipe it identically.
pub fn hash(dest: &Path) -> Result<()> {
    let (cfg, state_dir) = config_for_dest(dest)?;
    let cache = SqliteRepository::open_relaxed(&cache_db_path(&state_dir))?;
    match cache.get_branch_head(&cfg.virtual_branch)? {
        Some(h) => {
            println!("{}", h.as_str());
            Ok(())
        }
        None => Err(OakError::NoCommits),
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
pub fn diff(dest: &Path, print: bool, paths: &[PathBuf], stat: bool) -> Result<()> {
    let (cfg, state_dir) = config_for_dest(dest)?;
    let overlay = load_overlay_meta(&state_dir)?;

    if overlay.dirty.is_empty() && overlay.deletions.is_empty() && overlay.renames.is_empty() {
        output::info("No differences");
        return Ok(());
    }

    // User paths are resolved against the mount point (the repo root of a
    // mounted checkout), exactly like the regular diff resolves against the
    // working tree.
    let cwd = std::env::current_dir().map_err(OakError::Io)?;
    let filters = paths
        .iter()
        .map(|p| crate::pathutil::repo_relative_str(&cwd, &cfg.mount_point, &p.to_string_lossy()))
        .collect::<Result<Vec<_>>>()?;

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
    let changes = super::diff::filter_changes(changes, &filters);
    if changes.is_empty() {
        output::info(if filters.is_empty() {
            "No differences"
        } else {
            "No differences in the given paths"
        });
        return Ok(());
    }

    // Pre-image lookup table, same shape the regular diff builds from HEAD.
    let head_files: HashMap<&str, &ManifestEntry> = parent_manifest
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();

    // Same `--stat` summary as the regular diff, sourced from the mount's
    // change set.
    if stat {
        for line in super::diff::render_stat(&cache, &cfg.mount_point, &head_files, &changes)? {
            output::print_line(&line);
        }
        return Ok(());
    }

    // Interactive file-tree browser when attached to a terminal, unless the
    // caller asked for the plain printed diff. Plain non-TTY `oak diff` uses
    // the compact stat contract; explicit `--print` still emits full hunks.
    if !print && std::io::stdout().is_terminal() {
        let entries = super::diff::build_entries(&cache, &cfg.mount_point, &head_files, &changes)?;
        return super::diff::run_tui_entries(entries, &cfg.virtual_branch);
    }
    if !print {
        for line in super::diff::render_stat_with_limit(
            &cache,
            &cfg.mount_point,
            &head_files,
            &changes,
            Some(5),
        )? {
            output::print_line(&line);
        }
        return Ok(());
    }

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
// `oak mount [<owner>[/<repo> [<dest>]]]` — user-facing entry points
//
// Each of these spawns the mount as a detached background daemon (see
// `spawn.rs`) and returns once it's a live mountpoint, handing the terminal
// back. The blocking server itself is `serve()` below, run only via the hidden
// `__serve` subcommand the detached child invokes.
// ---------------------------------------------------------------------------

/// The well-known directory under which `oak mount` (no `owner/repo`) lays out
/// repos, one mountpoint per repo at `~/oaktree/<owner>/<repo>`.
fn oaktree_root() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| OakError::Server("could not determine home directory".into()))?;
    Ok(home.join("oaktree"))
}

/// `oak mount <owner>/<repo> [<dest>]` — mount a single repo at `dest`
/// (defaulting to `<cwd>/<repo>`) as a background daemon.
pub async fn mount_one(remote: &str, spec: &str, dest: Option<PathBuf>, cwd: &Path) -> Result<()> {
    let (_, repo_leaf) = super::parse_owner_repo(spec)?;
    let dest = dest.unwrap_or_else(|| cwd.join(&repo_leaf));
    // "Already mounted" only when something actually serves files there. A
    // registry entry alone can be a leftover from a crash or reboot —
    // `spawn_detached` below respawns or remounts those instead of lying.
    if spawn::plan_spawn(&dest)? == spawn::SpawnPlan::AlreadyLive {
        output::info(&format!("Already mounted at {}", dest.display()));
        return Ok(());
    }
    // On macOS, make sure the signed Oak Mount app is installed before we
    // spawn the (detached) mount daemon — on a fresh Mac this installs it and
    // returns an actionable "enable the extension, then retry" error.
    #[cfg(target_os = "macos")]
    fskit::ensure_mounter_ready().await?;
    output::info(&format!("Mounting {spec} at {}...", dest.display()));
    spawn::spawn_detached(remote, spec, &dest)?;
    output::success(&format!("Mounted {spec} at {}", dest.display()));
    Ok(())
}

/// `oak mount` / `oak mount <owner>` — mount every repo the user can see (or
/// every repo in `owner`'s org when given) under `~/oaktree/<owner>/<repo>`,
/// each as its own background daemon.
pub async fn mount_all(remote: &str, owner: Option<&str>) -> Result<()> {
    let token = token_for(remote);
    output::info(&format!("Listing repositories on {remote}..."));
    let mut repos = remote::list_repos(remote, token.as_deref()).await?;
    if let Some(owner) = owner {
        repos.retain(|(o, _)| o == owner);
        if repos.is_empty() {
            return Err(OakError::Server(format!(
                "no repositories found for '{owner}' on {remote}"
            )));
        }
    }
    if repos.is_empty() {
        output::info("No repositories to mount.");
        return Ok(());
    }

    // One Oak Mount app serves every mount on the machine, so check/install it
    // once up front (macOS only) rather than per-repo.
    #[cfg(target_os = "macos")]
    fskit::ensure_mounter_ready().await?;

    let root = oaktree_root()?;
    output::info(&format!(
        "Mounting {} repo(s) under {}...",
        repos.len(),
        root.display()
    ));

    let mut mounted = 0usize;
    let mut skipped = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();
    for (o, repo) in &repos {
        let dest = root.join(o).join(repo);
        let spec = format!("{o}/{repo}");
        // Skip only live mounts; stale registrations are respawned/remounted
        // by `spawn_detached`.
        if spawn::plan_spawn(&dest)? == spawn::SpawnPlan::AlreadyLive {
            skipped += 1;
            continue;
        }
        match spawn::spawn_detached(remote, &spec, &dest) {
            Ok(()) => {
                mounted += 1;
                output::item(&format!("{spec} -> {}", dest.display()));
            }
            Err(e) => failed.push((spec, e.to_string())),
        }
    }

    output::success(&format!(
        "Mounted {mounted}, skipped {skipped} (already mounted), failed {}.",
        failed.len()
    ));
    for (spec, err) in &failed {
        output::warning(&format!("{spec}: {err}"));
    }
    output::info(&format!("Explore your repos under {}", root.display()));
    Ok(())
}

// ---------------------------------------------------------------------------
// `oak mount __serve` — the blocking foreground server (spawned detached)
// ---------------------------------------------------------------------------

pub async fn serve(remote: &str, spec: &str, dest: &Path, branch: Option<&str>) -> Result<()> {
    use oak_core::{Branch, BranchStatus, MetadataKey};
    use state::{register_mount, save_config};
    use std::sync::Arc;

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
    };
    save_config(&state_dir, &cfg)?;
    register_mount(dest, &id)?;

    // Friendly volume label for Finder. The macOS FSKit extension reads this
    // file from the (security-scoped) state dir and uses it as the volume name,
    // falling back to the opaque `oak-<id>` when it's absent. Use the mount
    // dir's leaf — the folder the user opened — so the volume reads as e.g.
    // `oak` instead of `oak-1c53987…`.
    let volname = dest
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| name.clone());
    let _ = fs::write(state_dir.join("volname"), volname);

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

    let unique_hashes: Vec<String> = {
        let mut set = std::collections::HashSet::new();
        for e in &manifest.entries {
            set.insert(e.blob_hash.as_str().to_string());
        }
        set.into_iter().collect()
    };
    output::info(&format!(
        "Fetching metadata for {} blob(s) (sizes only, no content)...",
        unique_hashes.len()
    ));
    let sizes_vec = remote::fetch_blob_sizes(
        &cache,
        remote,
        &owner,
        &name,
        &unique_hashes,
        token.as_deref(),
    )
    .await?;
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

    run_backend(
        cfg, cache, &manifest, &sizes, token, dest, &state_dir, base_mtime, true,
    )
    .await?;

    output::info("Unmounted.");
    Ok(())
}

/// `oak mount __resume <dest>` — re-serve an existing mount from its intact
/// state dir after the daemon died (crash, reboot, external kill). Reuses the
/// registered id, virtual branch, overlay, and cache, so committed-but-
/// unpushed work and uncommitted overlay edits come back exactly as they
/// were. Spawned detached by the stale-registration recovery in `oak mount`
/// (see [`spawn::plan_spawn`]); not meant to be run by hand.
pub async fn serve_resume(dest: &Path) -> Result<()> {
    use std::sync::Arc;

    let id = lookup_id_for(dest)?
        .ok_or_else(|| OakError::Server(format!("no mount registered for '{}'", dest.display())))?;
    let state_dir = state_dir_for(&id)?;
    let cfg = load_config(&state_dir)?;

    if spawn::is_mountpoint(dest) {
        return Err(OakError::Server(format!(
            "mount at '{}' is already live",
            dest.display()
        )));
    }
    if dest.exists() {
        // Files written into the dead directory never reached the overlay;
        // mounting over them would hide them from both the user and `oak
        // status`, so surface them instead.
        if fs::read_dir(dest)?.next().is_some() {
            return Err(OakError::Server(format!(
                "mount destination '{}' is not empty — these files were written while the \
                 mount was down and mounting over them would hide them. Move them aside, \
                 then re-run `oak mount`.",
                dest.display()
            )));
        }
    } else {
        fs::create_dir_all(dest)?;
    }

    let token = token_for(&cfg.remote_url);
    let cache = Arc::new(SqliteRepository::open_relaxed(&cache_db_path(&state_dir))?);

    let head = cache
        .get_branch_head(&cfg.virtual_branch)?
        .ok_or_else(|| OakError::Server("virtual branch has no head".into()))?;
    let head_commit = cache
        .get_commit(&head)?
        .ok_or_else(|| OakError::Server("virtual branch head commit missing".into()))?;
    let base_mtime: std::time::SystemTime = head_commit.timestamp.into();
    let manifest = cache
        .get_manifest(&head_commit.manifest_hash)?
        .ok_or_else(|| OakError::Server("head manifest missing".into()))?;

    // Rebuild the stat-size table the backend serves `getattr` from, local
    // first: persisted chunk refs, then cached blob content. Only blobs never
    // touched since the original mount need the remote, and a failure there
    // degrades those sizes to 0 instead of blocking the resume — the mount
    // must come back even offline, and reads still hydrate lazily.
    let mut sizes: HashMap<String, u64> = HashMap::new();
    let mut missing: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in &manifest.entries {
        if !seen.insert(entry.blob_hash.as_str()) {
            continue;
        }
        let chunks = cache.get_blob_chunks(&entry.blob_hash)?;
        if let Some(chunks) = chunks.filter(|c| !c.is_empty()) {
            let total = chunks.iter().map(|c| c.length as u64).sum();
            sizes.insert(entry.blob_hash.as_str().to_string(), total);
        } else if let Some(blob) = cache.get_blob(&entry.blob_hash)? {
            sizes.insert(entry.blob_hash.as_str().to_string(), blob.size);
        } else {
            missing.push(entry.blob_hash.as_str().to_string());
        }
    }
    if !missing.is_empty() {
        match remote::fetch_blob_sizes(
            &cache,
            &cfg.remote_url,
            &cfg.owner,
            &cfg.repo,
            &missing,
            token.as_deref(),
        )
        .await
        {
            Ok(fetched) => sizes.extend(fetched),
            Err(e) => output::warning(&format!(
                "couldn't fetch sizes for {} blob(s): {e}; they'll stat as empty until \
                 remounted with the remote reachable",
                missing.len()
            )),
        }
    }

    output::success(&format!(
        "Resuming mount {}/{} at {} (virtual branch {}).",
        cfg.owner,
        cfg.repo,
        dest.display(),
        cfg.virtual_branch,
    ));
    output::info(&format!("  state dir: {}", state_dir.display()));

    run_backend(
        cfg, cache, &manifest, &sizes, token, dest, &state_dir, base_mtime, false,
    )
    .await?;

    output::info("Unmounted.");
    Ok(())
}

/// Start the platform virtualization backend over prepared mount state and
/// block until shutdown (Ctrl-C, external unmount, or backend exit). Shared
/// by a fresh `__serve` and a `__resume` over an existing state dir;
/// `unregister_on_fail` is true only for fresh mounts, where a failed start
/// should leave no registration behind. A resume's registration must survive
/// failure — its state dir may hold the only copy of unpushed work.
#[allow(clippy::too_many_arguments)]
async fn run_backend(
    cfg: MountConfig,
    cache: std::sync::Arc<SqliteRepository>,
    manifest: &Manifest,
    sizes: &HashMap<String, u64>,
    token: Option<String>,
    dest: &Path,
    state_dir: &Path,
    base_mtime: std::time::SystemTime,
    unregister_on_fail: bool,
) -> Result<()> {
    // Record this daemon's pid so `oak mount end` can terminate it after the
    // external unmount instead of leaking one parked process per mount.
    if let Err(e) = state::save_daemon_pid(state_dir, std::process::id()) {
        output::warning(&format!("couldn't record daemon pid: {e}"));
    }

    // Both Unix backends build the same `MountCore` from the prepared state;
    // only the OS virtualization layer differs.
    // We're on a tokio worker thread here (our callers are async). `MountCore::new`
    // hydrates the ignore file with a blocking `block_on`, which would panic on
    // a worker thread — but `new` runs that hydrate on its own scratch thread
    // (see `load_mount_ignore`), so calling it directly is safe. The
    // `Handle::current()` we hand it stays valid for the later FUSE callbacks.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let core = core::MountCore::new(
        cfg.clone(),
        cache,
        manifest,
        sizes,
        token,
        tokio::runtime::Handle::current(),
        state_dir.to_path_buf(),
        base_mtime,
    )?;

    #[cfg(target_os = "linux")]
    {
        // fuser's `mount2` blocks the caller's thread; spawn it on a blocking
        // task so the tokio runtime can keep servicing async fetches inside
        // the FUSE callbacks. It returns when the filesystem is unmounted —
        // externally too — so no extra mountpoint watch is needed here.
        let mfs = fuse_fs::MountFs::new(core);
        let dest_owned = dest.to_path_buf();
        let mount_result = tokio::task::spawn_blocking(move || fuse_fs::mount_fs(&dest_owned, mfs))
            .await
            .map_err(|e| {
                OakError::Io(std::io::Error::other(format!("mount task panicked: {}", e)))
            })?;

        if let Err(e) = mount_result {
            // Best-effort cleanup of the registration so a stale entry doesn't
            // prevent re-mounting.
            if unregister_on_fail {
                let _ = unregister_mount(dest);
            }
            return Err(e);
        }
    }

    #[cfg(target_os = "macos")]
    {
        // FSKit runs the filesystem in the OS-loaded `OakFS` extension; this
        // daemon serves its requests over IPC. `start()` mounts the volume and
        // returns immediately (like ProjFS), so we park until shutdown.
        let fmount = match fskit::FskitMount::start(core, dest, state_dir) {
            Ok(m) => m,
            Err(e) => {
                if unregister_on_fail {
                    let _ = unregister_mount(dest);
                }
                return Err(e);
            }
        };

        // Park on Ctrl-C — or on the mountpoint disappearing underneath us
        // (external `umount`, `diskutil`, `oak mount end` racing the TERM).
        // Without the watch an external unmount leaves this daemon parked
        // forever: one leaked process per finished task. Two consecutive
        // misses are required so a transient stat hiccup doesn't take down a
        // healthy mount.
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        let mut misses = 0u32;
        loop {
            tokio::select! {
                r = &mut ctrl_c => {
                    if let Err(e) = r {
                        output::warning(&format!("ctrl-c handler failed: {e}"));
                    }
                    break;
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                    if spawn::is_mountpoint(dest) {
                        misses = 0;
                    } else {
                        misses += 1;
                        if misses >= 2 {
                            output::info("Mountpoint is gone; shutting down.");
                            break;
                        }
                    }
                }
            }
        }
        if let Err(e) = fmount.stop() {
            output::warning(&format!("unmounting OakFS failed: {e}"));
        }
    }

    #[cfg(target_os = "windows")]
    {
        let _ = base_mtime;
        // ProjFS doesn't block — it spins up its own threadpool to service
        // callbacks. We start it, then park here until Ctrl-C, then stop.
        let rt_handle = tokio::runtime::Handle::current();
        let pfs = match projfs_fs::ProjFsMount::start(
            cfg.clone(),
            cache,
            manifest,
            sizes,
            token,
            rt_handle,
            state_dir.to_path_buf(),
        ) {
            Ok(p) => p,
            Err(e) => {
                if unregister_on_fail {
                    let _ = unregister_mount(dest);
                }
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

// ---------------------------------------------------------------------------
// `oak commit` inside a mount
// ---------------------------------------------------------------------------

pub fn commit(dest: &Path) -> Result<()> {
    commit_paths(dest, dest, &[])
}

/// Path-scoped variant of [`commit`]: land only the overlay changes under
/// `paths` (resolved against `cwd`, like `oak diff <paths>`), leaving the rest
/// of the overlay dirty for a later commit. Empty `paths` commits everything.
pub fn commit_paths(dest: &Path, cwd: &Path, paths: &[PathBuf]) -> Result<()> {
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

    let full_overlay = load_overlay_meta(&state_dir)?;

    if full_overlay.dirty.is_empty()
        && full_overlay.deletions.is_empty()
        && full_overlay.renames.is_empty()
    {
        output::info("Nothing to commit, mount is clean.");
        return Ok(());
    }

    // Canonicalize the mount root before stripping it: user paths get
    // canonicalized inside `repo_relative_str` (e.g. macOS `/var` →
    // `/private/var`), so the prefix must be too.
    let mount_root = fs::canonicalize(&cfg.mount_point).unwrap_or_else(|_| cfg.mount_point.clone());
    let filters: Vec<String> = paths
        .iter()
        .map(|p| crate::pathutil::repo_relative_str(cwd, &mount_root, &p.to_string_lossy()))
        .collect::<Result<_>>()?;
    let (overlay, remaining) = split_overlay(&full_overlay, &filters);
    if overlay.dirty.is_empty() && overlay.deletions.is_empty() && overlay.renames.is_empty() {
        output::info("No changes to commit in the given paths");
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

    // Clear the committed part of the overlay now that it's folded into the
    // branch — a scoped commit leaves the unselected entries dirty. Hold the
    // cross-process overlay lock so the daemon's own overlay persistence
    // (`MountCore::persist_overlay`) can't interleave with the rewrite and
    // resurrect just-committed entries.
    let remaining_count =
        remaining.dirty.len() + remaining.deletions.len() + remaining.renames.len();
    {
        let _overlay_lock = state::lock_overlay_meta(&state_dir)?;
        save_overlay_meta(&state_dir, &remaining)?;
        if remaining_count == 0 {
            // Best-effort: wipe overlay files so the next read flows from the cache.
            if let Ok(rd) = fs::read_dir(&overlay_root) {
                for ent in rd.flatten() {
                    let _ = fs::remove_file(ent.path());
                }
            }
        } else {
            // Scoped commit: remove only the files we just committed — the
            // remaining dirty entries still own theirs.
            for entry in overlay.dirty.values() {
                if !entry.in_place {
                    let _ = fs::remove_file(overlay_root.join(&entry.overlay_file));
                }
            }
        }
    }

    output::success(&format!(
        "Created commit {} on virtual branch '{}'",
        commit_hash.short(),
        cfg.virtual_branch
    ));
    if remaining_count > 0 {
        output::info(&format!(
            "{remaining_count} other change{} left uncommitted (see `oak status`)",
            if remaining_count == 1 { "" } else { "s" },
        ));
    }
    output::info("(Run `oak push` to publish, or remount the dest to see the change reflected.)");

    Ok(())
}

/// Split a mount overlay into the part a path-scoped commit lands (first) and
/// the part it leaves dirty (second). An empty filter set selects everything.
/// Renames match on either their old or new path — same as `oak diff` — and a
/// selected rename drags the dirty content of its destination along with it,
/// so a rename-with-edit always commits as one unit.
pub(super) fn split_overlay(
    overlay: &OverlayMeta,
    filters: &[String],
) -> (OverlayMeta, OverlayMeta) {
    use super::diff::path_matches;

    if filters.is_empty() {
        return (overlay.clone(), OverlayMeta::default());
    }
    let mut selected = OverlayMeta::default();
    let mut remaining = OverlayMeta::default();

    for (old, new) in &overlay.renames {
        if path_matches(old, filters) || path_matches(new, filters) {
            selected.renames.insert(old.clone(), new.clone());
        } else {
            remaining.renames.insert(old.clone(), new.clone());
        }
    }
    for (path, entry) in &overlay.dirty {
        if path_matches(path, filters) || selected.renames.values().any(|n| n == path) {
            selected.dirty.insert(path.clone(), entry.clone());
        } else {
            remaining.dirty.insert(path.clone(), entry.clone());
        }
    }
    for path in &overlay.deletions {
        if path_matches(path, filters) {
            selected.deletions.push(path.clone());
        } else {
            remaining.deletions.push(path.clone());
        }
    }
    (selected, remaining)
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
    .await?;

    // Record the head we just pushed so teardown (`end`, `space clean`,
    // the worktree-remove hook) can tell safely-on-the-server from
    // committed-but-unpushed work. Best-effort: a failure here only makes
    // teardown more conservative.
    if let Some(head) = cache.get_branch_head(&cfg.virtual_branch)? {
        if let Err(e) = state::save_pushed_head(&state_dir, head.as_str()) {
            output::warning(&format!("couldn't record the pushed head: {e}"));
        }
    }
    Ok(())
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
// `oak mount end` — unmount, drop local state, and remove the dest dir in one
// shot. Designed for branch-per-task agent workflows where each task lives in
// its own mount subdir and the final cleanup should leave nothing behind.
// ---------------------------------------------------------------------------

pub fn end(dest: &Path, force: bool) -> Result<()> {
    let id = lookup_id_for(dest)?
        .ok_or_else(|| OakError::Server(format!("no mount registered for '{}'", dest.display())))?;
    let state_dir = state_dir_for(&id)?;

    // Refuse to end a mount with uncommitted work so the user doesn't
    // silently lose changes. `--force` overrides this for the "yes I want
    // to discard" case.
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

    // A clean overlay isn't enough: commits made inside the mount live solely
    // in this state dir's cache.db until pushed, so deleting it below would
    // destroy the only copy (the classic "commit succeeded, push failed
    // offline, agent runs `oak mount end`" sequence).
    let unpushed = unpushed_commits(&state_dir);
    if unpushed > 0 {
        let branch = load_config(&state_dir)
            .map(|c| c.virtual_branch)
            .unwrap_or_else(|_| "<unknown>".to_string());
        if !force {
            return Err(OakError::Server(format!(
                "{unpushed} unpushed commit(s) on '{branch}' — run `oak push` inside the mount, \
                 or `oak mount end --force` to discard."
            )));
        }
        output::warning(&format!(
            "Discarding {unpushed} unpushed commit(s) on '{branch}'."
        ));
    }

    // Windows has no out-of-process unmount (`platform_unmount` is a no-op
    // there), so deleting the state dir would pull the cache and overlay out
    // from under a still-running virtualization. Refuse while the daemon is
    // alive instead of corrupting it.
    #[cfg(target_os = "windows")]
    if let Some(pid) = state::load_daemon_pid(&state_dir) {
        if state::pid_alive(pid) {
            return Err(OakError::Server(format!(
                "the mount daemon (pid {pid}) is still running and Windows has no \
                 out-of-process unmount — stop the `oak mount` process first, then \
                 re-run `oak mount end`."
            )));
        }
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

    // The external unmount doesn't stop the detached daemon (its mountpoint
    // watch is coarse), and deleting the state dir would yank the IPC socket
    // out from under it — terminate it explicitly so finished tasks don't
    // each leak a parked process.
    terminate_daemon(&state_dir);

    // Inline what forget() does to the registry — we want a single combined
    // success message at the end instead of forget()'s "Forgot mount" line
    // plus ours — and also drop the on-disk state, which forget keeps.
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

/// `oak mount end` with no dest — tear down every mount under `~/oaktree`.
/// The counterpart to a bare `oak mount`: one call cleans up the whole tree.
pub fn end_all(force: bool) -> Result<()> {
    let root = oaktree_root()?;
    // The index stores canonical, absolute paths; canonicalize the root the
    // same way so the prefix test matches.
    let root_key = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    let mut targets: Vec<PathBuf> = load_index()?
        .mounts
        .keys()
        .map(PathBuf::from)
        .filter(|p| p.starts_with(&root_key))
        .collect();
    targets.sort();

    if targets.is_empty() {
        output::info(&format!("No mounts under {}.", root.display()));
        return Ok(());
    }

    output::info(&format!(
        "Ending {} mount(s) under {}...",
        targets.len(),
        root.display()
    ));
    let mut ended = 0usize;
    let mut failed = 0usize;
    for dest in &targets {
        match end(dest, force) {
            Ok(()) => ended += 1,
            Err(e) => {
                failed += 1;
                output::warning(&format!("{}: {e}", dest.display()));
            }
        }
    }
    output::success(&format!("Ended {ended}, failed {failed}."));
    Ok(())
}

/// `oak mount forget <dest>` — drop a mount's registry entry without
/// unmounting or touching its on-disk state. The recovery tool for stale
/// registrations left behind by a crash or reboot; refuses a demonstrably
/// live mount unless forced, since forgetting one strands a running daemon.
pub fn forget(dest: &Path, force: bool) -> Result<()> {
    let id = lookup_id_for(dest)?
        .ok_or_else(|| OakError::Server(format!("no mount registered for '{}'", dest.display())))?;
    let state_dir = state_dir_for(&id)?;

    if !force {
        let live_mount = spawn::is_mountpoint(dest);
        let live_daemon = state::load_daemon_pid(&state_dir).is_some_and(state::pid_alive);
        if live_mount || live_daemon {
            return Err(OakError::Server(format!(
                "mount at '{}' looks live ({}) — use `oak mount end` to tear it down, or \
                 `oak mount forget --force` to drop the registration anyway.",
                dest.display(),
                if live_mount {
                    "the path is a mountpoint"
                } else {
                    "its daemon is running"
                },
            )));
        }
    }

    unregister_mount(dest)?;
    output::success(&format!("Forgot mount '{}'", dest.display()));
    if state_dir.exists() {
        output::info(&format!(
            "Mount state kept at {} (it may hold unpushed commits); delete it manually \
             once you're sure it isn't needed.",
            state_dir.display()
        ));
    }
    Ok(())
}

/// Commits on the mount's virtual branch that exist only in this mount's
/// cache.db: everything after the head recorded at the last successful
/// `oak push` (or every virtual-branch commit when nothing was ever pushed —
/// the base commit lives on the source branch, so it's never counted).
/// Unreadable state counts as 0 so teardown of a broken or half-created
/// mount stays possible.
pub(crate) fn unpushed_commits(state_dir: &Path) -> usize {
    let Ok(cfg) = load_config(state_dir) else {
        return 0;
    };
    let Ok(cache) = SqliteRepository::open_relaxed(&cache_db_path(state_dir)) else {
        return 0;
    };
    let since = state::load_pushed_head(state_dir).map(Hash);
    cache
        .get_commits_since(&cfg.virtual_branch, since.as_ref())
        .map(|commits| commits.len())
        .unwrap_or(0)
}

/// Stop the detached daemon recorded for this mount, if it's still running:
/// TERM, then KILL after a short grace. Skips our own pid so a daemon-side
/// caller can never shoot itself.
#[cfg(unix)]
fn terminate_daemon(state_dir: &Path) {
    let Some(pid) = state::load_daemon_pid(state_dir) else {
        return;
    };
    if pid == std::process::id() || !state::pid_alive(pid) {
        return;
    }
    spawn::terminate_pid(pid);
}

/// Windows refuses `end` while the daemon is alive (see `end`), so there is
/// never a live daemon to stop by the time teardown gets here.
#[cfg(not(unix))]
fn terminate_daemon(_state_dir: &Path) {}

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
