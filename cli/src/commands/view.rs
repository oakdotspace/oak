//! `oak sparse` in a view checkout — show or change the directories it holds.
//!
//! A view checkout (`oak clone ORG/REPO --path DIR` against a server that
//! serves views) holds view commits: ordinary
//! commits over a pruned tree, which the server derives from the full ones
//! (see `oak_core::view`). Widening or narrowing the view changes every commit
//! hash, so this doesn't rewrite history in place. It re-acquires the checkout
//! under the new view instead: a clone of the same repo and branch into a
//! sibling directory (reusing local content through `--from`), whose `.oak`
//! then replaces this one, with the working tree moved over from the old view's
//! files to the new view's.
//!
//! That is only safe with nothing local to lose, so a change refuses while the
//! working tree has uncommitted or untracked files, or the branch has commits
//! the server doesn't (push them first). The repo's hooks and watch config
//! are carried over; everything else local in `.oak` (push receipts, merge
//! state) speaks the old view's hashes and is left behind with it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use oak_core::{MetadataKey, OakError, Repository, Result, SqliteRepository, ViewSpec};
use serde::Serialize;

use crate::http::ActiveView;
use crate::output;

pub const SCHEMA_VERSION: u32 = 1;

/// What `oak view` should do. Mirrors the clap subcommands in `main.rs`.
pub enum ViewAction {
    Show,
    Add {
        paths: Vec<String>,
    },
    Remove {
        paths: Vec<String>,
    },
    /// Exactly these directories (`oak sparse set` in a view checkout).
    Set {
        paths: Vec<String>,
    },
}

/// Whether the checkout at `cwd` is a view checkout; `oak sparse` acts on the
/// view there (a view is the sparse checkout of a view-capable server).
pub fn is_view_checkout(cwd: &Path) -> bool {
    crate::resolve::resolve(cwd)
        .and_then(|ctx| SqliteRepository::open(&ctx.db_path()?))
        .is_ok_and(|repo| crate::http::view_of_checkout(&repo).is_some())
}

#[derive(Serialize)]
struct ViewJson {
    schema_version: u32,
    /// `owner/name` of the repo the view is of.
    repo: String,
    /// Canonical view spec.
    view: String,
    view_id: String,
    include: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exclude: Vec<String>,
    spine_files: Vec<String>,
    /// For a change: what it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    changed: Option<ChangeJson>,
    recommended_next_commands: Vec<String>,
}

#[derive(Serialize)]
struct ChangeJson {
    previous_view: String,
    branch: String,
    files_added: usize,
    files_removed: usize,
}

fn covers(prefix: &str, path: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || (path.len() > prefix.len()
            && path.as_bytes()[prefix.len()] == b'/'
            && path.starts_with(prefix))
}

fn normalize(path: &str) -> Result<String> {
    let path = path.replace('\\', "/");
    let path = path.trim_matches('/').to_string();
    if path.is_empty()
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(OakError::InvalidArgument(format!(
            "'{path}': use a repo-relative directory without `.` or `..`"
        )));
    }
    Ok(path)
}

pub fn run(cwd: &Path, rt: &tokio::runtime::Runtime, action: ViewAction, json: bool) -> Result<()> {
    let ctx = crate::resolve::resolve(cwd)?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    let Some(active) = crate::http::view_of_checkout(&repo) else {
        return Err(OakError::InvalidArgument(
            "this is a full checkout, not a view: `oak clone ORG/REPO --path DIR` makes one"
                .to_string(),
        ));
    };
    let spec = ViewSpec::parse(&active.spec)
        .map_err(|e| OakError::InvalidArgument(format!("stored view is invalid: {e}")))?;
    match action {
        ViewAction::Show => {
            drop(repo);
            show(&ctx.work_tree, &active, &spec, None, json)
        }
        ViewAction::Add { paths } => {
            let mut add = Vec::new();
            for p in &paths {
                add.push(normalize(p)?);
            }
            let next = widen(&spec, add)?;
            change(rt, &ctx.work_tree, repo, &active, &spec, next, json)
        }
        ViewAction::Remove { paths } => {
            let mut drop_paths = Vec::new();
            for p in &paths {
                drop_paths.push(normalize(p)?);
            }
            let next = narrow(&spec, drop_paths)?;
            change(rt, &ctx.work_tree, repo, &active, &spec, next, json)
        }
        ViewAction::Set { paths } => {
            let include = paths
                .iter()
                .flat_map(|p| p.split(','))
                .filter(|p| !p.trim().is_empty())
                .map(normalize)
                .collect::<Result<Vec<_>>>()?;
            if include.is_empty() {
                return Err(OakError::InvalidArgument(
                    "name at least one directory".to_string(),
                ));
            }
            let next = ViewSpec::new(&include, [] as [&str; 0], spec.spine_files())
                .map_err(|e| OakError::InvalidArgument(e.to_string()))?;
            change(rt, &ctx.work_tree, repo, &active, &spec, next, json)
        }
    }
}

