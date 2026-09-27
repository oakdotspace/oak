//! Local observations only. No remote checks, hydration, or cleanup authority.
//!
//! `--verify-local` (fb-168b/fb-324) additionally inspects each checkout's
//! working tree and unpublished commits under that checkout's workdir lock,
//! through a read-only database connection, and reports `unverified` with a
//! reason whenever the lock cannot be taken. `--include-ci` (fb-324) adds the
//! CI state of each checkout's exact head from a bounded recent-runs scan.
//! Neither certifies that a directory is safe to delete.
use oak_core::{MetadataKey, Repository, Result, SqliteRepository};
use serde::Serialize;
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Serialize)]
struct Entry {
    root: PathBuf,
    kind: &'static str,
    branch: Option<String>,
    head: Option<String>,
    repo_owner: Option<String>,
    repo_name: Option<String>,
    observation: &'static str,
    progress_markers: Vec<&'static str>,
    working_tree: &'static str,
    unpublished_commits: &'static str,
    remote_publication: &'static str,
    external_artifacts: &'static str,
    process_ownership: &'static str,
    daemon_pid_alive: Option<bool>,
    base_head: Option<String>,
    /// `--verify-local` only: "verified" when the fields below were observed
    /// under this checkout's workdir lock, else "unverified" with a reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    local_verification: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_verification_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_tree_change_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unpublished_commit_count: Option<usize>,
    /// How `unpublished_commit_count` was derived (see [`UnpublishedBasis`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    unpublished_commits_basis: Option<&'static str>,
    /// What the unpublished count covers: always `all_local_branches`.
    #[serde(skip_serializing_if = "Option::is_none")]
    unpublished_commits_scope: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_branches_examined: Option<usize>,
    /// Local branches holding unpublished commits, with per-branch evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    unpublished_by_branch: Option<Vec<BranchUnpublished>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ci: Option<CiObservation>,
}

/// CI evidence for one checkout's exact head (`--include-ci`).
#[derive(Serialize, Clone)]
struct CiObservation {
    /// success | failure | running | not_found_in_recent_runs | unavailable | skipped
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_branch_matches_checkout: Option<bool>,
    /// What was searched: the newest N runs of the repository, not all history.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl CiObservation {
    fn without_run(state: &'static str, reason: impl Into<String>) -> Self {
        Self {
            state,
            commit: None,
            run_id: None,
            run_branch: None,
            run_branch_matches_checkout: None,
            scope: None,
            reason: Some(reason.into()),
        }
    }
}

/// Basis for an unpublished-commit count, most to least specific.
mod unpublished_basis {
    /// The checkout's own receipt of its last push (or last remote refresh)
    /// for this branch and remote; commits after that head are counted.
    pub const PUSH_RECEIPT: &str = "local_push_receipt";
    /// The branch was never linked to a remote: every branch commit is local.
    pub const NO_REMOTE: &str = "no_remote_configured";
    /// No push receipt: commits on the branch not yet merged to its parent.
    /// An upper bound that may include commits already pushed.
    pub const UNMERGED_UPPER_BOUND: &str = "unmerged_commits_upper_bound";
    /// The checkout has no local branch other than `main`.
    pub const ALL_BRANCHES_EMPTY: &str = "no_local_branches";
}

impl Entry {
    fn unknown(root: PathBuf, kind: &'static str) -> Self {
        Self {
            root,
            kind,
            branch: None,
            head: None,
            repo_owner: None,
            repo_name: None,
            observation: "unavailable",
            progress_markers: Vec::new(),
            working_tree: "unverified",
            unpublished_commits: "unverified",
            remote_publication: "unverified",
            external_artifacts: "unverified",
            process_ownership: "unverified",
            daemon_pid_alive: None,
            base_head: None,
            local_verification: None,
            local_verification_reason: None,
            working_tree_change_count: None,
            unpublished_commit_count: None,
            unpublished_commits_basis: None,
            unpublished_commits_scope: None,
            local_branches_examined: None,
            unpublished_by_branch: None,
            ci: None,
        }
    }

    fn mark_unverified(&mut self, reason: impl Into<String>) {
        self.local_verification = Some("unverified");
        self.local_verification_reason = Some(reason.into());
    }
}

/// Options for [`run_with_options`].
#[derive(Clone, Copy, Debug, Default)]
pub struct InventoryOptions {
    pub max_entries: u32,
    pub max_depth: u8,
    pub json: bool,
    pub verify_local: bool,
    pub include_ci: bool,
}

