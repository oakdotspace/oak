//! `oak project import` and `oak project adopt`: moving a standalone repo into
//! the company repo as a project (`docs/project-import.md` in oakspace).
//!
//! `import` asks the server to run the job and follows it. `adopt` is for a
//! checkout of a repo that has moved: every request gets a `repo_moved`
//! answer, and adopt carries this checkout's work to a new project checkout.
//!
//! Adopt never touches the old checkout. It clones the project next to it,
//! switches to the branch the import recreated (or a new one), and applies two
//! layers there: what this branch changed since it left `main`, as one commit,
//! and the dirty files, left dirty. A file that changed on both sides isn't
//! overwritten: our version lands beside it as `<file>.adopted`, and the
//! command exits 5.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use oak_core::{Commit, FileMode, Hash, Manifest, OakError, Repository, Result, SqliteRepository};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::findings::{bounded_body, Target, TargetArgs};
use crate::output;

pub const SCHEMA_VERSION: u32 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_EVERY: Duration = Duration::from_secs(2);
const WAIT_LIMIT: Duration = Duration::from_secs(60 * 60);

fn client_request(
    builder: reqwest::RequestBuilder,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    let builder = builder.timeout(REQUEST_TIMEOUT);
    match token {
        Some(token) => builder.header("authorization", format!("Bearer {token}")),
        None => builder,
    }
}

async fn read_json(response: reqwest::Response) -> Result<(reqwest::StatusCode, Value)> {
    let status = response.status();
    let body = bounded_body(response).await?;
    Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

fn message(body: &Value, status: reqwest::StatusCode) -> String {
    body.get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {status}"))
}

/// `oak project import ORG/REPO --into PATH`.
pub async fn import(
    remote: String,
    source: &str,
    into: &str,
    dry_run: bool,
    wait: bool,
    json_output: bool,
) -> Result<()> {
    let (org, repo) = source
        .split_once('/')
        .filter(|(a, b)| !a.is_empty() && !b.is_empty())
        .ok_or_else(|| OakError::InvalidArgument(format!("expected ORG/REPO, got {source:?}")))?;
    let remote = remote.trim_end_matches('/').to_string();
    let token = super::credentials::effective_token(&remote);
    let client = crate::http::api_client();
    let net = |e: reqwest::Error| OakError::Http(format!("project import: {e}"));

    let index = client_request(
        client.get(format!("{remote}/api/orgs/{org}/projects?limit=1")),
        token.as_deref(),
    )
    .send()
    .await
    .map_err(net)?;
    let (status, body) = read_json(index).await?;
    let company = body
        .get("repo")
        .and_then(Value::as_str)
        .filter(|_| status.is_success())
        .map(str::to_string)
        .ok_or_else(|| {
            OakError::InvalidArgument(format!("{org} has no company repo you can read"))
        })?;
    let imports = format!("{remote}/api/{company}/project-imports");
    let started = client_request(client.post(&imports), token.as_deref())
        .json(&json!({"source": repo, "into": into, "dry_run": dry_run}))
        .send()
        .await
        .map_err(net)?;
    let (status, body) = read_json(started).await?;
    if !status.is_success() {
        return Err(match status {
            reqwest::StatusCode::UNPROCESSABLE_ENTITY | reqwest::StatusCode::CONFLICT => {
                OakError::InvalidArgument(message(&body, status))
            }
            _ => OakError::Server(message(&body, status)),
        });
    }
    if dry_run {
        return print_plan(&body, json_output);
    }
    let id = body
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| OakError::Server("the server didn't return an import id".to_string()))?;
    let url = format!("{imports}/{id}");
    let mut last = body;
    if wait {
        let deadline = std::time::Instant::now() + WAIT_LIMIT;
        loop {
            let state = last
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("running");
            if matches!(state, "done" | "failed" | "abandoned") {
                break;
            }
            if std::time::Instant::now() > deadline {
                return Err(OakError::Http(format!("import {id} is still running")));
            }
            tokio::time::sleep(POLL_EVERY).await;
            let polled = client_request(client.get(&url), token.as_deref())
                .send()
                .await
                .map_err(net)?;
            let (status, body) = read_json(polled).await?;
            if !status.is_success() {
                return Err(OakError::Server(message(&body, status)));
            }
            last = body;
        }
    }
    if json_output {
        return output::print_json(&last);
    }
    let state = last
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("running");
    output::print_line(&format!("Import {id}: {state}"));
    if let Some(error) = last.get("error").and_then(Value::as_str) {
        output::print_line(error);
    }
    if state == "failed" {
        return Err(OakError::Server(format!(
            "import {id} failed. Retry it with POST {url}/retry or on the web"
        )));
    }
    Ok(())
}