/// The paths among `paths` a view checkout's view doesn't hold (`None` for a
/// full checkout).
pub fn outside_view<'a>(
    repo: &dyn Repository,
    paths: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let Some(spec) =
        crate::http::view_of_checkout(repo).and_then(|v| ViewSpec::parse(&v.spec).ok())
    else {
        return Vec::new();
    };
    paths
        .into_iter()
        .filter(|p| !spec.includes_file(p))
        .map(str::to_string)
        .collect()
}

/// The `path_outside_view` error for `paths`: the `oak sparse add` that
/// brings their top directories in.
pub fn outside_view_error(paths: Vec<String>) -> OakError {
    let dirs: BTreeSet<String> = paths
        .iter()
        .map(|p| match p.rsplit_once('/') {
            Some((dir, _)) => dir.to_string(),
            None => p.clone(),
        })
        .collect();
    let mut recommended_next_commands: Vec<String> = dirs
        .iter()
        .map(|d| format!("oak sparse add {}", output::shell_quote(d)))
        .collect();
    recommended_next_commands.push("oak sparse show --json".to_string());
    OakError::PathOutsideView {
        paths,
        recommended_next_commands,
    }
}

/// Mark the checkout at `work_tree` as holding `view`, so every later command
/// there sends it.
pub fn record_view(work_tree: &Path, view: &ActiveView) -> Result<()> {
    let ctx = crate::resolve::resolve(work_tree)?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    repo.set_metadata(MetadataKey::ViewSpec, &view.spec)
}

/// `spec` with the directories `add` included (and no longer excluded).
fn widen(spec: &ViewSpec, add: Vec<String>) -> Result<ViewSpec> {
    let mut include: BTreeSet<String> = spec.include().iter().cloned().collect();
    let mut exclude: BTreeSet<String> = spec.exclude().iter().cloned().collect();
    for p in add {
        exclude.retain(|e| !covers(&p, e) && !covers(e, &p));
        include.insert(p);
    }
    ViewSpec::new(&include, &exclude, spec.spine_files())
        .map_err(|e| OakError::InvalidArgument(e.to_string()))
}

/// `spec` without the directories `drop`: an included directory (or one
/// under it) leaves the include list; part of an included directory becomes
/// an exclude.
fn narrow(spec: &ViewSpec, drop: Vec<String>) -> Result<ViewSpec> {
    let mut include: BTreeSet<String> = spec.include().iter().cloned().collect();
    let mut exclude: BTreeSet<String> = spec.exclude().iter().cloned().collect();
    for p in drop {
        let before = include.len();
        include.retain(|i| !covers(&p, i));
        if include.len() == before {
            if include.iter().any(|i| covers(i, &p)) {
                exclude.insert(p);
            } else {
                return Err(OakError::InvalidArgument(format!(
                    "'{p}' isn't in this checkout (see `oak sparse show`)"
                )));
            }
        }
    }
    if include.is_empty() {
        return Err(OakError::InvalidArgument(
            "that would leave nothing in the view".to_string(),
        ));
    }
    ViewSpec::new(&include, &exclude, spec.spine_files())
        .map_err(|e| OakError::InvalidArgument(e.to_string()))
}

fn show(
    work_tree: &Path,
    active: &ActiveView,
    spec: &ViewSpec,
    changed: Option<ChangeJson>,
    json: bool,
) -> Result<()> {
    let _ = work_tree;
    if !json {
        if let Some(c) = &changed {
            output::success(&format!(
                "Directories changed: +{} file(s), -{} file(s), on branch {}",
                c.files_added, c.files_removed, c.branch
            ));
        }
        output::print_line(&format!("Partial checkout of {}:", active.repo));
        for i in spec.include() {
            output::print_line(&format!("  + {i}"));
        }
        for e in spec.exclude() {
            output::print_line(&format!("  - {e}"));
        }
        return Ok(());
    }
    output::print_json(&ViewJson {
        schema_version: SCHEMA_VERSION,
        repo: active.repo.clone(),
        view: spec.to_canonical(),
        view_id: spec.id().to_string(),
        include: spec.include().to_vec(),
        exclude: spec.exclude().to_vec(),
        spine_files: spec.spine_files().to_vec(),
        changed,
        recommended_next_commands: vec!["oak status --json".to_string()],
    })
}