/// Distinct repositories whose CI is queried per run, and the per-request
/// wall budget. Each repository costs one bounded recent-runs listing.
const CI_MAX_REPOSITORIES: usize = 20;
const CI_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// Overall wall budget for all CI listings in one inventory run.
const CI_TOTAL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// Most local branches whose unpublished commits are counted per checkout.
/// Beyond this the checkout's unpublished state is reported unverified.
const MAX_LOCAL_BRANCHES: usize = 500;

/// Unpublished commits on one local branch.
#[derive(Serialize, Clone)]
struct BranchUnpublished {
    branch: String,
    count: usize,
    basis: &'static str,
}

/// Observed facts for a checkout verified under its lock.
struct LocalFacts {
    branch: Option<String>,
    head: Option<String>,
    owner: Option<String>,
    name: Option<String>,
    changes: usize,
    /// Every local branch (except the server-owned `main`) with its
    /// unpublished count, or `Err(reason)` when that could not be determined.
    unpublished: std::result::Result<Vec<BranchUnpublished>, String>,
    branches_examined: usize,
}

/// Unpublished commits on `branch_name`, with the evidence used.
fn branch_unpublished(
    repo: &SqliteRepository,
    oak_dir: &Path,
    identity: Option<(&str, &str, &str)>,
    branch_name: &str,
) -> std::result::Result<(usize, &'static str), String> {
    let recorded = crate::commands::commit::unmerged_commit_count(repo, branch_name)
        .map_err(|error| format!("commit_count_failed: {error}"))?;
    let Some((remote, owner, repo_name)) = identity else {
        return Ok((recorded, unpublished_basis::NO_REMOTE));
    };
    // The receipt names exactly one branch; it is evidence only for that one.
    let head = repo
        .get_branch_head(branch_name)
        .map_err(|error| format!("branch_head_unreadable: {error}"))?
        .map(|h| h.to_string());
    match crate::work_state::checkout_push_receipt(oak_dir, remote, owner, repo_name, branch_name) {
        Some((Some(pushed), _)) if Some(&pushed) == head.as_ref() => {
            Ok((0, unpublished_basis::PUSH_RECEIPT))
        }
        Some((Some(pushed), _)) => {
            match repo.get_commits_since(branch_name, Some(&oak_core::Hash(pushed))) {
                Ok(commits) => Ok((commits.len(), unpublished_basis::PUSH_RECEIPT)),
                Err(_) => Ok((recorded, unpublished_basis::UNMERGED_UPPER_BOUND)),
            }
        }
        _ => Ok((recorded, unpublished_basis::UNMERGED_UPPER_BOUND)),
    }
}

/// Describe a workdir lock's recorded owner without implying any action on
/// it: inspection never reaps, even when the owner has exited.
fn lock_owner_for_inspection(lock_path: &Path) -> String {
    use crate::workdir_lock::{process_liveness, ProcessLiveness};
    match bounded_file(lock_path, 64)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| text.trim().parse::<u32>().ok())
    {
        Some(pid) => match process_liveness(pid) {
            ProcessLiveness::Alive => format!("held by live pid {pid}"),
            ProcessLiveness::Dead => format!(
                "recorded owner pid {pid} has exited; the stale lock is left in place for its owner or the next Oak writer to clear"
            ),
            ProcessLiveness::Unknown => {
                format!("recorded owner pid {pid}, whose liveness cannot be determined")
            }
        },
        None => "owner pid unreadable or not yet recorded".to_string(),
    }
}

