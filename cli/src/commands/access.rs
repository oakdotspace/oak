//! `oak access` and `oak owners`: a repo's ownership boundaries.
//!
//! Boundaries live on the server (`/api/:owner/:name/boundaries`): a directory,
//! the team that owns it, whether a change there needs that team's approval
//! (`required`, `advisory` or `off`), and who may read and write it. Nothing
//! about them is in the repo, so these commands always ask the server, with
//! the caller's own credentials: a boundary over a directory the caller can't
//! read isn't listed.
//!
//! `oak owners --branch` is the impact block: the boundaries the current
//! branch touches, with their owner team, approval setting and state. The same
//! block rides on `oak finish --json`, `oak merge --json` and mount finish.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use oak_core::{OakError, Repository, Result, SqliteRepository};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::findings::bounded_body;
use crate::output;

pub const SCHEMA_VERSION: u32 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Most boundaries read from one response.
const MAX_BOUNDARIES: usize = 10_000;

/// One ownership boundary, as the server describes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Boundary {
    pub id: i64,
    /// The directory (`""` is the whole repo).
    pub path: String,
    pub owner_team: String,
    /// `required`, `advisory` or `off`.
    pub approval: String,
    #[serde(default)]
    pub self_approve: bool,
    /// `None`: everyone who can read the repo (and the boundary above).
    #[serde(default)]
    pub readers: Option<Vec<String>>,
    /// `None`: everyone who can read the directory.
    #[serde(default)]
    pub writers: Option<Vec<String>>,
    #[serde(default)]
    pub approvers: Vec<String>,
    #[serde(default)]
    pub can_edit: bool,
}

/// The repo (and server) a command talks to.
pub struct RepoTarget {
    pub remote: String,
    pub owner: String,
    pub repo: String,
    token: Option<String>,
}

impl RepoTarget {
    /// `--repo OWNER/NAME` (with `--remote`), or the checkout at `cwd`.
    pub fn resolve(cwd: &Path, repo: Option<&str>, remote: Option<&str>) -> Result<Self> {
        if let Some(slug) = repo {
            let (owner, name) = slug
                .split_once('/')
                .filter(|(o, n)| !o.is_empty() && !n.is_empty() && !n.contains('/'))
                .ok_or_else(|| {
                    OakError::InvalidArgument(format!("--repo must be OWNER/NAME, got {slug:?}"))
                })?;
            let remote = match remote {
                Some(r) => super::push::normalize_remote_url(r).ok_or_else(|| {
                    OakError::InvalidArgument("--remote cannot be empty".to_string())
                })?,
                None => super::push::env_remote_override()
                    .unwrap_or_else(|| super::push::DEFAULT_REMOTE.to_string()),
            };
            let remote = remote.trim_end_matches('/').to_string();
            return Ok(Self {
                token: super::credentials::effective_token(&remote),
                remote,
                owner: owner.to_string(),
                repo: name.to_string(),
            });
        }
        if remote.is_some() {
            return Err(OakError::InvalidArgument(
                "--remote applies with --repo; in a checkout the checkout's remote is used"
                    .to_string(),
            ));
        }
        let workspace = super::branch::RemoteWorkspace::open(cwd)?;
        let remote = workspace.remote;
        Ok(Self {
            remote: remote.remote_url.trim_end_matches('/').to_string(),
            owner: remote.owner,
            repo: remote.repo_name,
            token: remote.token,
        })
    }

    /// The checkout's repo, without failing: `None` when it has no remote.
    pub fn of_checkout(repo: &dyn Repository) -> Option<Self> {
        use oak_core::MetadataKey;
        let remote = repo
            .get_metadata(MetadataKey::RemoteUrl)
            .ok()
            .flatten()?
            .trim_end_matches('/')
            .to_string();
        Some(Self {
            token: super::credentials::effective_token(&remote),
            owner: repo.get_metadata(MetadataKey::RepoOwner).ok().flatten()?,
            repo: repo.get_metadata(MetadataKey::RepoName).ok().flatten()?,
            remote,
        })
    }