/// The manifest at the current branch's head.
fn head_manifest(repo: &SqliteRepository) -> Result<(String, Option<oak_core::Manifest>)> {
    let branch = repo
        .get_current_branch_name()?
        .ok_or_else(|| OakError::InvalidArgument("no current branch".to_string()))?;
    let Some(head) = repo.get_branch_head(&branch)? else {
        return Ok((branch, None));
    };
    let commit = repo
        .get_commit(&head)?
        .ok_or_else(|| OakError::CommitNotFound(head.to_string()))?;
    let manifest = repo
        .get_manifest(&commit.manifest_hash)?
        .ok_or_else(|| OakError::ManifestNotFound(commit.manifest_hash.to_string()))?;
    Ok((branch, Some(manifest)))
}

fn change(
    rt: &tokio::runtime::Runtime,
    work_tree: &Path,
    repo: SqliteRepository,
    active: &ActiveView,
    current: &ViewSpec,
    next: ViewSpec,
    json: bool,
) -> Result<()> {
    if next.to_canonical() == current.to_canonical() {
        output::info("The checkout already holds exactly that; nothing to change.");
        return show(work_tree, active, current, None, json);
    }
    let (branch, old_manifest) = head_manifest(&repo)?;

    // Nothing local may be lost: no uncommitted or untracked files...
    let empty = oak_core::Manifest::new(Vec::new());
    let dirty = super::commit::dirty_paths_in_scope(
        &repo,
        work_tree,
        old_manifest.as_ref().unwrap_or(&empty),
        &|_| true,
        &|entry| repo.has_blob(&entry.blob_hash).unwrap_or(false),
    )?;
    if !dirty.is_empty() {
        return Err(OakError::DirtyWorkingTree(format!(
            "{} uncommitted or untracked file(s) (first: {}); commit and push them, or remove them, before changing the view",
            dirty.len(),
            dirty[0]
        )));
    }
    // ...and no commits the server doesn't have.
    let workspace = super::branch::RemoteWorkspace::open(work_tree)?;
    let remote_branches = rt.block_on(super::branch::fetch_remote_branches(&workspace.remote))?;
    let local_head = repo.get_branch_head(&branch)?;
    let on_server = remote_branches.iter().find(|b| b.name == branch);
    let unpushed = match on_server {
        Some(b) => b.head.as_deref() != local_head.as_ref().map(|h| h.as_str()),
        None => local_head != repo.get_branch_head(oak_core::DEFAULT_BRANCH)?,
    };
    if unpushed {
        return Err(OakError::InvalidArgument(format!(
            "branch {branch} has commits the server doesn't; run `oak push` before changing the view"
        )));
    }
    let remote = workspace.remote.remote_url.clone();
    let watch_config = repo.get_metadata(MetadataKey::WatchConfig)?;
    drop(workspace);
    drop(repo);

    // Re-acquire under the new view next to the checkout.
    let parent = work_tree
        .parent()
        .ok_or_else(|| OakError::InvalidArgument("the checkout has no parent directory".into()))?;
    let name = work_tree
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "checkout".to_string());
    let staging: PathBuf = parent.join(format!(".{name}.oak-view-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    let next_view = ActiveView {
        repo: active.repo.clone(),
        spec: next.to_canonical(),
    };
    crate::http::set_active_view(Some(next_view.clone()));
    // The clone's own narration is progress for a person; a `--json` caller
    // gets only this command's document.
    if json {
        output::begin_capture();
    }
    let acquired = rt.block_on(super::repo::clone_repo_with_seed(
        &remote,
        &active.repo,
        &staging,
        false,
        None,
        on_server.map(|_| branch.as_str()),
        None,
        super::integrity::CloneIntegrityPolicy {
            allow_legacy_scope: false,
            allow_unverified_budget: false,
        },
        false,
        false,
        Some(work_tree),
    ));
    if json {
        let _ = output::end_capture();
    }
    if let Err(e) = acquired.and_then(|()| record_view(&staging, &next_view)) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    let result = swap_in(work_tree, &staging, old_manifest.as_ref());
    let _ = std::fs::remove_dir_all(&staging);
    let (added, removed) = result?;
    let new_repo = SqliteRepository::open(&crate::resolve::resolve(work_tree)?.db_path()?)?;
    if let Some(config) = watch_config {
        new_repo.set_metadata(MetadataKey::WatchConfig, &config)?;
    }
    let new_branch = new_repo.get_current_branch_name()?.unwrap_or(branch);
    show(
        work_tree,
        &next_view,
        &next,
        Some(ChangeJson {
            previous_view: current.to_canonical(),
            branch: new_branch,
            files_added: added,
            files_removed: removed,
        }),
        json,
    )
}