fn verify_checkout(path: &Path) -> std::result::Result<LocalFacts, String> {
    let oak_dir = path.join(".oak");
    // Take the checkout's workdir lock so no Oak writer (commit, pull,
    // merge, reset, restore, switch) moves refs or files mid-observation.
    // Never wait, never reap: a held lock is reported, not broken.
    let _lock = match crate::workdir_lock::WorkdirLock::try_acquire_for_inspection(&oak_dir) {
        Ok(lock) => lock,
        Err(oak_core::OakError::RepoLocked) => {
            return Err(format!(
                "checkout_locked: its workdir lock file exists ({}); inspection never waits for or removes it",
                lock_owner_for_inspection(&oak_dir.join("wdlock"))
            ))
        }
        Err(error) => return Err(format!("lock_unavailable: {error}")),
    };
    let db = oak_dir.join("oak.db");
    let repo = SqliteRepository::open_read_only(&db)
        .map_err(|error| format!("database_unreadable: {error}"))?;
    let (changes, head, branch) = crate::commands::commit::compute_changes_read_only(&repo, path)
        .map_err(|error| format!("working_tree_scan_failed: {error}"))?;
    let owner = repo.get_metadata(MetadataKey::RepoOwner).ok().flatten();
    let name = repo.get_metadata(MetadataKey::RepoName).ok().flatten();
    let remote = repo
        .get_metadata(MetadataKey::RemoteUrl)
        .ok()
        .flatten()
        .as_deref()
        .and_then(crate::commands::push::normalize_remote_url);
    let identity = match (remote.as_deref(), owner.as_deref(), name.as_deref()) {
        (Some(remote), Some(owner), Some(repo_name)) => Some((remote, owner, repo_name)),
        _ => None,
    };
    // Every local branch counts, not just the current one: a commit on a
    // branch the checkout is not currently on is just as easily lost.
    let mut branches_examined = 0;
    let unpublished = (|| {
        let branches = repo
            .list_branches()
            .map_err(|error| format!("branch_list_unreadable: {error}"))?;
        if branches.len() > MAX_LOCAL_BRANCHES {
            return Err(format!(
                "too_many_local_branches: {} (limit {MAX_LOCAL_BRANCHES})",
                branches.len()
            ));
        }
        let mut rows = Vec::new();
        for local in branches {
            if local.name == oak_core::DEFAULT_BRANCH {
                continue;
            }
            branches_examined += 1;
            let (count, basis) = branch_unpublished(&repo, &oak_dir, identity, &local.name)?;
            rows.push(BranchUnpublished {
                branch: local.name,
                count,
                basis,
            });
        }
        rows.sort_by(|a, b| a.branch.cmp(&b.branch));
        Ok(rows)
    })();
    Ok(LocalFacts {
        branch,
        head: head.as_ref().map(ToString::to_string),
        owner,
        name,
        changes: changes.len(),
        unpublished,
        branches_examined,
    })
}

fn apply_local_verification(entry: &mut Entry) {
    if entry.kind != "checkout" {
        entry.mark_unverified(match entry.kind {
            "mount" => "mount_not_inspected: use `oak mount status --json`",
            _ => "not_an_oak_checkout",
        });
        return;
    }
    if entry.progress_markers.contains(&"CLONE_IN_PROGRESS") {
        entry.mark_unverified("clone_in_progress");
        return;
    }
    // A merge or parent sync in progress leaves the checkout between states;
    // its working tree and branch heads are not a settled observation.
    if let Some(marker) = entry
        .progress_markers
        .iter()
        .find(|marker| matches!(**marker, "MERGE_HEAD" | "SYNC_HEAD" | "SYNC_STATE"))
    {
        entry.mark_unverified(format!("operation_in_progress: {marker}"));
        return;
    }
    match verify_checkout(&entry.root) {
        Err(reason) => entry.mark_unverified(reason),
        Ok(facts) => {
            entry.local_verification = Some("verified");
            entry.branch = facts.branch;
            entry.head = facts.head;
            entry.repo_owner = facts.owner;
            entry.repo_name = facts.name;
            entry.observation = "observed";
            entry.working_tree = if facts.changes == 0 { "clean" } else { "dirty" };
            entry.working_tree_change_count = Some(facts.changes);
            match facts.unpublished {
                Ok(rows) => {
                    let count: usize = rows.iter().map(|row| row.count).sum();
                    entry.unpublished_commits = if count == 0 { "none" } else { "present" };
                    entry.unpublished_commit_count = Some(count);
                    // The weakest evidence used for any local branch.
                    entry.unpublished_commits_basis = Some(
                        [
                            unpublished_basis::UNMERGED_UPPER_BOUND,
                            unpublished_basis::NO_REMOTE,
                            unpublished_basis::PUSH_RECEIPT,
                        ]
                        .into_iter()
                        .find(|basis| rows.iter().any(|row| row.basis == *basis))
                        .unwrap_or(unpublished_basis::ALL_BRANCHES_EMPTY),
                    );
                    entry.unpublished_commits_scope = Some("all_local_branches");
                    entry.local_branches_examined = Some(facts.branches_examined);
                    entry.unpublished_by_branch =
                        Some(rows.into_iter().filter(|row| row.count > 0).collect());
                }
                Err(reason) => {
                    entry.local_verification_reason =
                        Some(format!("unpublished_commits: {reason}"));
                }
            }
        }
    }
}

