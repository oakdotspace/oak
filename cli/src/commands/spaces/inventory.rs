//! Local observations only. No remote checks, hydration, or cleanup authority.
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
        }
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
    if json {
        crate::output::print_json(&result)
    } else {
        for entry in &result.entries {
            crate::output::print_line(&format!("{}  {}  {}  (working tree, artifacts, publication and process ownership unverified)",entry.root.display(),entry.kind,entry.observation));
        }
        for gap in &result.coverage_gaps {
            crate::output::print_line(&format!("Coverage gap: {gap}"));
        }
        crate::output::print_line("Local observation only; this does not authorize cleanup.");
        Ok(())
    }
}