/// Replace `work_tree`'s `.oak` with `staging`'s and move the working files
/// over: tracked files the new view drops are deleted, the new view's files
/// move in (identical ones are left alone). The repo's hooks are carried over.
/// Returns (added, removed).
fn swap_in(
    work_tree: &Path,
    staging: &Path,
    old_manifest: Option<&oak_core::Manifest>,
) -> Result<(usize, usize)> {
    let new_repo = SqliteRepository::open(&staging.join(".oak").join("oak.db"))?;
    let (_, new_manifest) = head_manifest(&new_repo)?;
    drop(new_repo);
    let new_paths: BTreeSet<&str> = new_manifest
        .iter()
        .flat_map(|m| m.entries.iter().map(|e| e.path.as_str()))
        .collect();
    let old_entries: std::collections::HashMap<&str, &oak_core::ManifestEntry> = old_manifest
        .iter()
        .flat_map(|m| m.entries.iter().map(|e| (e.path.as_str(), e)))
        .collect();

    // The metadata first: from here on the new `.oak` is authoritative, and
    // `oak reset -f` repairs a working tree a failure below left half-moved.
    let old_oak = work_tree.join(".oak");
    let retired = work_tree.join(format!(".oak.view-old-{}", std::process::id()));
    std::fs::rename(&old_oak, &retired)?;
    if let Err(e) = std::fs::rename(staging.join(".oak"), &old_oak) {
        let _ = std::fs::rename(&retired, &old_oak);
        return Err(e.into());
    }
    let hooks = retired.join("hooks");
    if hooks.is_dir() && !old_oak.join("hooks").exists() {
        let _ = std::fs::rename(&hooks, old_oak.join("hooks"));
    }
    let _ = std::fs::remove_dir_all(&retired);

    let mut removed = Vec::new();
    for path in old_entries.keys() {
        if !new_paths.contains(path) {
            let file = work_tree.join(path);
            if std::fs::symlink_metadata(&file).is_ok_and(|m| !m.is_dir()) {
                std::fs::remove_file(&file)?;
            }
            removed.push(path.to_string());
        }
    }
    let mut added = 0usize;
    for entry in new_manifest.iter().flat_map(|m| m.entries.iter()) {
        let same = old_entries
            .get(entry.path.as_str())
            .is_some_and(|old| old.blob_hash == entry.blob_hash && old.mode == entry.mode);
        if same {
            continue;
        }
        let from = staging.join(&entry.path);
        let to = work_tree.join(&entry.path);
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if std::fs::symlink_metadata(&to).is_ok() {
            std::fs::remove_file(&to)?;
        }
        std::fs::rename(&from, &to)?;
        if !old_entries.contains_key(entry.path.as_str()) {
            added += 1;
        }
    }
    if !removed.is_empty() {
        let ignore = oak_core::IgnorePatterns::new(work_tree)?;
        crate::materialize::prune_emptied_dirs(work_tree, &ignore, &removed)?;
    }
    Ok((added, removed.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(canonical: &str) -> ViewSpec {
        ViewSpec::parse(canonical).unwrap()
    }

    #[test]
    fn widen_includes_and_lifts_overlapping_excludes() {
        let s = spec("+apps/web\n-apps/web/legacy\n=AGENTS.md");
        let w = widen(&s, vec!["apps/billing".into()]).unwrap();
        assert_eq!(
            w.to_canonical(),
            "+apps/billing\n+apps/web\n-apps/web/legacy\n=AGENTS.md"
        );
        let w = widen(&s, vec!["apps/web/legacy".into()]).unwrap();
        assert!(w.exclude().is_empty());
        let w = widen(&s, vec!["apps".into()]).unwrap();
        assert!(w.exclude().is_empty() && w.include().contains(&"apps".to_string()));
    }

    #[test]
    fn narrow_drops_includes_or_excludes_part_of_one() {
        let s = spec("+apps/billing\n+apps/web\n=AGENTS.md");
        let n = narrow(&s, vec!["apps/billing".into()]).unwrap();
        assert_eq!(n.to_canonical(), "+apps/web\n=AGENTS.md");
        let n = narrow(&s, vec!["apps/web/legacy".into()]).unwrap();
        assert_eq!(n.exclude(), ["apps/web/legacy".to_string()]);
        assert!(
            narrow(&s, vec!["libs/ui".into()]).is_err(),
            "not in the view"
        );
        assert!(
            narrow(&s, vec!["apps".into()]).is_err(),
            "nothing would be left"
        );
    }

    #[test]
    fn normalize_refuses_escapes() {
        assert_eq!(normalize("/apps/web/").unwrap(), "apps/web");
        assert!(normalize("apps/../secret").is_err());
        assert!(normalize("").is_err());
    }
}