async fn fill_ci(entries: &mut [Entry]) {
    use std::collections::HashMap;
    let mut listings: HashMap<
        (String, String, String),
        std::result::Result<Vec<crate::commands::ci::CiRun>, String>,
    > = HashMap::new();
    let scope = format!(
        "newest {} runs of the repository",
        crate::commands::ci::STATUS_SCAN_LIMIT
    );
    let deadline = tokio::time::Instant::now() + CI_TOTAL_DEADLINE;
    for entry in entries.iter_mut() {
        if entry.kind != "checkout" {
            continue;
        }
        let Some(head) = entry.head.clone() else {
            entry.ci = Some(CiObservation::without_run("unavailable", "no_local_head"));
            continue;
        };
        let db = entry.root.join(".oak/oak.db");
        let client = match SqliteRepository::open_read_only(&db)
            .and_then(|repo| crate::commands::ci::CiClient::from_open_repo(&repo))
        {
            Ok(client) => client,
            Err(error) => {
                entry.ci = Some(CiObservation::without_run(
                    "unavailable",
                    format!("no_remote_identity: {error}"),
                ));
                continue;
            }
        };
        let key = (
            client.remote.trim_end_matches('/').to_string(),
            client.owner.clone(),
            client.repo.clone(),
        );
        if !listings.contains_key(&key) {
            if listings.len() >= CI_MAX_REPOSITORIES {
                entry.ci = Some(CiObservation::without_run(
                    "skipped",
                    format!("ci_repository_budget_exhausted ({CI_MAX_REPOSITORIES})"),
                ));
                continue;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                entry.ci = Some(CiObservation::without_run(
                    "skipped",
                    format!(
                        "ci_deadline_exhausted ({}s total)",
                        CI_TOTAL_DEADLINE.as_secs()
                    ),
                ));
                continue;
            }
            let listed = match tokio::time::timeout(
                CI_REQUEST_TIMEOUT.min(remaining),
                client.list_runs(crate::commands::ci::STATUS_SCAN_LIMIT),
            )
            .await
            {
                Ok(Ok(runs)) => Ok(runs),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err("ci_request_timed_out".to_string()),
            };
            listings.insert(key.clone(), listed);
        }
        entry.ci = Some(match &listings[&key] {
            Err(reason) => CiObservation::without_run("unavailable", reason.clone()),
            Ok(runs) => match runs
                .iter()
                .filter(|run| run.commit_hash == head)
                .max_by_key(|run| run.id)
            {
                None => CiObservation {
                    state: "not_found_in_recent_runs",
                    commit: Some(head),
                    run_id: None,
                    run_branch: None,
                    run_branch_matches_checkout: None,
                    scope: Some(scope.clone()),
                    reason: None,
                },
                Some(run) => CiObservation {
                    state: run.gate_state().as_str(),
                    commit: Some(head),
                    run_id: Some(run.id),
                    run_branch: Some(run.branch.clone()),
                    run_branch_matches_checkout: Some(
                        entry.branch.as_deref() == Some(run.branch.as_str()),
                    ),
                    scope: Some(scope.clone()),
                    reason: None,
                },
            },
        });
    }
}

fn bounded_file(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    if !path.symlink_metadata()?.file_type().is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::other("metadata too large"));
    }
    Ok(bytes)
}

fn mount(path: &Path, id: &str) -> Entry {
    use crate::commands::mount::state;
    let mut entry = Entry::unknown(path.into(), "mount");
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return entry;
    }
    let observed = (|| -> Option<_> {
        let dir = state::state_dir_for(id).ok()?;
        if !dir.symlink_metadata().ok()?.file_type().is_dir() {
            return None;
        }
        let bytes = bounded_file(&dir.join("config.toml"), 64 * 1024).ok()?;
        let cfg: state::MountConfig = toml::from_str(std::str::from_utf8(&bytes).ok()?).ok()?;
        if cfg.id != id || cfg.mount_point != path {
            return None;
        }
        let pid = bounded_file(&dir.join("daemon.pid"), 32)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|pid| *pid > 0 && *pid <= i32::MAX as u32);
        entry.daemon_pid_alive = pid.map(state::pid_alive);
        for marker in ["sync-state.json"] {
            if dir.join(marker).symlink_metadata().is_ok() {
                entry.progress_markers.push(marker);
            }
        }
        Some(cfg)
    })();
    if let Some(cfg) = observed {
        entry.branch = Some(cfg.virtual_branch);
        entry.base_head = Some(cfg.base_commit);
        entry.repo_owner = Some(cfg.owner);
        entry.repo_name = Some(cfg.repo);
        entry.observation = "registered_config";
    }
    entry
}