    /// `ORG/REPO` on `remote`; `None` when `spec` isn't one.
    pub fn for_spec(remote: &str, spec: &str) -> Option<Self> {
        let (owner, repo) = spec
            .trim()
            .split_once('/')
            .filter(|(o, n)| !o.is_empty() && !n.is_empty() && !n.contains('/'))?;
        let remote = remote.trim_end_matches('/').to_string();
        Some(Self {
            token: super::credentials::effective_token(&remote),
            remote,
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
    }

    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    fn url(&self, tail: &str) -> String {
        format!(
            "{}/api/{}/{}/{tail}",
            self.remote,
            urlencoding::encode(&self.owner),
            urlencoding::encode(&self.repo),
        )
    }

    fn get(&self, url: String, timeout: Duration) -> reqwest::RequestBuilder {
        let req = crate::http::api_client().get(url).timeout(timeout);
        match &self.token {
            Some(token) => req.header("authorization", format!("Bearer {token}")),
            None => req,
        }
    }
}

fn server_error(target: &RepoTarget, status: reqwest::StatusCode, body: &[u8]) -> OakError {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return OakError::AuthRequired {
            remote: target.remote.clone(),
            status: status.as_u16(),
        };
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return OakError::Server(format!(
            "{} has no ownership boundaries you can see: teams aren't on for it, or it isn't \
             visible to you (HTTP 404)",
            target.slug()
        ));
    }
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| format!("HTTP {status}"));
    OakError::Server(message)
}

/// Every boundary the caller can see. `Ok(None)` when the server has none to
/// offer (teams off for the repo, or an older server).
pub async fn fetch_boundaries(
    target: &RepoTarget,
    timeout: Duration,
) -> Result<Option<Vec<Boundary>>> {
    let response = target
        .get(target.url("boundaries"), timeout)
        .send()
        .await
        .map_err(|e| OakError::Http(format!("reading boundaries: {e}")))?;
    let status = response.status();
    let body = bounded_body(response).await?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(server_error(target, status, &body));
    }
    #[derive(Deserialize)]
    struct List {
        boundaries: Vec<Boundary>,
    }
    let mut list: List = serde_json::from_slice(&body)
        .map_err(|_| OakError::Server("invalid boundaries response".to_string()))?;
    list.boundaries.truncate(MAX_BOUNDARIES);
    Ok(Some(list.boundaries))
}

/// Does the server serve partial (view) checkouts of this repo? It does
/// whenever it has boundaries for it: both come with teams.
pub async fn serves_views(target: &RepoTarget) -> bool {
    matches!(
        fetch_boundaries(target, crate::http::read_only_probe_timeout()).await,
        Ok(Some(_))
    )
}

fn covers(prefix: &str, path: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || (path.len() > prefix.len()
            && path.as_bytes()[prefix.len()] == b'/'
            && path.starts_with(prefix))
}

/// The deepest boundary covering `path`.
pub fn owner_of<'a>(boundaries: &'a [Boundary], path: &str) -> Option<&'a Boundary> {
    boundaries
        .iter()
        .filter(|b| covers(&b.path, path))
        .max_by_key(|b| b.path.len())
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_string()
}

fn display_path(path: &str) -> String {
    if path.is_empty() {
        "/".to_string()
    } else {
        format!("{path}/")
    }
}

fn access_label(list: &Option<Vec<String>>, default: &str) -> String {
    match list {
        None => default.to_string(),
        Some(list) if list.is_empty() => "org owners and admins".to_string(),
        Some(list) => list.join(", "),
    }
}

#[derive(Serialize)]
struct AccessJson<'a> {
    schema_version: u32,
    repo: String,
    boundaries: &'a [Boundary],
    recommended_next_commands: Vec<String>,
}

/// `oak access`.
pub async fn access(target: RepoTarget, json: bool) -> Result<()> {
    let boundaries = fetch_boundaries(&target, REQUEST_TIMEOUT)
        .await?
        .ok_or_else(|| server_error(&target, reqwest::StatusCode::NOT_FOUND, &[]))?;
    if json {
        return output::print_json(&AccessJson {
            schema_version: SCHEMA_VERSION,
            repo: target.slug(),
            boundaries: &boundaries,
            recommended_next_commands: vec!["oak owners PATH --json".to_string()],
        });
    }
    if boundaries.is_empty() {
        output::print_line(&format!(
            "{} has no ownership boundaries: repo access applies everywhere, and no approvals \
             are needed.",
            target.slug()
        ));
        return Ok(());
    }
    let mut sorted = boundaries.clone();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    for b in &sorted {
        output::print_line(&format!(
            "{:<32} @team/{}  approval: {}{}",
            display_path(&b.path),
            b.owner_team,
            b.approval,
            if b.self_approve {
                " (owners may approve their own)"
            } else {
                ""
            }
        ));
        output::print_line(&format!(
            "{:<32} read: {}  write: {}",
            "",
            access_label(&b.readers, "everyone who can read the repo"),
            access_label(&b.writers, "everyone who can read it"),
        ));
    }
    Ok(())
}