fn print_plan(plan: &Value, json_output: bool) -> Result<()> {
    if json_output {
        return output::print_json(plan);
    }
    let text = |key: &str| plan.get(key).and_then(Value::as_str).unwrap_or_default();
    output::print_line(&format!(
        "{} becomes project {} at {}",
        text("source"),
        text("project"),
        text("prefix")
    ));
    output::print_line(&format!(
        "{} commits, {} open branches{}",
        plan.get("commits").and_then(Value::as_u64).unwrap_or(0),
        plan.get("open_branches")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        if plan.get("restricted").and_then(Value::as_bool) == Some(true) {
            ", restricted to the source's people"
        } else {
            ""
        }
    ));
    for warning in plan
        .get("warnings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        output::print_line(&format!("- {}", warning.as_str().unwrap_or_default()));
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
struct MovedBranch {
    source_branch: String,
    target_branch: Option<String>,
    outcome: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Moved {
    project: String,
    path: String,
    company: String,
    archive: Option<String>,
    source_tip: Option<String>,
    #[serde(default)]
    branches: Vec<MovedBranch>,
}

#[derive(Debug, Default, Serialize)]
pub struct Adopted {
    pub schema_version: u32,
    pub project: String,
    pub path: String,
    pub company: String,
    pub archive: Option<String>,
    pub checkout: String,
    pub branch: String,
    /// Files changed by this branch's commits, committed in the new checkout.
    pub committed: Vec<String>,
    /// Dirty files, left dirty there.
    pub dirty: Vec<String>,
    /// Changed on both sides; ours is at `<file>.adopted`.
    pub conflicts: Vec<String>,
}

/// One file to carry: its content here, `None` when deleted, and what it was
/// before the change.
#[derive(Debug, PartialEq, Eq)]
struct Carried {
    path: String,
    content: Option<Vec<u8>>,
    executable: bool,
    base: Option<Hash>,
}

/// The newest commit of `head`'s first-parent chain that's on `main` as the
/// server knew it: this branch's fork point.
fn fork_point(
    repo: &SqliteRepository,
    head: &Hash,
    source_tip: Option<&str>,
) -> Result<Option<Commit>> {
    let on_main = |commit: &Commit| commit.branch_name == oak_core::DEFAULT_BRANCH;
    let mut at = repo.get_commit(head)?;
    let mut first = None;
    while let Some(commit) = at {
        if source_tip == Some(commit.hash.as_str()) {
            return Ok(Some(commit));
        }
        if on_main(&commit) && first.is_none() {
            first = Some(commit.clone());
        }
        at = match &commit.parent_hash {
            Some(parent) => repo.get_commit(parent)?,
            None => None,
        };
    }
    Ok(first)
}

fn manifest_of(repo: &SqliteRepository, commit: Option<&Commit>) -> Result<Manifest> {
    Ok(match commit {
        Some(commit) => repo
            .get_manifest(&commit.manifest_hash)?
            .unwrap_or_else(Manifest::empty),
        None => Manifest::empty(),
    })
}

fn committed_delta(
    repo: &SqliteRepository,
    base: &Manifest,
    head: &Manifest,
) -> Result<Vec<Carried>> {
    let old: BTreeMap<&str, &Hash> = base
        .entries
        .iter()
        .map(|e| (e.path.as_str(), &e.blob_hash))
        .collect();
    let new: BTreeMap<&str, _> = head.entries.iter().map(|e| (e.path.as_str(), e)).collect();
    let mut out = Vec::new();
    for (path, entry) in &new {
        if old.get(path).copied() == Some(&entry.blob_hash) {
            continue;
        }
        let blob = repo.get_blob(&entry.blob_hash)?.ok_or_else(|| {
            OakError::InvalidArgument(format!(
                "{path} has no local content: run this after `oak hydrate`"
            ))
        })?;
        out.push(Carried {
            path: path.to_string(),
            content: Some(blob.content),
            executable: entry.mode == FileMode::Executable,
            base: old.get(path).map(|h| (*h).clone()),
        });
    }
    for (path, hash) in &old {
        if !new.contains_key(path) {
            out.push(Carried {
                path: path.to_string(),
                content: None,
                executable: false,
                base: Some((*hash).clone()),
            });
        }
    }
    Ok(out)
}

/// The dirty files, from `oak status`, which knows the ignore rules.
fn dirty_delta(oak: &Path, cwd: &Path, head: &Manifest) -> Result<Vec<Carried>> {
    let output = Command::new(oak)
        .args(["status", "--json"])
        .current_dir(cwd)
        .output()
        .map_err(|e| OakError::Http(format!("running oak status: {e}")))?;
    let status: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        OakError::Server(format!(
            "oak status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    })?;
    let base: BTreeMap<&str, &Hash> = head
        .entries
        .iter()
        .map(|e| (e.path.as_str(), &e.blob_hash))
        .collect();
    let mut out = Vec::new();
    let changes = status
        .pointer("/working_changes/changes")
        .or_else(|| status.get("changes"))
        .and_then(Value::as_array);
    for change in changes.into_iter().flatten() {
        let Some(path) = change.get("path").and_then(Value::as_str) else {
            continue;
        };
        let file = cwd.join(path);
        let content = std::fs::read(&file).ok();
        if let Some(old) = change.get("old_path").and_then(Value::as_str) {
            out.push(Carried {
                path: old.to_string(),
                content: None,
                executable: false,
                base: base.get(old).map(|h| (*h).clone()),
            });
        }
        out.push(Carried {
            path: path.to_string(),
            executable: is_executable(&file),
            content,
            base: base.get(path).map(|h| (*h).clone()),
        });
    }
    Ok(out)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    false
}

fn write_file(path: &Path, content: &[u8], executable: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

/// Apply `changes` under `root`. Returns the paths applied and the paths that
/// conflicted: the file there is neither what we started from nor our result.
fn apply(root: &Path, changes: &[Carried]) -> Result<(Vec<String>, Vec<String>)> {
    let mut applied = Vec::new();
    let mut conflicts = Vec::new();
    for change in changes {
        if oak_core::worktree_path_problem(&change.path).is_some() {
            continue;
        }
        let target = root.join(&change.path);
        let there = std::fs::read(&target)
            .ok()
            .map(|bytes| oak_core::hash_bytes(&bytes));
        let ours = change.content.as_deref().map(oak_core::hash_bytes);
        if there == ours {
            continue;
        }
        if there != change.base {
            if let Some(content) = &change.content {
                let mut name = target.clone().into_os_string();
                name.push(".adopted");
                write_file(Path::new(&name), content, change.executable)?;
            }
            conflicts.push(change.path.clone());
            continue;
        }
        match &change.content {
            Some(content) => write_file(&target, content, change.executable)?,
            None => std::fs::remove_file(&target)?,
        }
        applied.push(change.path.clone());
    }
    Ok((applied, conflicts))
}

fn oak_run(oak: &Path, cwd: &Path, remote: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(oak)
        .args(args)
        .current_dir(cwd)
        .env("OAK_REMOTE", remote)
        .output()
        .map_err(|e| OakError::Http(format!("running oak {}: {e}", args.join(" "))))?;
    if output.status.success() {
        return Ok(());
    }
    Err(OakError::Server(format!(
        "oak {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

async fn fetch_moved(target: &Target, token: Option<&str>) -> Result<Moved> {
    let url = format!(
        "{}/api/{}/{}/branches",
        target.remote,
        urlencoding::encode(&target.owner),
        urlencoding::encode(&target.repo),
    );
    let response = client_request(crate::http::api_client().get(url), token)
        .send()
        .await
        .map_err(|e| OakError::Http(format!("asking where this repo went: {e}")))?;
    let (status, body) = read_json(response).await?;
    if status == reqwest::StatusCode::GONE
        && body.get("code").and_then(Value::as_str) == Some("repo_moved")
    {
        return serde_json::from_value(body)
            .map_err(|_| OakError::Server("the server's move record is unreadable".to_string()));
    }
    if status.is_success() {
        return Err(OakError::InvalidArgument(format!(
            "{}/{} hasn't moved: there's nothing to adopt",
            target.owner, target.repo
        )));
    }
    Err(OakError::InvalidArgument(format!(
        "can't tell where {}/{} went: {}",
        target.owner,
        target.repo,
        message(&body, status)
    )))
}

/// `oak project adopt [--into DIR]`.
pub async fn adopt(cwd: &Path, into: Option<PathBuf>, json_output: bool) -> Result<()> {
    let target = Target::resolve(cwd, &TargetArgs::default())?;
    let token = super::credentials::effective_token(&target.remote);
    let moved = fetch_moved(&target, token.as_deref()).await?;

    let ctx = crate::resolve::resolve(cwd)?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    let work_tree = ctx.work_tree.clone();
    let branch = repo
        .get_current_branch_name()?
        .unwrap_or_else(|| oak_core::DEFAULT_BRANCH.to_string());
    let head = repo.get_branch_head(&branch)?.or(repo.get_head()?);
    let head_commit = match &head {
        Some(hash) => repo.get_commit(hash)?,
        None => None,
    };
    let fork = match &head {
        Some(hash) => fork_point(&repo, hash, moved.source_tip.as_deref())?,
        None => None,
    };
    let head_manifest = manifest_of(&repo, head_commit.as_ref())?;
    let committed = committed_delta(&repo, &manifest_of(&repo, fork.as_ref())?, &head_manifest)?;
    let oak = std::env::current_exe()?;
    let dirty = dirty_delta(&oak, &work_tree, &head_manifest)?;

    let checkout = match into {
        Some(dir) => dir,
        None => {
            let name = work_tree
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| moved.project.clone());
            work_tree
                .parent()
                .unwrap_or(&work_tree)
                .join(format!("{name}-adopted"))
        }
    };
    if checkout.exists() {
        return Err(OakError::InvalidArgument(format!(
            "{} already exists: choose another with --into",
            checkout.display()
        )));
    }
    let parent = checkout.parent().unwrap_or(Path::new("."));
    let checkout_arg = checkout.display().to_string();
    let spec = format!("{}/{}", target.owner, moved.project);
    oak_run(
        &oak,
        parent,
        &target.remote,
        &["clone", &spec, &checkout_arg],
    )?;

    let imported = moved
        .branches
        .iter()
        .find(|b| b.source_branch == branch && b.outcome == "created" && b.target_branch.is_some());
    let has_work = !committed.is_empty() || !dirty.is_empty();
    let new_branch = match imported {
        Some(found) => {
            let name = found.target_branch.clone().unwrap_or_default();
            oak_run(&oak, &checkout, &target.remote, &["switch", &name])?;
            name
        }
        None if branch != oak_core::DEFAULT_BRANCH && has_work => {
            oak_run(&oak, &checkout, &target.remote, &["switch", "-c", &branch])?;
            branch.clone()
        }
        None if has_work => {
            let name = format!("adopt-{}", moved.project);
            oak_run(&oak, &checkout, &target.remote, &["switch", "-c", &name])?;
            name
        }
        None => oak_core::DEFAULT_BRANCH.to_string(),
    };

    let root = checkout.join(&moved.path);
    let (carried, mut conflicts) = apply(&root, &committed)?;
    if !carried.is_empty() {
        oak_run(&oak, &checkout, &target.remote, &["commit"])?;
        if let Some(description) = repo.get_branch(&branch)?.and_then(|b| b.description) {
            let _ = oak_run(&oak, &checkout, &target.remote, &["desc", &description]);
        }
    }
    let (left_dirty, more) = apply(&root, &dirty)?;
    conflicts.extend(more);

    let report = Adopted {
        schema_version: SCHEMA_VERSION,
        project: moved.project,
        path: moved.path,
        company: moved.company,
        archive: moved.archive,
        checkout: checkout_arg,
        branch: new_branch,
        committed: carried,
        dirty: left_dirty,
        conflicts,
    };
    if json_output {
        output::print_json(&report)?;
    } else {
        output::print_line(&format!(
            "Adopted into {} on {}: {} committed, {} left dirty.",
            report.checkout,
            report.branch,
            report.committed.len(),
            report.dirty.len()
        ));
        output::print_line(
            "This checkout is untouched. Work from the new one, and remove this when you're happy.",
        );
        for path in &report.conflicts {
            output::print_line(&format!(
                "Conflict: {path} changed on both sides; yours is at {path}.adopted"
            ));
        }
    }
    if report.conflicts.is_empty() {
        Ok(())
    } else {
        Err(OakError::ConflictDetected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn carried(path: &str, content: Option<&str>, base: Option<&str>) -> Carried {
        Carried {
            path: path.to_string(),
            content: content.map(|c| c.as_bytes().to_vec()),
            executable: false,
            base: base.map(|b| oak_core::hash_bytes(b.as_bytes())),
        }
    }

    #[test]
    fn applies_edits_and_keeps_both_sides_of_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("same.txt"), "base\n").unwrap();
        std::fs::write(root.join("moved.txt"), "someone else's\n").unwrap();
        std::fs::write(root.join("gone.txt"), "base\n").unwrap();
        std::fs::write(root.join("already.txt"), "ours\n").unwrap();
        let (applied, conflicts) = apply(
            root,
            &[
                carried("same.txt", Some("ours\n"), Some("base\n")),
                carried("moved.txt", Some("ours\n"), Some("base\n")),
                carried("gone.txt", None, Some("base\n")),
                carried("new/file.txt", Some("new\n"), None),
                carried("already.txt", Some("ours\n"), Some("base\n")),
            ],
        )
        .unwrap();
        assert_eq!(applied, ["same.txt", "gone.txt", "new/file.txt"]);
        assert_eq!(conflicts, ["moved.txt"]);
        assert_eq!(
            std::fs::read_to_string(root.join("same.txt")).unwrap(),
            "ours\n"
        );
        assert!(!root.join("gone.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("moved.txt")).unwrap(),
            "someone else's\n",
            "a conflict never overwrites"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("moved.txt.adopted")).unwrap(),
            "ours\n"
        );
    }

    #[test]
    fn refuses_a_path_that_would_escape_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let (applied, conflicts) =
            apply(dir.path(), &[carried("../escape.txt", Some("x"), None)]).unwrap();
        assert!(applied.is_empty() && conflicts.is_empty());
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }
}