fn registered_mounts(root: &Path) -> std::result::Result<BTreeMap<PathBuf, String>, ()> {
    let index = crate::commands::mount::state::index_path().map_err(|_| ())?;
    let bytes = match bounded_file(&index, 1024 * 1024) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(_) => return Err(()),
    };
    let index: crate::commands::mount::state::MountIndex =
        serde_json::from_slice(&bytes).map_err(|_| ())?;
    Ok(index
        .mounts
        .into_iter()
        .filter_map(|(p, id)| {
            let path = PathBuf::from(p);
            (path.is_absolute()
                && (path.starts_with(root) || root.starts_with(&path))
                && path.components().all(|c| {
                    !matches!(
                        c,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                }))
            .then_some((path, id))
        })
        .collect())
}

#[derive(Serialize)]
struct Inventory {
    schema_version: u32,
    root: PathBuf,
    consistency: &'static str,
    coverage_scope: &'static str,
    sqlite_coordination: &'static str,
    complete: bool,
    examined_entries: u32,
    coverage_gaps: Vec<String>,
    entries: Vec<Entry>,
}

fn checkout(path: &Path) -> Entry {
    let mut entry = Entry::unknown(path.into(), "checkout");
    let dir = path.join(".oak");
    for marker in ["CLONE_IN_PROGRESS", "MERGE_HEAD", "SYNC_HEAD", "SYNC_STATE"] {
        if dir.join(marker).symlink_metadata().is_ok() {
            entry.progress_markers.push(marker);
        }
    }
    if !entry.progress_markers.contains(&"CLONE_IN_PROGRESS") {
        let observed = (|| -> Result<_> {
            let db = dir.join("oak.db");
            if !db.symlink_metadata()?.file_type().is_file() {
                return Err(oak_core::OakError::InvalidArgument(
                    "not a regular database".into(),
                ));
            }
            let repo = SqliteRepository::open_read_only(&db)?;
            let branch = repo.get_current_branch_name()?;
            let head = match &branch {
                Some(b) => repo.get_branch_head(b)?.map(|h| h.to_string()),
                None => None,
            };
            Ok((
                branch,
                head,
                repo.get_metadata(MetadataKey::RepoOwner)?,
                repo.get_metadata(MetadataKey::RepoName)?,
            ))
        })();
        if let Ok((branch, head, owner, name)) = observed {
            entry.branch = branch;
            entry.head = head;
            entry.repo_owner = owner;
            entry.repo_name = name;
            entry.observation = "observed";
        }
    }
    entry
}

pub fn run(root: &Path, max_entries: u32, max_depth: u8, json: bool) -> Result<()> {
    let options = InventoryOptions {
        max_entries,
        max_depth,
        json,
        ..InventoryOptions::default()
    };
    let (result, json) = discover(root, options)?;
    print(&result, json)
}

/// `oak space inventory` with optional local verification and CI evidence.
pub async fn run_with_options(root: &Path, options: InventoryOptions) -> Result<()> {
    let (mut result, json) = discover(root, options)?;
    if options.verify_local {
        result.coverage_scope = "directory_discovery_with_locked_local_verification";
        for entry in &mut result.entries {
            apply_local_verification(entry);
        }
    }
    if options.include_ci {
        fill_ci(&mut result.entries).await;
    }
    print(&result, json)
}