#[derive(Serialize)]
struct OwnersJson {
    schema_version: u32,
    repo: String,
    path: String,
    /// The deepest boundary covering the path; `null` when none does.
    boundary: Option<Boundary>,
}

/// `oak owners PATH`.
pub async fn owners(target: RepoTarget, path: &str, json: bool) -> Result<()> {
    let path = normalize(path);
    let boundaries = fetch_boundaries(&target, REQUEST_TIMEOUT)
        .await?
        .ok_or_else(|| server_error(&target, reqwest::StatusCode::NOT_FOUND, &[]))?;
    let boundary = owner_of(&boundaries, &path).cloned();
    if json {
        return output::print_json(&OwnersJson {
            schema_version: SCHEMA_VERSION,
            repo: target.slug(),
            path,
            boundary,
        });
    }
    match boundary {
        Some(b) => output::print_line(&format!(
            "{} is in {}, owned by @team/{} (approval: {})",
            display_path(&path).trim_end_matches('/'),
            display_path(&b.path),
            b.owner_team,
            b.approval
        )),
        None => output::print_line(&format!(
            "No boundary covers {}: repo access applies, and no approval is needed.",
            display_path(&path).trim_end_matches('/')
        )),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Impact: the boundaries a branch touches
// ---------------------------------------------------------------------------

/// The boundaries a set of changed paths falls in, with their approval state.
/// The same block rides on `oak finish --json`, `oak merge --json` and mount
/// finish.
#[derive(Serialize, Debug, Clone, Default)]
pub struct ImpactJson {
    pub touched_boundaries: Vec<TouchedBoundaryJson>,
    /// Paths of `required` boundaries still needing an approval.
    pub missing_approvals: Vec<String>,
    /// Boundaries you can't see that also still need one. They block the
    /// merge the same, and are never named.
    pub hidden_missing_approvals: usize,
    /// The server gates this branch on approvals.
    pub approvals_enforced: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub caveats: Vec<String>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct TouchedBoundaryJson {
    pub path: String,
    pub owner_team: String,
    pub approval: String,
    pub approvers: Vec<String>,
    pub changed_files: usize,
    /// `None` when the server didn't say (the branch isn't pushed yet).
    pub approved: Option<bool>,
}

/// The impact of `paths` over `boundaries`; `None` when the repo has none.
pub fn impact_over<'a>(
    boundaries: &[Boundary],
    paths: impl IntoIterator<Item = &'a str>,
) -> Option<ImpactJson> {
    if boundaries.is_empty() {
        return None;
    }
    let mut touched: BTreeMap<&str, (usize, &Boundary)> = BTreeMap::new();
    for path in paths {
        if let Some(b) = owner_of(boundaries, path) {
            touched.entry(b.path.as_str()).or_insert((0, b)).0 += 1;
        }
    }
    Some(ImpactJson {
        touched_boundaries: touched
            .into_values()
            .map(|(changed_files, b)| TouchedBoundaryJson {
                path: b.path.clone(),
                owner_team: b.owner_team.clone(),
                approval: b.approval.clone(),
                approvers: b.approvers.clone(),
                changed_files,
                approved: None,
            })
            .collect(),
        ..ImpactJson::default()
    })
}

impl ImpactJson {
    /// Fold in the server's approval state for the branch, or say it's
    /// unknown.
    pub fn with_approvals(mut self, approvals: Option<Value>) -> Self {
        let Some(approvals) = approvals else {
            self.caveats.push(
                "approval state is unknown here; the branch may not be pushed, or the server \
                 didn't answer"
                    .to_string(),
            );
            return self;
        };
        self.approvals_enforced = approvals["enforced"].as_bool() == Some(true);
        self.missing_approvals = super::approve::parse_missing(&approvals)
            .into_iter()
            .map(|m| m.path)
            .collect();
        self.hidden_missing_approvals =
            approvals["hidden_missing_approvals"].as_u64().unwrap_or(0) as usize;
        let approved: BTreeMap<String, bool> = approvals["boundaries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| Some((b["path"].as_str()?.to_string(), b["approved"].as_bool()?)))
            .collect();
        for t in &mut self.touched_boundaries {
            t.approved = approved.get(&t.path).copied();
        }
        self
    }
}

/// Which branch of which repo a checkout's approvals live on:
/// `(remote, owner, repo, branch)`.
pub fn approvals_source(repo: &dyn Repository) -> Option<(String, String, String, String)> {
    use oak_core::MetadataKey;
    Some((
        repo.get_metadata(MetadataKey::RemoteUrl).ok().flatten()?,
        repo.get_metadata(MetadataKey::RepoOwner).ok().flatten()?,
        repo.get_metadata(MetadataKey::RepoName).ok().flatten()?,
        repo.get_current_branch_name().ok().flatten()?,
    ))
}

/// The server's approval state for a branch; `None` when it can't say (not
/// pushed, an older server, offline).
pub async fn fetch_approvals(
    (remote, owner, repo, branch): (String, String, String, String),
) -> Option<Value> {
    let url = format!(
        "{}/api/{}/{}/branches/{}/approvals",
        remote.trim_end_matches('/'),
        urlencoding::encode(&owner),
        urlencoding::encode(&repo),
        super::branch_api_segment(&branch),
    );
    let mut request = crate::http::api_client()
        .get(url)
        .timeout(crate::http::read_only_probe_timeout());
    if let Some(token) = super::credentials::effective_token(&remote) {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// The impact block for `paths` in the checkout `repo`: the boundaries from
/// the server, plus the branch's approval state. `None` when the repo has no
/// boundaries (or the server can't say).
pub async fn impact_for(repo: &dyn Repository, paths: &[String]) -> Option<ImpactJson> {
    let target = RepoTarget::of_checkout(repo)?;
    let boundaries = fetch_boundaries(&target, crate::http::read_only_probe_timeout())
        .await
        .ok()
        .flatten()?;
    let impact = impact_over(&boundaries, paths.iter().map(String::as_str))?;
    let approvals = match approvals_source(repo) {
        Some(source) => fetch_approvals(source).await,
        None => None,
    };
    Some(impact.with_approvals(approvals))
}

/// Every path the current branch changes: its committed contribution since
/// the fork point plus uncommitted work.
pub fn branch_changed_paths(repo: &dyn Repository, work_tree: &Path) -> Result<Vec<String>> {
    let (changes, head, branch_name) =
        crate::commands::commit::compute_changes_for_status(repo, work_tree)?;
    let mut paths: BTreeSet<String> = changes.into_iter().map(|c| c.path).collect();
    if let (Some(branch_name), Some(head)) = (branch_name.as_deref(), head.as_ref()) {
        if let Some(branch) = repo.get_branch(branch_name)? {
            let against = branch
                .parent_branch
                .as_deref()
                .unwrap_or(oak_core::DEFAULT_BRANCH);
            if branch_name != against {
                let (_, base_root, _) =
                    crate::commands::review::branch_fork_base_root(repo, branch_name, against)?;
                let head_root = crate::commands::review::root_for_head(repo, Some(head))?;
                for change in crate::commands::review::diff_roots_with_content_renames(
                    repo, &base_root, &head_root,
                )? {
                    if let Some(old) = change.old_path {
                        paths.insert(old);
                    }
                    paths.insert(change.path);
                }
            }
        }
    }
    Ok(paths.into_iter().collect())
}

/// `oak owners --branch`.
pub async fn branch_impact(cwd: &Path, json: bool) -> Result<()> {
    let ctx = crate::resolve::resolve(cwd)?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    let paths = branch_changed_paths(&repo, &ctx.work_tree)?;
    let target = RepoTarget::of_checkout(&repo).ok_or_else(|| {
        OakError::InvalidArgument("this checkout has no remote to ask".to_string())
    })?;
    let boundaries = fetch_boundaries(&target, REQUEST_TIMEOUT).await?;
    let impact = match boundaries {
        Some(boundaries) => {
            let approvals = match approvals_source(&repo) {
                Some(source) => fetch_approvals(source).await,
                None => None,
            };
            impact_over(&boundaries, paths.iter().map(String::as_str))
                .map(|i| i.with_approvals(approvals))
        }
        None => None,
    };
    if json {
        #[derive(Serialize)]
        struct BranchImpactJson {
            schema_version: u32,
            changed_paths: usize,
            /// `null` when the repo has no boundaries.
            impact: Option<ImpactJson>,
            recommended_next_commands: Vec<String>,
        }
        let recommended_next_commands = match &impact {
            Some(i) if !i.missing_approvals.is_empty() || i.hidden_missing_approvals > 0 => {
                vec!["oak open --print".to_string()]
            }
            _ => Vec::new(),
        };
        return output::print_json(&BranchImpactJson {
            schema_version: SCHEMA_VERSION,
            changed_paths: paths.len(),
            impact,
            recommended_next_commands,
        });
    }
    let Some(impact) = impact else {
        output::print_line("This repo has no ownership boundaries: no approvals are needed.");
        return Ok(());
    };
    if impact.touched_boundaries.is_empty() {
        output::print_line("The branch touches no boundary.");
    }
    for t in &impact.touched_boundaries {
        let state = match (t.approval.as_str(), t.approved) {
            ("off", _) => String::new(),
            (_, Some(true)) => "  ✓ approved".to_string(),
            (_, Some(false)) => "  needs approval".to_string(),
            (_, None) => String::new(),
        };
        output::print_line(&format!(
            "touches {:<28} {} file(s)  @team/{} ({}){state}",
            display_path(&t.path),
            t.changed_files,
            t.owner_team,
            t.approval,
        ));
    }
    if impact.hidden_missing_approvals > 0 {
        output::print_line(&format!(
            "still needs approval: {} boundary(ies) you can't see",
            impact.hidden_missing_approvals
        ));
    }
    for c in &impact.caveats {
        output::warning(c);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boundary(id: i64, path: &str, team: &str, approval: &str) -> Boundary {
        Boundary {
            id,
            path: path.to_string(),
            owner_team: team.to_string(),
            approval: approval.to_string(),
            self_approve: false,
            readers: None,
            writers: None,
            approvers: vec![format!("@team/{team}")],
            can_edit: false,
        }
    }

    #[test]
    fn the_deepest_boundary_owns_a_path() {
        let all = [
            boundary(1, "", "platform", "required"),
            boundary(2, "apps", "eng", "advisory"),
            boundary(3, "apps/web", "web", "required"),
        ];
        assert_eq!(owner_of(&all, "apps/web/src/a.ts").unwrap().id, 3);
        assert_eq!(owner_of(&all, "apps/webby/a.ts").unwrap().id, 2);
        assert_eq!(owner_of(&all, "README.md").unwrap().id, 1);
        assert!(owner_of(&all[1..], "README.md").is_none());
    }

    #[test]
    fn impact_counts_files_per_boundary_and_folds_in_approvals() {
        let all = [
            boundary(1, "", "platform", "off"),
            boundary(3, "apps/web", "web", "required"),
            boundary(4, "services/billing", "billing", "required"),
        ];
        let impact = impact_over(
            &all,
            [
                "apps/web/a.ts",
                "apps/web/b.ts",
                "services/billing/lib.rs",
                "README.md",
            ],
        )
        .unwrap();
        let touched: Vec<(&str, usize)> = impact
            .touched_boundaries
            .iter()
            .map(|t| (t.path.as_str(), t.changed_files))
            .collect();
        assert_eq!(touched, [("", 1), ("apps/web", 2), ("services/billing", 1)]);
        let impact = impact.with_approvals(Some(serde_json::json!({
            "enforced": true,
            "boundaries": [
                {"path": "apps/web", "approved": true},
                {"path": "services/billing", "approved": false},
            ],
            "missing_approvals": [
                {"path": "services/billing", "owner_team": "billing", "approvers": ["@team/billing"]}
            ],
            "hidden_missing_approvals": 1,
        })));
        assert!(impact.approvals_enforced);
        assert_eq!(impact.missing_approvals, ["services/billing"]);
        assert_eq!(impact.hidden_missing_approvals, 1);
        assert_eq!(impact.touched_boundaries[1].approved, Some(true));
        assert_eq!(impact.touched_boundaries[2].approved, Some(false));
        assert_eq!(impact.touched_boundaries[0].approved, None);
        let unknown = impact.clone().with_approvals(None);
        assert!(unknown.caveats.iter().any(|c| c.contains("unknown")));
        assert!(impact_over(&[], ["x"]).is_none());
    }

    #[test]
    fn boundaries_parse_with_defaults() {
        let parsed: Boundary = serde_json::from_value(serde_json::json!({
            "id": 12, "path": "services/billing", "owner_team": "billing",
            "approval": "required", "readers": null, "writers": ["@team/billing"],
        }))
        .unwrap();
        assert_eq!(parsed.readers, None);
        assert_eq!(
            parsed.writers.as_deref(),
            Some(&["@team/billing".to_string()][..])
        );
        assert!(!parsed.self_approve && !parsed.can_edit && parsed.approvers.is_empty());
    }
}