fn discover(root: &Path, options: InventoryOptions) -> Result<(Inventory, bool)> {
    let InventoryOptions {
        max_entries,
        max_depth,
        json,
        ..
    } = options;
    let root = fs::canonicalize(root)?;
    if !root.is_dir() {
        return Err(oak_core::OakError::InvalidArgument(
            "inventory root must be a directory".into(),
        ));
    }
    let mut result = Inventory {
        schema_version: 1,
        root: root.clone(),
        consistency: "local_observation",
        coverage_scope: "directory_discovery_only",
        sqlite_coordination: "read_only_database_with_normal_wal_coordination",
        complete: true,
        examined_entries: 1,
        coverage_gaps: vec![],
        entries: vec![],
    };
    let mut queue = VecDeque::from([(root, 0)]);
    let mounts = match registered_mounts(&result.root) {
        Ok(m) => m,
        Err(()) => {
            result.coverage_gaps.push(
                "mount registry unreadable; directory discovery withheld to avoid hydration".into(),
            );
            queue.clear();
            BTreeMap::new()
        }
    };
    for (path, id) in &mounts {
        if !path.starts_with(&result.root) {
            result
                .coverage_gaps
                .push("selected root is inside a registered mount; traversal withheld".into());
            queue.clear();
            continue;
        }
        if result.examined_entries >= max_entries {
            result
                .coverage_gaps
                .push("entry budget exhausted reading mount registry".into());
            queue.clear();
            break;
        }
        result.examined_entries += 1;
        result.entries.push(mount(path, id));
    }
    while let Some((dir, depth)) = queue.pop_front() {
        if mounts.keys().any(|mount| dir.starts_with(mount)) {
            continue;
        }
        #[cfg(unix)]
        if crate::commands::mount::spawn::is_mountpoint(&dir) {
            result.coverage_gaps.push(format!(
                "filesystem boundary not traversed: {}",
                dir.display()
            ));
            continue;
        }
        if dir
            .join(".oak")
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_dir())
        {
            result.entries.push(checkout(&dir));
            continue;
        }
        if dir.join(".git").symlink_metadata().is_ok() {
            result.entries.push(Entry::unknown(dir, "git_checkout"));
            continue;
        }
        let children = match fs::read_dir(&dir) {
            Ok(c) => c,
            Err(_) => {
                result
                    .coverage_gaps
                    .push(format!("unreadable directory: {}", dir.display()));
                continue;
            }
        };
        let mut directories = Vec::new();
        for child in children {
            if result.examined_entries >= max_entries {
                result.coverage_gaps.push("entry budget exhausted".into());
                queue.clear();
                break;
            }
            result.examined_entries += 1;
            let child = match child {
                Ok(c) => c,
                Err(_) => {
                    result
                        .coverage_gaps
                        .push("unreadable directory entry".into());
                    continue;
                }
            };
            match child.file_type() {
                Ok(kind) if kind.is_dir() => {
                    if depth >= max_depth {
                        result
                            .coverage_gaps
                            .push(format!("depth limit: {}", child.path().display()));
                    } else {
                        directories.push(child.path());
                    }
                }
                Ok(kind) if kind.is_symlink() => result
                    .coverage_gaps
                    .push(format!("symlink not followed: {}", child.path().display())),
                Err(_) => result.coverage_gaps.push("unreadable entry type".into()),
                _ => {}
            }
        }
        if result.examined_entries < max_entries {
            directories.sort();
            queue.extend(directories.into_iter().map(|d| (d, depth + 1)));
        } else if !directories.is_empty() || !queue.is_empty() {
            result
                .coverage_gaps
                .push("entry budget exhausted before directory inspection".into());
            queue.clear();
        }
    }
    result.entries.sort_by(|a, b| a.root.cmp(&b.root));
    result.complete = result.coverage_gaps.is_empty();
    Ok((result, json))
}

fn print(result: &Inventory, json: bool) -> Result<()> {
    if json {
        crate::output::print_json(result)
    } else {
        for entry in &result.entries {
            match entry.local_verification {
                Some("verified") => crate::output::print_line(&format!(
                    "{}  {}  {}  working tree {} ({} change(s)), unpublished commits {}{}  (artifacts, remote publication and process ownership unverified)",
                    entry.root.display(),
                    entry.kind,
                    entry.observation,
                    entry.working_tree,
                    entry.working_tree_change_count.unwrap_or(0),
                    entry
                        .unpublished_commit_count
                        .map_or_else(|| "unverified".to_string(), |n| n.to_string()),
                    entry
                        .unpublished_commits_basis
                        .map_or_else(String::new, |basis| format!(" [{basis}]")),
                )),
                _ => crate::output::print_line(&format!("{}  {}  {}  (working tree, artifacts, publication and process ownership unverified{})",entry.root.display(),entry.kind,entry.observation, entry.local_verification_reason.as_deref().map_or_else(String::new, |r| format!("; {r}")))),
            }
            if let Some(ci) = &entry.ci {
                crate::output::print_line(&format!("    ci: {}", ci.state));
            }
        }
        for gap in &result.coverage_gaps {
            crate::output::print_line(&format!("Coverage gap: {gap}"));
        }
        crate::output::print_line("Local observation only; this does not authorize cleanup.");
        Ok(())
    }
}
