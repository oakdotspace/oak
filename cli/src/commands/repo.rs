use std::fs;
use std::path::{Path, PathBuf};

use oak_core::protocol::{BranchPullData, PullResponse};
use oak_core::{
    Branch, BranchStatus, FileMode, Hash, ManifestEntry, MetadataKey, OakError, Result, SparseCone,
};
use oak_core::{Repository, SqliteRepository};
use serde::{Deserialize, Serialize};

use dialoguer::{Confirm, FuzzySelect, Input, Select};

use crate::output;

#[derive(Deserialize)]
struct RepoListResponse {
    repos: Vec<RepoInfo>,
}

#[derive(Deserialize)]
struct RepoInfo {
    name: String,
    description: Option<String>,
    head: Option<String>,
    owner: Option<String>,
    updated_at: Option<String>,
}

/// Format a relative time string from an RFC3339 timestamp
fn format_relative_time(timestamp: &str) -> String {
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
        return String::new();
    };
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(dt);

    if duration.num_days() > 365 {
        format!("{} year(s) ago", duration.num_days() / 365)
    } else if duration.num_days() > 30 {
        format!("{} month(s) ago", duration.num_days() / 30)
    } else if duration.num_days() > 0 {
        format!("{} day(s) ago", duration.num_days())
    } else if duration.num_hours() > 0 {
        format!("{} hour(s) ago", duration.num_hours())
    } else if duration.num_minutes() > 0 {
        format!("{} minute(s) ago", duration.num_minutes())
    } else {
        "just now".to_string()
    }
}

/// List repositories on the server
pub async fn list(remote: &str, sort: &str) -> Result<()> {
    let client = crate::http::api_client();

    let resp = client
        .get(format!("{remote}/api/repos"))
        .query(&[("sort", sort)])
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(crate::http::server_error(resp).await);
    }

    let list: RepoListResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if list.repos.is_empty() {
        output::info("No repositories found");
        return Ok(());
    }

    output::info("Repositories:");
    output::print_line("");

    for repo in list.repos {
        let desc = repo.description.unwrap_or_default();
        let head = repo
            .head
            .map(|h| format!(" ({})", &h[..12.min(h.len())]))
            .unwrap_or_default();
        let updated = repo
            .updated_at
            .as_deref()
            .map(|t| format!(" [{}]", format_relative_time(t)))
            .unwrap_or_default();

        if desc.is_empty() {
            output::print_line(&format!("  {}{}{}", repo.name, head, updated));
        } else {
            output::print_line(&format!("  {}{} - {}{}", repo.name, head, desc, updated));
        }
    }

    Ok(())
}

#[derive(Clone)]
struct CloneChoice {
    owner: String,
    repo: String,
    label: String,
    detail: String,
}

/// A trusted host move hit during clone: there's no local repo to retarget
/// yet (the clone stores whichever remote it ends up using), so just carry
/// the old origin's login over — `get_token_for_server` keys off the server
/// URL — and let the caller retry against `origin`.
fn note_clone_remote_move(old_remote: &str, origin: &str) -> Result<()> {
    output::info(&format!(
        "Remote {old_remote} has moved to {origin} — retrying against the new origin"
    ));
    if super::credentials::migrate_server_credential(old_remote, origin)? {
        output::info(&format!("Carried your {old_remote} login over to {origin}"));
    }
    Ok(())
}

/// Interactive clone picker used by `oak clone` with no repo argument.
pub async fn clone_interactive(remote: &str, cwd: &Path, shallow: bool) -> Result<()> {
    clone_interactive_with_policy(
        remote,
        cwd,
        shallow,
        super::integrity::CloneIntegrityPolicy::default(),
    )
    .await
}

/// Interactive clone picker with the same explicit integrity policy accepted
/// by named Oak clones. The picker chooses an Oak repository before this
/// policy reaches preflight, so overrides do not need a positional repo name.
pub async fn clone_interactive_with_policy(
    remote: &str,
    cwd: &Path,
    shallow: bool,
    integrity_policy: super::integrity::CloneIntegrityPolicy,
) -> Result<()> {
    // Follow a trusted host move before the picker, so both the repo list
    // and the clone below run against the new origin.
    let (remote, choices) = match fetch_clone_choices(remote).await {
        Err(OakError::RemoteMoved { origin }) if crate::http::is_trusted_origin(&origin) => {
            note_clone_remote_move(remote, &origin)?;
            let choices = fetch_clone_choices(&origin).await?;
            (origin, choices)
        }
        other => (remote.to_string(), other?),
    };
    let remote = remote.as_str();
    if choices.is_empty() {
        output::info("No repositories found.");
        return Ok(());
    }

    let labels: Vec<String> = choices
        .iter()
        .map(|c| {
            if c.detail.is_empty() {
                c.label.clone()
            } else {
                format!("{}  {}", c.label, c.detail)
            }
        })
        .collect();
    let idx = FuzzySelect::new()
        .with_prompt("Search repos")
        .items(&labels)
        .default(0)
        .interact()
        .map_err(|e| OakError::Server(e.to_string()))?;
    let choice = &choices[idx];

    let default_dest = cwd.join(&choice.repo);
    let dest_actions = [
        format!("Use {}", default_dest.display()),
        "Enter a different destination".to_string(),
    ];
    let dest_idx = Select::new()
        .with_prompt("Destination")
        .items(&dest_actions)
        .default(0)
        .interact()
        .map_err(|e| OakError::Server(e.to_string()))?;
    let dest = if dest_idx == 0 {
        default_dest
    } else {
        let typed: String = Input::new()
            .with_prompt("Destination path")
            .with_initial_text(default_dest.display().to_string())
            .allow_empty(false)
            .interact_text()
            .map_err(|e| OakError::Server(e.to_string()))?;
        let path = Path::new(typed.trim());
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    };

    let spec = format!("{}/{}", choice.owner, choice.repo);
    clone_repo_sparse_on_branch_with_policy(
        remote,
        &spec,
        &dest,
        shallow,
        None,
        None,
        None,
        integrity_policy,
    )
    .await
}

async fn fetch_clone_choices(remote: &str) -> Result<Vec<CloneChoice>> {
    let client = crate::http::api_client();
    let token = super::credentials::effective_token(remote, None);

    let mut req = client
        .get(format!("{remote}/api/repos"))
        .query(&[("sort", "updated")]);
    if let Some(ref t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(crate::http::server_error(resp).await);
    }

    let list: RepoListResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    let mut choices = Vec::new();
    for repo in list.repos {
        let owner = repo.owner.clone().unwrap_or_else(|| {
            super::credentials::get_username_for_server(remote).unwrap_or_default()
        });
        if owner.is_empty() {
            continue;
        }
        let detail = repo
            .description
            .as_deref()
            .filter(|d| !d.is_empty())
            .unwrap_or_default()
            .to_string();
        choices.push(CloneChoice {
            owner: owner.clone(),
            repo: repo.name.clone(),
            label: format!("{}/{}", owner, repo.name),
            detail,
        });
    }
    Ok(choices)
}

/// Create a new repository on the server. `spec` is `owner/name`.
///
/// The owner segment is always sent to the server as `organization_slug`. The
/// server looks it up; if it's an organization the caller belongs to, the repo is
/// owned by that organization. If it's not an organization (e.g. the caller's
/// username), the server rejects the request with a clear error.
pub async fn create(remote: &str, spec: &str, description: Option<&str>) -> Result<()> {
    let (owner, name) = super::parse_owner_repo(spec)?;
    let client = crate::http::api_client();

    let token = super::credentials::effective_token(remote, None);

    let mut req = client
        .post(format!("{remote}/api/repos"))
        .json(&serde_json::json!({
            "name": name,
            "description": description,
            "organization_slug": owner,
        }));
    if let Some(ref t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if resp.status().as_u16() == 409 {
        return Err(OakError::RemoteRepoAlreadyExists(format!("{owner}/{name}")));
    }

    if !resp.status().is_success() {
        return Err(crate::http::server_error(resp).await);
    }

    output::success(&format!("Created repository '{owner}/{name}'"));
    Ok(())
}

/// Clone a repository from the server. `spec` is `owner/name`, or a bare
/// `name` — in which case the logged-in user's username (from credentials for
/// `remote`) is used as the owner, so `oak clone foo` works for repos in your
/// personal organization.
pub async fn clone_repo(remote: &str, spec: &str, dest: &Path, shallow: bool) -> Result<()> {
    clone_repo_sparse(remote, spec, dest, shallow, None).await
}

/// Clone, optionally scoping the working tree to a [`SparseCone`] (a
/// Perforce-style partial clone). `sparse: None` is a full checkout; `Some`
/// stores the cone in the repo and asks the server to ship only the blobs
/// reachable from those paths.
pub async fn clone_repo_sparse(
    remote: &str,
    spec: &str,
    dest: &Path,
    shallow: bool,
    sparse: Option<SparseCone>,
) -> Result<()> {
    clone_repo_sparse_on_branch(remote, spec, dest, shallow, sparse, None).await
}

/// Clone with an optional branch selection. Capable servers bind and
/// materialize that branch directly; legacy servers retain the original
/// clone-then-fetch behavior. For shallow clones it also selects the single
/// history root fetched from the server.
pub async fn clone_repo_sparse_on_branch(
    remote: &str,
    spec: &str,
    dest: &Path,
    shallow: bool,
    sparse: Option<SparseCone>,
    branch: Option<&str>,
) -> Result<()> {
    clone_repo_sparse_on_branch_with_policy(
        remote,
        spec,
        dest,
        shallow,
        sparse,
        branch,
        None,
        super::integrity::CloneIntegrityPolicy::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn clone_repo_sparse_on_branch_with_policy(
    remote: &str,
    spec: &str,
    dest: &Path,
    shallow: bool,
    sparse: Option<SparseCone>,
    branch: Option<&str>,
    expected_head: Option<&str>,
    integrity_policy: super::integrity::CloneIntegrityPolicy,
) -> Result<()> {
    clone_repo_sparse_on_branch_with_policy_and_output(
        remote,
        spec,
        dest,
        shallow,
        sparse,
        branch,
        expected_head,
        integrity_policy,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn clone_repo_sparse_on_branch_with_policy_and_output(
    remote: &str,
    spec: &str,
    dest: &Path,
    shallow: bool,
    sparse: Option<SparseCone>,
    branch: Option<&str>,
    expected_head: Option<&str>,
    integrity_policy: super::integrity::CloneIntegrityPolicy,
    json: bool,
) -> Result<()> {
    // `--branch` means "switch to this working branch after clone". Main is
    // intentionally server-only, so reject it before credentials, network,
    // destination creation, or personal-branch initialization can happen.
    // A read-only main snapshot can be added later as an explicit detached
    // mode without changing this mutation-free preflight contract.
    if branch == Some("main") {
        return Err(OakError::InvalidArgument(
            "`main` only exists on the server — omit `--branch main` to clone its current tree onto a fresh personal branch"
                .to_string(),
        ));
    }

    let username = super::credentials::get_username_for_server(remote);
    let (owner, name) = resolve_clone_spec(remote, spec, username.as_deref())?;

    // A redirect to a trusted Oak host means the remote has moved; retry
    // once against the new origin. Safe to re-run from scratch: the failed
    // attempt's cleanup already removed the `.oak` dir it created.
    let options = CloneOptions {
        shallow,
        sparse,
        branch,
        expected_head,
        integrity_policy,
        auth_token: super::integrity::effective_token(remote),
        json,
    };
    match get_single_repo(remote, &owner, &name, dest, options.clone()).await {
        Err(OakError::RemoteMoved { origin }) if crate::http::is_trusted_origin(&origin) => {
            note_clone_remote_move(remote, &origin)?;
            let mut redirected_options = options;
            redirected_options.auth_token = super::integrity::effective_token(&origin);
            get_single_repo(&origin, &owner, &name, dest, redirected_options).await
        }
        result => result,
    }
}

#[derive(Clone)]
struct CloneOptions<'a> {
    shallow: bool,
    sparse: Option<SparseCone>,
    branch: Option<&'a str>,
    expected_head: Option<&'a str>,
    integrity_policy: super::integrity::CloneIntegrityPolicy,
    /// One normalized credential is bound to capability, proof, pull, and
    /// persisted metadata for the entire clone attempt.
    auth_token: Option<String>,
    json: bool,
}

#[derive(Debug, Default, Serialize)]
struct CloneObjectCounts {
    commits: u64,
    trees: u64,
    blob_descriptors: u64,
}

#[derive(Debug, Default, Serialize)]
struct CloneChunkCounts {
    required_unique: u64,
    downloaded_unique: u64,
    downloaded_logical_bytes: u64,
    present_at_clone_start_unique: u64,
    reused_from_prior_phase_unique: u64,
}

#[derive(Debug, Serialize)]
struct CloneMaterializationCounts {
    passes_observed: u64,
    path_counters_scope: &'static str,
    observed_eligible_paths: u64,
    written_paths: u64,
    logical_bytes: u64,
    restricted_paths: u64,
    known_lost_paths: u64,
    missing_required_paths: u64,
    observed_out_of_cone_paths: u64,
}

impl Default for CloneMaterializationCounts {
    fn default() -> Self {
        Self {
            passes_observed: 0,
            path_counters_scope: "final_clone_materialization",
            observed_eligible_paths: 0,
            written_paths: 0,
            logical_bytes: 0,
            restricted_paths: 0,
            known_lost_paths: 0,
            missing_required_paths: 0,
            observed_out_of_cone_paths: 0,
        }
    }
}

#[derive(Debug, Default, Serialize)]
struct ClonePhaseChunkCounts {
    required_unique: u64,
    downloaded_unique: u64,
    downloaded_logical_bytes: u64,
    valid_before_phase_fetch_unique: u64,
}

#[derive(Debug, Serialize)]
struct ClonePullPhase {
    name: &'static str,
    response_body_bytes: Option<u64>,
    counters_complete: bool,
    object_entries_received: CloneObjectCounts,
    chunks: ClonePhaseChunkCounts,
    materialization_passes: u64,
}

#[derive(Debug, Serialize)]
struct CloneHistoryRequest {
    mode: &'static str,
    depth: Option<u8>,
}

#[derive(Debug, Serialize)]
struct CloneMaterializationRequest {
    mode: &'static str,
    prefix_count: usize,
}

#[derive(Debug, Serialize)]
struct CloneReceiptRequest<'a> {
    history: CloneHistoryRequest,
    selected_branch: Option<&'a str>,
    expected_head: Option<&'a str>,
    materialization: CloneMaterializationRequest,
}

#[derive(Debug, Serialize)]
struct CloneReceiptRepository<'a> {
    origin: Option<String>,
    owner: &'a str,
    name: &'a str,
}

#[derive(Debug, Serialize)]
struct CloneReceiptResult {
    local_branch: String,
    head: Option<Hash>,
    manifest: Option<Hash>,
    acquisition_profile: &'static str,
    preflight_profile: &'static str,
    proof_scope: &'static str,
    snapshot_bound: bool,
    initial_pull_snapshot_bound: bool,
    selected_branch_snapshot_bound: Option<bool>,
    preflight_credential_disposition: &'static str,
}

#[derive(Debug, Serialize)]
struct CloneReceiptObserved {
    elapsed_ms: f64,
    pull_response_body_bytes_observed: u64,
    pull_phase_counters_complete: bool,
    pull_phases: Vec<ClonePullPhase>,
    object_entries_received: CloneObjectCounts,
    chunks: CloneChunkCounts,
    materialization: CloneMaterializationCounts,
}

#[derive(Debug, Serialize)]
struct CloneReceiptUnknown {
    field: &'static str,
    reason: &'static str,
}

#[derive(Debug, Serialize)]
struct CloneReceipt<'a> {
    schema_version: u8,
    operation: &'static str,
    repository: CloneReceiptRepository<'a>,
    request: CloneReceiptRequest<'a>,
    result: CloneReceiptResult,
    observed: CloneReceiptObserved,
    unknown: Vec<CloneReceiptUnknown>,
}

struct ClonePullRequest<'a> {
    remote: &'a str,
    owner: &'a str,
    name: &'a str,
    shallow: bool,
    branch: Option<&'a str>,
    selected_branch: Option<&'a str>,
    expected_head: Option<&'a str>,
    sparse: Option<&'a SparseCone>,
    auth_token: Option<&'a str>,
    snapshot_token: Option<&'a str>,
    known_loss_protocol: bool,
    sparse_materialization_v1: bool,
}

struct CloneDestinationGuard {
    repo_dir: PathBuf,
    oak_dir: PathBuf,
    remove_repo_dir: bool,
    armed: std::cell::Cell<bool>,
}

impl CloneDestinationGuard {
    fn new(repo_dir: PathBuf, oak_dir: PathBuf, remove_repo_dir: bool) -> Self {
        Self {
            repo_dir,
            oak_dir,
            remove_repo_dir,
            armed: std::cell::Cell::new(true),
        }
    }

    fn cleanup(&self) {
        if !self.armed.get() {
            return;
        }
        let result = if self.remove_repo_dir {
            fs::remove_dir_all(&self.repo_dir)
        } else {
            fs::remove_dir_all(&self.oak_dir)
        };
        if result.is_ok()
            || matches!(result, Err(ref error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            self.armed.set(false);
        }
    }

    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for CloneDestinationGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn validate_pull_missing_content(
    preflight: &std::collections::HashSet<String>,
    declared: &[oak_core::protocol::MissingContentData],
) -> Result<Vec<String>> {
    let hashes = parse_pull_missing_content(declared)?;
    if &hashes != preflight {
        return Err(OakError::Server(
            "pull missing_content did not exactly match the snapshot-bound integrity preflight; retry clone"
                .to_string(),
        ));
    }
    let mut ordered: Vec<String> = hashes.into_iter().collect();
    ordered.sort();
    Ok(ordered)
}

fn validate_pinned_pull_selection(
    pull: &PullResponse,
    selected: Option<&super::integrity::SelectedBranchEvidence>,
) -> Result<Option<(String, oak_core::Hash)>> {
    let Some(selected) = selected else {
        return Ok(None);
    };
    let branches: Vec<_> = pull
        .branches
        .iter()
        .filter(|branch| branch.name == selected.name)
        .chain(
            pull.branch
                .iter()
                .filter(|branch| branch.name == selected.name),
        )
        .collect();
    let branch = branches.first().copied().ok_or_else(|| {
        OakError::Server(
            "pull omitted the snapshot-bound selected branch; clone was not created".to_string(),
        )
    })?;
    if branch.status != "open" {
        return Err(OakError::Server(
            "pull returned the snapshot-bound selected branch as non-open; clone was not created"
                .to_string(),
        ));
    }
    let head = oak_core::Hash::from_hex(&selected.head).map_err(|error| {
        OakError::Server(format!(
            "invalid snapshot-bound selected branch head: {error}"
        ))
    })?;
    if branches.iter().skip(1).any(|candidate| {
        candidate.description != branch.description
            || candidate.parent_branch != branch.parent_branch
            || candidate.status != branch.status
            || candidate.created_at != branch.created_at
            || candidate.close_reason != branch.close_reason
    }) {
        return Err(OakError::Server(
            "pull returned conflicting metadata for the snapshot-bound selected branch; clone was not created"
                .to_string(),
        ));
    }
    let matching_commits: Vec<_> = pull
        .commits
        .iter()
        .filter(|commit| commit.hash == selected.head)
        .collect();
    if matching_commits.len() != 1 {
        return Err(OakError::Server(format!(
            "pull omitted or duplicated selected branch '{}' exact head {}; clone was not created",
            selected.name, selected.head
        )));
    }
    let commit = super::pull::commit_from_pull_data(matching_commits[0]).map_err(|error| {
        OakError::Server(format!(
            "pull returned an invalid selected branch exact-head commit: {error}; clone was not created"
        ))
    })?;
    let root_matches = commit.manifest_hash == oak_core::Tree::empty_hash()
        || pull.trees.iter().any(|tree| {
            tree.hash == commit.manifest_hash.0
                && super::pull::wire_to_core_tree(tree)
                    .is_ok_and(|decoded| decoded.hash == commit.manifest_hash)
        });
    if !root_matches {
        return Err(OakError::Server(format!(
            "pull omitted selected branch '{}' exact-head manifest {}; clone was not created",
            selected.name, commit.manifest_hash
        )));
    }
    Ok(Some((selected.name.clone(), head)))
}

pub(super) fn parse_pull_missing_content(
    declared: &[oak_core::protocol::MissingContentData],
) -> Result<std::collections::HashSet<String>> {
    let mut hashes = std::collections::HashSet::with_capacity(declared.len());
    for item in declared {
        if item.kind != "blob"
            || item.reason_code != super::known_loss::OPERATOR_LOSS_REASON
            || oak_core::Hash::from_hex(&item.hash).is_err()
            || !hashes.insert(item.hash.clone())
        {
            return Err(OakError::Server(
                "pull returned an invalid or duplicate report_v1 missing_content item".to_string(),
            ));
        }
    }
    Ok(hashes)
}

async fn request_clone_pull(
    client: &reqwest::Client,
    request: ClonePullRequest<'_>,
) -> Result<reqwest::Response> {
    let pull_url = if request.shallow {
        format!(
            "{}/api/{}/{}/pull?depth=1",
            request.remote, request.owner, request.name
        )
    } else {
        format!(
            "{}/api/{}/{}/pull",
            request.remote, request.owner, request.name
        )
    };
    let mut req = client.get(pull_url);
    if request.shallow {
        if let Some(branch) = request.branch {
            req = req.query(&[("branch_name", branch)]);
        }
    }
    if let Some(selected_branch) = request.selected_branch {
        req = req.query(&[("selected_branch", selected_branch)]);
    }
    if let Some(expected_head) = request.expected_head {
        req = req.query(&[("expected_head", expected_head)]);
    }
    if let Some(token) = request.auth_token {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    if let Some(cone) = request.sparse {
        req = req.query(&[("paths", cone.prefixes().join(","))]);
    }
    if let Some(snapshot) = request.snapshot_token {
        req = req.query(&[("integrity_snapshot", snapshot)]);
    }
    if request.known_loss_protocol {
        req = req.query(&[(
            "known_loss_protocol",
            super::known_loss::KNOWN_LOSS_PROTOCOL,
        )]);
    }
    if request.sparse_materialization_v1 {
        req = req.query(&[(
            "sparse_materialization_protocol",
            super::integrity::SPARSE_MATERIALIZATION_PROTOCOL,
        )]);
    }
    req.send()
        .await
        .map_err(|error| OakError::Http(error.to_string()))
}

fn resolve_clone_spec(
    remote: &str,
    spec: &str,
    username: Option<&str>,
) -> Result<(String, String)> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(OakError::Server(
            "Repository spec is empty; expected '<repo>' or '<org>/<repo>'".to_string(),
        ));
    }

    let resolved = if spec.contains('/') {
        spec.to_string()
    } else {
        match username {
            Some(username) => format!("{username}/{spec}"),
            None => {
                return Err(OakError::Server(format!(
                    "No login for {remote}; run `oak login` first, or specify the org explicitly (e.g. `<org>/{spec}`)"
                )));
            }
        }
    };

    super::parse_owner_repo(&resolved)
}

/// Resolve a repo spec — `<org>/<repo>` verbatim, or a bare `<repo>` filled in
/// with the logged-in username as the owner — into a canonical `<owner>/<repo>`
/// string. This is the same resolution `oak clone` uses, so `oak mount foo`
/// and `oak clone foo` target the same repo in your personal org. Errors when
/// a bare repo is given but you're not logged in to `remote`.
pub fn resolve_repo_spec(remote: &str, spec: &str) -> Result<String> {
    let username = super::credentials::get_username_for_server(remote);
    let (owner, name) = resolve_clone_spec(remote, spec, username.as_deref())?;
    Ok(format!("{owner}/{name}"))
}

/// Clone a single repo from the server.
///
/// `shallow` selects between a full clone (the default — the entire history)
/// and a shallow clone that fetches only the newest commit on the selected
/// branch (the default branch when none is selected), like `git clone
/// --depth=1`. The selected commit's manifest is complete, but local history
/// plus preflight and recovery scope are intentionally narrower.
async fn get_single_repo(
    remote: &str,
    owner: &str,
    name: &str,
    dest: &Path,
    options: CloneOptions<'_>,
) -> Result<()> {
    let started = std::time::Instant::now();
    let CloneOptions {
        shallow,
        sparse,
        branch,
        expected_head,
        integrity_policy,
        auth_token,
        json,
    } = options;
    let client = crate::http::api_client();

    // Prove the requested history is servable before creating any destination
    // state or beginning the bulk pull download. Older servers return 404 for
    // this append-only endpoint and retain the legacy pull behavior.
    // Full clone downloads the all-history snapshot. Only a shallow clone
    // narrows the server-side history root to the selected branch, so its
    // history proof must use the same scope. Capable servers independently
    // bind either shape to the requested selected-branch identity.
    let preflight_branch = shallow.then_some(branch).flatten();
    let mut preflight = super::integrity::preflight_clone_acquisition_with_policy_and_token(
        remote,
        &format!("{owner}/{name}"),
        shallow,
        preflight_branch,
        branch,
        expected_head,
        sparse.as_ref().map(SparseCone::prefixes),
        integrity_policy,
        auth_token.as_deref(),
    )
    .await?;

    // Create destination directory
    let repo_dir = dest.to_path_buf();
    let oak_dir = repo_dir.join(".oak");

    // Check if a repository already exists at this location
    if oak_dir.exists() {
        return Err(OakError::RepoAlreadyExists);
    }

    // If we created the destination, we'll remove it on failure. If it was
    // already there (e.g. the user passed `.`), leave it alone — removing the
    // user's existing directory is rarely what they want and on some platforms
    // fails outright (e.g. removing the current working directory).
    let we_created_dest = !repo_dir.exists();
    if !we_created_dest {
        let metadata = fs::symlink_metadata(&repo_dir)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || fs::read_dir(&repo_dir)?.next().transpose()?.is_some()
        {
            return Err(OakError::InvalidArgument(format!(
                "clone destination '{}' is not empty or is not a plain directory (files and symlinks are refused)",
                repo_dir.display()
            )));
        }
    }
    fs::create_dir_all(&repo_dir)?;
    let destination_guard =
        CloneDestinationGuard::new(repo_dir.clone(), oak_dir.clone(), we_created_dest);
    let cleanup = || destination_guard.cleanup();

    // Initialize local repo
    fs::create_dir_all(&oak_dir)?;
    let clone_marker = oak_dir.join("CLONE_IN_PROGRESS");
    fs::write(
        &clone_marker,
        "clone has not finished publishing refs and working tree\n",
    )?;

    let db_path = oak_dir.join("oak.db");
    let repo = SqliteRepository::open(&db_path)?;

    // Save remote URL, owner, and repo name
    repo.set_metadata(MetadataKey::RemoteUrl, remote)?;
    repo.set_metadata(MetadataKey::RepoOwner, owner)?;
    repo.set_metadata(MetadataKey::RepoName, name)?;

    // Persist the sparse cone before materializing so `write_working_directory`
    // (and every later command) scopes the working tree to it.
    if let Some(ref cone) = sparse {
        repo.set_metadata(MetadataKey::SparsePaths, &cone.to_metadata())?;
    }

    if !json {
        output::info(&format!("Cloning '{owner}/{name}' from {remote}..."));
    }

    // Full clone by default: omit `depth` so the server returns the entire
    // history. `--shallow` asks for just the newest commit on the selected
    // branch (or the default branch when none was selected).
    let mut resp = request_clone_pull(
        &client,
        ClonePullRequest {
            remote,
            owner,
            name,
            shallow,
            branch,
            selected_branch: preflight
                .selected_branch
                .as_ref()
                .map(|selected| selected.name.as_str()),
            expected_head,
            sparse: sparse.as_ref(),
            auth_token: auth_token.as_deref(),
            snapshot_token: preflight.snapshot_token.as_deref(),
            known_loss_protocol: preflight.snapshot_token.is_some(),
            sparse_materialization_v1: preflight.sparse_materialization_v1,
        },
    )
    .await?;

    if resp.status().as_u16() == 404 {
        cleanup();
        return Err(OakError::RemoteRepoNotFound(format!("{owner}/{name}")));
    }

    // A branch moved between proof and pull. Re-prove once with the exact
    // bound credential and requested scope; never reuse or weaken the stale
    // snapshot. A second race is surfaced so clone work remains finite.
    if resp.status().as_u16() == 412 {
        if expected_head.is_some() {
            cleanup();
            return Err(OakError::Server(
                "repository snapshot or selected branch changed after exact-head preflight; the exact pinned clone was not acquired"
                    .to_string(),
            ));
        }
        preflight = super::integrity::preflight_clone_acquisition_with_policy_and_token(
            remote,
            &format!("{owner}/{name}"),
            shallow,
            preflight_branch,
            branch,
            None,
            sparse.as_ref().map(SparseCone::prefixes),
            integrity_policy,
            auth_token.as_deref(),
        )
        .await
        .inspect_err(|_| cleanup())?;
        resp = request_clone_pull(
            &client,
            ClonePullRequest {
                remote,
                owner,
                name,
                shallow,
                branch,
                selected_branch: preflight
                    .selected_branch
                    .as_ref()
                    .map(|selected| selected.name.as_str()),
                expected_head: None,
                sparse: sparse.as_ref(),
                auth_token: auth_token.as_deref(),
                snapshot_token: preflight.snapshot_token.as_deref(),
                known_loss_protocol: preflight.snapshot_token.is_some(),
                sparse_materialization_v1: preflight.sparse_materialization_v1,
            },
        )
        .await
        .inspect_err(|_| cleanup())?;
        if resp.status().as_u16() == 412 {
            cleanup();
            return Err(OakError::Server(
                "repository branches changed again during clone; retry to prove a stable snapshot"
                    .to_string(),
            ));
        }
    }

    if !resp.status().is_success() {
        cleanup();
        return Err(crate::http::server_error(resp).await);
    }

    if preflight.credential_accepted {
        if let Some(ref token) = auth_token {
            repo.set_metadata(MetadataKey::ApiKey, token)?;
        }
    } else if auth_token.is_some() {
        output::warning(
            "The presented credential was not accepted; clone proceeded anonymously and the rejected credential was not persisted",
        );
    }

    // Stream the response body with a progress bar. The server may or may not
    // send Content-Length — fall back to a byte-counting spinner when unknown.
    let total_bytes = resp.content_length();
    let download_pb = match total_bytes {
        Some(total) => {
            let pb = indicatif::ProgressBar::new(total);
            pb.set_style(
                indicatif::ProgressStyle::default_bar()
                    .template(
                        "  Downloading [{bar:30.cyan/dim}] {bytes}/{total_bytes} ({bytes_per_sec})",
                    )
                    .unwrap()
                    .progress_chars("━╸─"),
            );
            pb
        }
        None => {
            let pb = indicatif::ProgressBar::new_spinner();
            pb.set_style(
                indicatif::ProgressStyle::default_spinner()
                    .template("  Downloading {spinner} {bytes} ({bytes_per_sec})")
                    .unwrap(),
            );
            pb.enable_steady_tick(std::time::Duration::from_millis(80));
            pb
        }
    };

    let mut body_buf: Vec<u8> = match total_bytes {
        Some(t) => Vec::with_capacity(t as usize),
        None => Vec::new(),
    };
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                body_buf.extend_from_slice(&chunk);
                download_pb.inc(chunk.len() as u64);
            }
            Ok(None) => break,
            Err(e) => {
                download_pb.finish_and_clear();
                cleanup();
                return Err(OakError::Http(e.to_string()));
            }
        }
    }
    download_pb.finish_and_clear();

    let pull_response_body_bytes = body_buf.len() as u64;
    let pull_resp: PullResponse = serde_json::from_slice(&body_buf)
        .map_err(|e| OakError::Http(format!("Failed to parse server response: {e}")))
        .inspect_err(|_| cleanup())?;
    drop(body_buf);

    let pinned_selection =
        validate_pinned_pull_selection(&pull_resp, preflight.selected_branch.as_ref())
            .inspect_err(|_| cleanup())?;

    let known_lost = validate_pull_missing_content(
        &preflight.known_lost_blob_hashes,
        &pull_resp.missing_content,
    )
    .inspect_err(|_| cleanup())?;
    let response_head = pull_resp
        .head
        .as_deref()
        .map(oak_core::Hash::from_hex)
        .transpose()
        .map_err(|error| OakError::Server(format!("invalid pull response head: {error}")))?;
    let content_head = pinned_selection
        .as_ref()
        .map(|(_, head)| head)
        .or(response_head.as_ref());
    super::pull::validate_known_loss_absent_from_selected_head(
        &repo,
        &pull_resp,
        content_head,
        &preflight.known_lost_blob_hashes,
    )
    .inspect_err(|_| cleanup())?;
    super::known_loss::record_known_lost_blobs(&repo, &known_lost).inspect_err(|_| cleanup())?;
    if !known_lost.is_empty() {
        output::warning(&super::known_loss::warning(known_lost.len()));
    }

    // Path permissions: remember which blobs the server withheld as
    // restricted BEFORE materializing, so `write_working_directory` (and
    // later status/commit) can skip those files with an access message
    // instead of failing the clone as corrupt.
    super::restricted::record_restricted_blobs(&repo, &pull_resp.restricted_blobs)
        .inspect_err(|_| cleanup())?;

    // Fetch blob bytes (chunks → R2) and write `blobs` + `blob_chunks`
    // rows. Shared with `oak pull` via `pull::fetch_and_store_blobs`.
    // FK enforcement is off for the whole import (see `FkGuard` in
    // `store_pull_response`) — re-acquire that guard here for the blob
    // phase too, since chunks → blob_chunks references rely on rows in
    // an order the bulk import doesn't guarantee.
    repo.set_foreign_keys(false)?;
    let _fk_guard = super::FkGuard { repo: &repo };
    let initial_blob_stats = super::pull::fetch_and_store_blobs_with_stats(
        &repo,
        &pull_resp.blobs,
        super::pull::BlobFetchContext {
            client: &client,
            remote,
            owner,
            name,
            api_key: auth_token.as_deref(),
            emit_human_output: !json,
        },
    )
    .await
    .inspect_err(|_| cleanup())?;
    drop(_fk_guard);

    let mut objects_received = CloneObjectCounts {
        commits: pull_resp.commits.len() as u64,
        trees: pull_resp.trees.len() as u64,
        blob_descriptors: pull_resp.blobs.len() as u64,
    };
    let commit_count = store_pull_response(&repo, &pull_resp)
        .await
        .inspect_err(|_| cleanup())?;
    let mut legacy_pull_stats: Option<super::pull::PullAcquisitionStats> = None;
    let mut legacy_additional_fetch_unmeasured = false;

    // A negotiated selected-branch acquisition publishes that server branch
    // directly. Unselected and legacy clones retain the longstanding local
    // branch behavior.
    let chosen_head = if let Some((branch, head)) = pinned_selection {
        if repo.get_commit(&head).inspect_err(|_| cleanup())?.is_none() {
            cleanup();
            return Err(OakError::Server(format!(
                "selected branch '{branch}' exact head {head} was not imported"
            )));
        }
        repo.set_branch_head(&branch, &head)
            .inspect_err(|_| cleanup())?;
        repo.set_current_branch(&branch)
            .inspect_err(|_| cleanup())?;
        Some(head)
    } else if let Some(branch) = branch {
        let branch_data = repo.get_branch(branch).inspect_err(|_| cleanup())?;
        if branch_data
            .as_ref()
            .is_none_or(|branch| branch.status != BranchStatus::Open)
        {
            return Err(OakError::Server(format!(
                "legacy pull omitted open selected branch '{branch}'; clone was not created"
            )));
        }
        let head = match repo.get_branch_head(branch).inspect_err(|_| cleanup())? {
            Some(head) => Some(head),
            None => {
                // Pre-acquisition servers expose branch-specific content
                // through the ordinary pull endpoint. Preserve the old
                // clone-then-switch behavior inside the guarded destination:
                // pull the selected branch before consulting its head
                // endpoint (a branch with its own commit is resolved by this
                // pull and never needs the latter request).
                let lock = crate::workdir_lock::WorkdirLock::acquire(&oak_dir)
                    .inspect_err(|_| cleanup())?;
                legacy_pull_stats = Some(
                    super::pull::pull_async_with_stats(
                        &lock,
                        &repo,
                        remote,
                        &format!("{owner}/{name}/pull"),
                        Some(branch),
                        None,
                        false,
                        &repo_dir,
                        auth_token.as_deref(),
                        !json,
                        super::pull::PullWorktreeMode::DeferToClone,
                    )
                    .await
                    .inspect_err(|_| cleanup())?,
                );
                drop(lock);

                let refreshed = repo.get_branch(branch).inspect_err(|_| cleanup())?;
                if refreshed
                    .as_ref()
                    .is_none_or(|branch| branch.status != BranchStatus::Open)
                {
                    return Err(OakError::Server(format!(
                        "legacy selected branch '{branch}' is no longer open; clone was not created"
                    )));
                }

                if let Some(head) = repo.get_branch_head(branch).inspect_err(|_| cleanup())? {
                    Some(head)
                } else {
                    let endpoint = format!("{owner}/{name}");
                    let head = super::push::fetch_remote_branch_head(
                    &client,
                    remote,
                    &endpoint,
                    branch,
                    auth_token.as_deref(),
                )
                .await
                .inspect_err(|_| cleanup())?
                .ok_or_else(|| {
                    OakError::Server(format!(
                        "legacy selected branch '{branch}' has no remote head; clone was not created"
                    ))
                })?;
                    if repo.get_commit(&head).inspect_err(|_| cleanup())?.is_none() {
                        // A branch with no commits of its own points at its main
                        // seed. The branch pull can name that head without
                        // carrying the main commit, matching the longstanding
                        // switch fallback.
                        legacy_additional_fetch_unmeasured = true;
                        super::sync::fetch_parent_from_server_with_remote_quiet(
                            &repo,
                            oak_core::DEFAULT_BRANCH,
                            Some(remote),
                            json,
                        )
                        .await
                        .inspect_err(|_| cleanup())?;
                    }
                    if repo.get_commit(&head).inspect_err(|_| cleanup())?.is_none() {
                        return Err(OakError::Server(format!(
                        "legacy selected branch '{branch}' moved to unavailable head {head}; clone was not created"
                    )));
                    }
                    repo.set_branch_head(branch, &head)
                        .inspect_err(|_| cleanup())?;
                    Some(head)
                }
            }
        };
        repo.set_current_branch(branch).inspect_err(|_| cleanup())?;
        head
    } else {
        let chosen_name = super::init::default_local_branch_name();
        let branch = prepare_personal_branch(&repo, &chosen_name).inspect_err(|_| {
            cleanup();
        })?;
        repo.get_branch_head(&branch).inspect_err(|_| cleanup())?
    };

    // Materialize the working tree from the chosen branch's head. This
    // happens AFTER `prepare_personal_branch` so the on-disk files reflect
    // the branch the user actually landed on — not whichever branch
    // happened to be picked by `store_pull_response`'s scan.
    let mut materialization = if let Some(ref head) = chosen_head {
        repo.set_head(head).inspect_err(|_| cleanup())?;
        let mut stats = write_working_directory(&repo, head, &repo_dir).inspect_err(|_| {
            cleanup();
        })?;
        stats.passes_observed += 1;
        stats
    } else {
        CloneMaterializationCounts::default()
    };
    fs::remove_file(&clone_marker).inspect_err(|_| cleanup())?;
    destination_guard.disarm();

    let active = repo
        .get_current_branch_name()?
        .ok_or_else(|| OakError::Server("clone completed without a current branch".to_string()))?;
    if json {
        let mut pull_response_body_bytes_observed = pull_response_body_bytes;
        let mut pull_phase_counters_complete = !legacy_additional_fetch_unmeasured;
        let mut required = initial_blob_stats.required.clone();
        let mut downloaded = initial_blob_stats.downloaded.clone();
        let mut downloaded_logical_bytes = initial_blob_stats.downloaded_logical_bytes;
        let present_at_clone_start_unique = initial_blob_stats.valid_before_fetch.len() as u64;
        let mut reused_from_prior_phase = std::collections::HashSet::new();
        let mut pull_phases = vec![ClonePullPhase {
            name: "initial",
            response_body_bytes: Some(pull_response_body_bytes),
            counters_complete: true,
            object_entries_received: CloneObjectCounts {
                commits: objects_received.commits,
                trees: objects_received.trees,
                blob_descriptors: objects_received.blob_descriptors,
            },
            chunks: ClonePhaseChunkCounts {
                required_unique: initial_blob_stats.required.len() as u64,
                downloaded_unique: initial_blob_stats.downloaded.len() as u64,
                downloaded_logical_bytes: initial_blob_stats.downloaded_logical_bytes,
                valid_before_phase_fetch_unique: initial_blob_stats.valid_before_fetch.len() as u64,
            },
            materialization_passes: 0,
        }];
        if let Some(ref legacy) = legacy_pull_stats {
            if let Some(bytes) = legacy.response_body_bytes {
                pull_response_body_bytes_observed =
                    pull_response_body_bytes_observed.saturating_add(bytes);
            } else {
                pull_phase_counters_complete = false;
            }
            pull_phase_counters_complete &= legacy.counters_complete;
            objects_received.commits = objects_received
                .commits
                .saturating_add(legacy.commits_received);
            objects_received.trees = objects_received.trees.saturating_add(legacy.trees_received);
            objects_received.blob_descriptors = objects_received
                .blob_descriptors
                .saturating_add(legacy.blob_descriptors_received);
            reused_from_prior_phase.extend(
                legacy
                    .blob_fetch
                    .valid_before_fetch
                    .intersection(&downloaded)
                    .cloned(),
            );
            required.extend(legacy.blob_fetch.required.iter().cloned());
            downloaded.extend(legacy.blob_fetch.downloaded.iter().cloned());
            downloaded_logical_bytes =
                downloaded_logical_bytes.saturating_add(legacy.blob_fetch.downloaded_logical_bytes);
            materialization.passes_observed = materialization
                .passes_observed
                .saturating_add(legacy.materialization_passes);
            pull_phases.push(ClonePullPhase {
                name: "legacy_selected_branch",
                response_body_bytes: legacy.response_body_bytes,
                counters_complete: legacy.counters_complete,
                object_entries_received: CloneObjectCounts {
                    commits: legacy.commits_received,
                    trees: legacy.trees_received,
                    blob_descriptors: legacy.blob_descriptors_received,
                },
                chunks: ClonePhaseChunkCounts {
                    required_unique: legacy.blob_fetch.required.len() as u64,
                    downloaded_unique: legacy.blob_fetch.downloaded.len() as u64,
                    downloaded_logical_bytes: legacy.blob_fetch.downloaded_logical_bytes,
                    valid_before_phase_fetch_unique: legacy.blob_fetch.valid_before_fetch.len()
                        as u64,
                },
                materialization_passes: legacy.materialization_passes,
            });
        }
        let mut unknown = vec![
            CloneReceiptUnknown {
                field: "http_header_and_tls_bytes",
                reason: "transport stack does not expose it",
            },
            CloneReceiptUnknown {
                field: "final_allocated_disk_bytes",
                reason: "would require an additional filesystem traversal",
            },
            CloneReceiptUnknown {
                field: "chunk_transfer_response_body_bytes",
                reason: "existing download helpers expose verified logical content, not every encoded response body",
            },
        ];
        if legacy_additional_fetch_unmeasured {
            unknown.push(CloneReceiptUnknown {
                field: "legacy_parent_hydration",
                reason: "legacy headless-branch fallback may fetch parent objects outside the instrumented pull seam",
            });
        }
        let head = repo.get_head()?;
        let manifest = head
            .as_ref()
            .map(|head| {
                repo.get_commit(head)?
                    .map(|commit| commit.manifest_hash)
                    .ok_or_else(|| OakError::Server(format!("clone HEAD {head} is unavailable")))
            })
            .transpose()?;
        let initial_pull_snapshot_bound = preflight.snapshot_token.is_some();
        let selected_branch_snapshot_bound = branch.map(|_| preflight.selected_branch.is_some());
        let snapshot_bound = initial_pull_snapshot_bound
            && selected_branch_snapshot_bound.is_none_or(std::convert::identity);
        let (acquisition_profile, proof_scope) = if snapshot_bound {
            (
                preflight.disposition.as_str(),
                preflight.disposition.proof_scope(),
            )
        } else if initial_pull_snapshot_bound {
            (
                "bounded_v1_selected_branch_unbound",
                "initial_pull_only_selected_branch_unbound",
            )
        } else {
            ("legacy_unverified", "downloaded_objects_only")
        };
        let preflight_credential_disposition = match (
            auth_token.is_some(),
            preflight.snapshot_token.is_some(),
            preflight.credential_accepted,
        ) {
            (false, _, _) => "not_present",
            (true, false, _) => "not_proven",
            (true, true, true) => "server_reported_accepted",
            (true, true, false) => "server_reported_not_accepted",
        };
        output::print_json(&CloneReceipt {
            schema_version: 1,
            operation: "clone",
            repository: CloneReceiptRepository {
                origin: super::push::normalize_remote_url(remote),
                owner,
                name,
            },
            request: CloneReceiptRequest {
                history: CloneHistoryRequest {
                    mode: if shallow { "shallow" } else { "full" },
                    depth: shallow.then_some(1),
                },
                selected_branch: branch,
                expected_head,
                materialization: CloneMaterializationRequest {
                    mode: if sparse.is_some() { "sparse" } else { "full" },
                    prefix_count: sparse.as_ref().map_or(0, |cone| cone.prefixes().len()),
                },
            },
            result: CloneReceiptResult {
                local_branch: active,
                head,
                manifest,
                acquisition_profile,
                preflight_profile: preflight.disposition.as_str(),
                proof_scope,
                snapshot_bound,
                initial_pull_snapshot_bound,
                selected_branch_snapshot_bound,
                preflight_credential_disposition,
            },
            observed: CloneReceiptObserved {
                elapsed_ms: started.elapsed().as_secs_f64() * 1_000.0,
                pull_response_body_bytes_observed,
                pull_phase_counters_complete,
                pull_phases,
                object_entries_received: objects_received,
                chunks: CloneChunkCounts {
                    required_unique: required.len() as u64,
                    downloaded_unique: downloaded.len() as u64,
                    downloaded_logical_bytes,
                    present_at_clone_start_unique,
                    reused_from_prior_phase_unique: reused_from_prior_phase.len() as u64,
                },
                materialization,
            },
            unknown,
        })?;
    } else {
        output::success(&format!(
            "Cloned '{}/{}' with {} commit(s) into {}",
            owner,
            name,
            commit_count,
            repo_dir.display(),
        ));
        output::item(&format!(
            "Working on branch {}{}{}",
            output::colors::CYAN,
            active,
            output::colors::RESET,
        ));
        if shallow {
            output::item("Shallow clone (recent history only) — re-run without `--shallow` for the complete history");
        }
    }

    Ok(())
}

/// Create the personal branch for this clone and switch to it.
///
/// `proposed_name` is expected to already be unique (from
/// `init::default_local_branch_name()`, which suffixes a random 6-hex tag).
/// If the name does collide with an existing local row (extremely rare —
/// would require a uuid prefix collision plus the prior branch having been
/// stored under the same DB), we regenerate with a fresh suffix until we
/// find a free name. This is intentionally simpler than the old
/// `-1`/`-2`/`-3` walk that tried to recycle author-base names: branches
/// are cheap, branch-per-clone is the model.
///
/// `main` exists only on the server, so the new branch's head is pinned
/// explicitly to whatever the server reports as main's head — without it,
/// `resolve_effective_head` (used by commit / log / diff) has no parent row
/// to walk to find the starting commit. If main has no local head record
/// (empty repo, or the server hadn't yet bootstrapped main), the new branch
/// is left head-less, matching `oak init` on a fresh local repo.
pub fn prepare_personal_branch(repo: &SqliteRepository, proposed_name: &str) -> Result<String> {
    // Defensive: regenerate on the off chance the proposed name is taken.
    let mut name = proposed_name.to_string();
    while repo.get_branch(&name)?.is_some() {
        name = regenerate_personal_branch_name(proposed_name);
    }
    let new_branch = oak_core::Branch::new(name.clone(), None, Some("main".to_string()));
    repo.store_branch(&new_branch)?;

    // Pin the new branch's head to main's. Fall back to any open branch
    // that has a head — e.g. a server where main hasn't been bootstrapped
    // yet but a `default` branch carries the initial commits. Without a
    // pinned head, `resolve_effective_head` can't walk parent_branch to
    // find a starting commit.
    let head_to_pin = match repo.get_branch_head("main")? {
        Some(h) => Some(h),
        None => repo
            .list_branches()?
            .into_iter()
            .filter(|b| b.status == BranchStatus::Open && b.name != name)
            .find_map(|b| repo.get_branch_head(&b.name).ok().flatten()),
    };
    if let Some(head) = head_to_pin {
        repo.set_branch_head(&name, &head)?;
    }
    repo.set_current_branch(&name)?;
    Ok(name)
}

fn regenerate_personal_branch_name(proposed_name: &str) -> String {
    let base = proposed_name
        .rsplit_once('-')
        .and_then(|(base, suffix)| {
            let looks_like_generated_suffix =
                suffix.len() == 6 && suffix.chars().all(|c| c.is_ascii_hexdigit());
            if !base.is_empty() && looks_like_generated_suffix {
                Some(base)
            } else {
                None
            }
        })
        .unwrap_or(proposed_name);
    format!("{base}-{}", super::init::random_branch_suffix())
}

/// Commit the bulk-import transaction every this many tree/commit rows, to
/// bound WAL growth on a repo with a large history or many files. These rows
/// are small (no blob content), so the threshold is in rows, not bytes.
const BULK_FLUSH_ROWS: usize = 10_000;

/// Store the data side of a clone's pull response into local SQLite:
/// blobs, trees, branches, commits, and per-branch heads. Returns the
/// number of commits stored.
///
/// This intentionally does NOT pick a current branch, set the global head,
/// or materialize the working tree — `clone_repo` does that explicitly
/// after `prepare_personal_branch` decides which branch the user is
/// actually landing on. Doing it here would race that decision and could
/// leave the on-disk files written from a different branch than the one
/// the user lands on (a stale-WD bug under random HashSet iteration order
/// in `pull_resp.branches`).
async fn store_pull_response(repo: &SqliteRepository, pull_resp: &PullResponse) -> Result<usize> {
    repo.set_foreign_keys(false)?;
    let _fk_guard = super::FkGuard { repo };

    // Batch the per-object metadata writes (trees, branches, commits, heads)
    // into one relaxed-durability transaction. As with the blob phase, each
    // store_* would otherwise be its own fsync'd commit — and a many-file
    // repo can have tens of thousands of tree and commit rows. `bulk_commit`
    // is called explicitly below; the guard rolls back if any step errors.
    // The FK toggle above happens before `bulk_begin` (PRAGMA foreign_keys is
    // a no-op inside a transaction) and the `_fk_guard` drops after the
    // commit, so both pragmas always fire outside the transaction.
    let bulk = super::BulkTxn::begin(repo)?;
    let mut rows_since_flush: usize = 0;

    // Blobs are fetched + stored separately via `pull::fetch_and_store_blobs`
    // before this runs — it owns the chunked-download dance and the
    // `blobs` + `blob_chunks` writes. This function handles trees,
    // branches, commits, etc.

    // Store tree objects
    for tree_data in &pull_resp.trees {
        let tree = super::pull::wire_to_core_tree(tree_data)?;
        repo.store_tree(&tree)?;
        rows_since_flush += 1;
        if rows_since_flush >= BULK_FLUSH_ROWS {
            bulk.flush()?;
            rows_since_flush = 0;
        }
    }

    // Store all branches referenced by commits. Sort parent-before-child so
    // the self-referential FK on branches(parent_branch) is satisfied.
    let sorted_branches = super::sort_branches_topologically(
        pull_resp.branches.iter().collect::<Vec<_>>(),
        |b: &&BranchPullData| b.name.as_str(),
        |b: &&BranchPullData| b.parent_branch.as_deref(),
    );
    for br_data in sorted_branches {
        let status = BranchStatus::from_db_str(&br_data.status);
        // A branch that's already closed on the server is finished history
        // (merged or abandoned) — a fresh clone shouldn't materialize
        // tombstones for it. Their commits still land below; branch_name on
        // commits is a soft label.
        if status == BranchStatus::Closed {
            continue;
        }
        let created_at = chrono::DateTime::parse_from_rfc3339(&br_data.created_at)
            .map_err(|e| OakError::Database(e.to_string()))?
            .with_timezone(&chrono::Utc);

        let br = Branch {
            name: br_data.name.clone(),
            description: br_data.description.clone(),
            parent_branch: br_data.parent_branch.clone(),
            status,
            close_reason: br_data
                .close_reason
                .as_deref()
                .map(oak_core::CloseReason::parse)
                .transpose()?,
            created_at,
        };
        repo.store_branch(&br)?;
    }
    // The singular `branch` field is set by pull requests that asked for
    // a specific branch; clone never sets it. Still store the row if
    // present so the local repo's branch set is complete.
    if let Some(br_data) = &pull_resp.branch {
        let status = BranchStatus::from_db_str(&br_data.status);
        let created_at = chrono::DateTime::parse_from_rfc3339(&br_data.created_at)
            .map_err(|e| OakError::Database(e.to_string()))?
            .with_timezone(&chrono::Utc);

        let br = Branch {
            name: br_data.name.clone(),
            description: br_data.description.clone(),
            parent_branch: br_data.parent_branch.clone(),
            status,
            close_reason: br_data
                .close_reason
                .as_deref()
                .map(oak_core::CloseReason::parse)
                .transpose()?,
            created_at,
        };
        repo.store_branch(&br)?;
    }

    // Store commits — track the last commit hash per branch (commits are ordered ASC by timestamp)
    let mut branch_heads: std::collections::HashMap<String, Hash> =
        std::collections::HashMap::new();
    for commit_data in &pull_resp.commits {
        let commit = super::pull::commit_from_pull_data(commit_data)?;

        // Track latest commit per branch (last one written wins, list is ASC order)
        branch_heads.insert(commit.branch_name.clone(), commit.hash.clone());
        repo.store_commit(&commit)?;
        rows_since_flush += 1;
        if rows_since_flush >= BULK_FLUSH_ROWS {
            bulk.flush()?;
            rows_since_flush = 0;
        }
    }

    let commit_count = pull_resp.commits.len();

    // Set branch heads for every branch we received commits for
    for (branch_name, head_hash) in &branch_heads {
        repo.set_branch_head(branch_name, head_hash)?;
    }
    if let Some((branch_name, head_hash)) = super::pull::response_head_for_branch(pull_resp, None)?
    {
        if repo.get_commit(&head_hash)?.is_none() {
            return Err(OakError::Server(format!(
                "response head for branch '{branch_name}' points at missing commit {head_hash}"
            )));
        }
        repo.set_branch_head(&branch_name, &head_hash)?;
    }

    bulk.commit()?;
    Ok(commit_count)
}

/// Materialize the commit at `head`'s manifest into the working tree at
/// `repo_dir`. Used by `clone_repo` after the personal branch is chosen.
fn write_working_directory(
    repo: &SqliteRepository,
    head: &Hash,
    repo_dir: &Path,
) -> Result<CloneMaterializationCounts> {
    let commit = repo.get_commit(head)?.unwrap();
    let manifest = repo.get_manifest(&commit.manifest_hash)?.unwrap();

    // A sparse clone scopes the working tree to the active cone: out-of-cone
    // blobs were never shipped by the server, so we neither write them nor
    // treat their absence as a broken clone.
    let cone = oak_core::SparseCone::from_metadata(
        repo.get_metadata(MetadataKey::SparsePaths)?.as_deref(),
    );
    let entries_to_write: Vec<&ManifestEntry> = manifest
        .entries
        .iter()
        .filter(|e| cone.as_ref().is_none_or(|c| c.covers(&e.path)))
        .collect();
    let mut stats = CloneMaterializationCounts {
        observed_eligible_paths: entries_to_write.len() as u64,
        observed_out_of_cone_paths: (manifest.entries.len() - entries_to_write.len()) as u64,
        ..CloneMaterializationCounts::default()
    };

    // Reconstruct the empty blob before the missing-blob check below. Its
    // bytes follow from its hash, so a pull response that omits it (servers
    // whose blob migration filtered on `octet_length(content) > 0` have a
    // metadata-only row and no chunks) is recoverable client-side instead of
    // being a clone-blocking corruption report.
    oak_core::ensure_empty_blobs_in_manifest(repo, &manifest)?;

    let write_pb = indicatif::ProgressBar::new(entries_to_write.len() as u64);
    write_pb.set_style(
        indicatif::ProgressStyle::default_bar()
            .template("  Writing files [{bar:30.cyan/dim}] {pos}/{len}")
            .unwrap()
            .progress_chars("━╸─"),
    );
    // Escape hatch for recovering from a server in a broken state: set
    // OAK_ALLOW_PARTIAL_CLONE=1 to skip missing blobs instead of failing.
    // Each skipped file is reported so the user knows the working tree
    // is incomplete.
    let allow_partial = std::env::var("OAK_ALLOW_PARTIAL_CLONE")
        .ok()
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
    let mut skipped: Vec<String> = Vec::new();

    // Files the server withheld under path-based permissions (recorded from
    // the pull response's `restricted_blobs`) can never materialize — skip
    // them with an access message instead of failing the clone as corrupt.
    let restricted: std::collections::HashSet<String> =
        super::restricted::restricted_paths_in_manifest(repo, &manifest)
            .into_iter()
            .collect();
    let mut restricted_skipped: Vec<String> = Vec::new();
    let known_lost: std::collections::HashSet<String> =
        super::known_loss::known_lost_paths_in_manifest(repo, &manifest)
            .into_iter()
            .collect();
    let mut known_lost_skipped: Vec<String> = Vec::new();
    let mut cache_upserts = Vec::new();

    // Validate availability before touching the working tree. This keeps a
    // malformed or incomplete pull from partially overwriting an existing
    // destination before a later entry exposes the missing content.
    for entry in &entries_to_write {
        if repo.get_blob(&entry.blob_hash)?.is_none()
            && !restricted.contains(&entry.path)
            && !known_lost.contains(&entry.path)
            && !allow_partial
        {
            return Err(OakError::Server(format!(
                "clone is missing blob {} for '{}'. The server's pull response didn't include this blob — its `blob_chunks` rows or R2 backing may be incomplete. Refusing to write a partial working tree. Set OAK_ALLOW_PARTIAL_CLONE=1 to skip missing files instead.",
                entry.blob_hash, entry.path,
            )));
        }
    }

    for entry in &entries_to_write {
        let write_path = repo_dir.join(&entry.path);

        if let Some(parent) = write_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Fail loudly when a manifest entry's blob isn't locally available:
        // a silent skip leaves the working tree with missing files and
        // makes `oak status` show the gap as "modified," which is the
        // exact failure mode that masked broken-blob and partial-pull
        // bugs on real clones. The pull endpoint either ships every blob
        // it needs or it shouldn't have advertised the commit at all, so
        // a missing blob here is a real corruption / wire-format
        // mismatch that the user needs to see. (Permission-withheld blobs
        // are the deliberate exception, split out above.)
        let blob = match repo.get_blob(&entry.blob_hash)? {
            Some(b) => b,
            None if restricted.contains(&entry.path) => {
                restricted_skipped.push(entry.path.clone());
                stats.restricted_paths += 1;
                write_pb.inc(1);
                continue;
            }
            None if known_lost.contains(&entry.path) => {
                known_lost_skipped.push(entry.path.clone());
                stats.known_lost_paths += 1;
                write_pb.inc(1);
                continue;
            }
            None if allow_partial => {
                skipped.push(entry.path.clone());
                stats.missing_required_paths += 1;
                write_pb.inc(1);
                continue;
            }
            None => {
                return Err(OakError::Server(format!(
                    "clone is missing blob {} for '{}'. The server's pull \
                     response didn't include this blob — its `blob_chunks` rows \
                     or R2 backing may be incomplete. Refusing to write a \
                     partial working tree. Set OAK_ALLOW_PARTIAL_CLONE=1 to \
                     skip missing files instead.",
                    entry.blob_hash, entry.path,
                )));
            }
        };
        crate::materialize::materialize_path(&write_path, entry.mode, &blob.content)?;
        if entry.mode != FileMode::Symlink {
            if let Some(upsert) = crate::commands::commit::stat_cache_upsert(
                &entry.path,
                &write_path,
                &entry.blob_hash,
            ) {
                cache_upserts.push(upsert);
            }
        }
        stats.written_paths += 1;
        stats.logical_bytes = stats
            .logical_bytes
            .saturating_add(blob.content.len() as u64);
        write_pb.inc(1);
    }
    write_pb.finish_and_clear();

    // This is the clone's final materialization, so cache the metadata from
    // the files as they exist after their last write/chmod and discard any
    // rows left by an earlier legacy pull pass. Without this refresh the
    // optimized path had no rows, while the old two-pass path kept rows made
    // stale by the final rewrite; either state forces the first scan to hash
    // every file again.
    crate::commands::commit::refresh_stat_cache_after_materialize(repo, cache_upserts)?;

    if !restricted_skipped.is_empty() {
        output::warning(&super::restricted::withheld_summary(
            restricted_skipped.len(),
        ));
        for p in &restricted_skipped {
            output::warning(&format!("  - {p}"));
        }
    }
    if !known_lost_skipped.is_empty() {
        output::warning(&super::known_loss::warning(known_lost_skipped.len()));
        for path in &known_lost_skipped {
            output::warning(&format!("  - {path}"));
        }
    }
    if !skipped.is_empty() {
        output::warning(&format!(
            "OAK_ALLOW_PARTIAL_CLONE: skipped {} missing file(s):",
            skipped.len()
        ));
        for p in &skipped {
            output::warning(&format!("  - {p}"));
        }
    }

    Ok(stats)
}

#[derive(Deserialize)]
struct DeleteResponse {
    #[allow(dead_code)]
    deleted: bool,
    message: String,
}

/// Delete a repository from the server. `spec` is `owner/name`.
pub async fn delete(remote: &str, spec: &str, force: bool) -> Result<()> {
    let (owner, name) = super::parse_owner_repo(spec)?;
    let full = format!("{owner}/{name}");
    // Strong confirmation unless --force is used
    if !force {
        output::warning(&format!(
            "You are about to permanently delete the repository '{full}'"
        ));
        output::warning("This action CANNOT be undone!");
        output::warning("All commits, history, and data will be permanently destroyed.");
        output::print_line("");

        // First confirmation
        let confirm1 = Confirm::new()
            .with_prompt(format!("Are you sure you want to delete '{full}'?"))
            .default(false)
            .interact()
            .map_err(|e| OakError::Server(e.to_string()))?;

        if !confirm1 {
            output::info("Deletion cancelled");
            return Ok(());
        }

        // Second confirmation - type the repo name
        output::print_line("");
        output::warning("To confirm, please type the repository name exactly:");

        let input: String = dialoguer::Input::new()
            .with_prompt("Repository name (without owner)")
            .interact_text()
            .map_err(|e| OakError::Server(e.to_string()))?;

        if input != name {
            output::error("Repository name does not match. Deletion cancelled.");
            return Err(OakError::Server("Repository name mismatch".to_string()));
        }
    }

    let client = crate::http::api_client();
    let token = super::credentials::effective_token(remote, None);

    let mut req = client.delete(format!("{remote}/api/{owner}/{name}"));
    if let Some(ref t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if resp.status().as_u16() == 404 {
        return Err(OakError::RemoteRepoNotFound(full));
    }

    if !resp.status().is_success() {
        return Err(crate::http::server_error(resp).await);
    }

    let delete_resp: DeleteResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    output::success(&delete_resp.message);
    Ok(())
}

#[derive(Deserialize)]
struct TransferResponse {
    name: String,
    owner: Option<String>,
}

/// Transfer a repository to a different owner (organization or user).
///
/// The target is either an organization slug or a username — organization slugs and
/// usernames share a global namespace on the server, so a single argument is
/// unambiguous. When both an organization and a user match (which shouldn't happen
/// per DB triggers), the organization wins.
pub async fn transfer(remote: &str, spec: &str, target: &str) -> Result<()> {
    let (owner, name) = super::parse_owner_repo(spec)?;
    let token = super::credentials::effective_token(remote, None);

    let client = crate::http::api_client();

    // Disambiguate target: try organization first (matches `oak/oak` intent), fall
    // back to user. We do this client-side by probing both endpoints — simpler
    // for the user than requiring `--to-organization` / `--to-user` flags.
    let body = {
        let mut probe = client.get(format!("{remote}/api/orgs/{target}"));
        if let Some(ref t) = token {
            probe = probe.header("authorization", format!("Bearer {t}"));
        }
        let ws_resp = probe
            .send()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        if ws_resp.status().is_success() {
            serde_json::json!({ "to_organization": target })
        } else {
            serde_json::json!({ "to_user": target })
        }
    };

    let mut req = client
        .post(format!("{remote}/api/{owner}/{name}/transfer"))
        .json(&body);
    if let Some(ref t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(crate::http::server_error(resp).await);
    }

    let transfer: TransferResponse = resp
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;

    let new_path = match transfer.owner {
        Some(o) => format!("{}/{}", o, transfer.name),
        None => transfer.name,
    };
    output::success(&format!("Transferred repository to '{new_path}'"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{resolve_clone_spec, validate_pull_missing_content};
    use oak_core::protocol::MissingContentData;

    #[test]
    fn resolve_clone_spec_accepts_owner_repo() {
        assert_eq!(
            resolve_clone_spec("https://oak.space", "oak/benchmarks", None).unwrap(),
            ("oak".to_string(), "benchmarks".to_string())
        );
    }

    #[test]
    fn resolve_clone_spec_uses_username_for_bare_repo() {
        assert_eq!(
            resolve_clone_spec("https://oak.space", "benchmarks", Some("oak")).unwrap(),
            ("oak".to_string(), "benchmarks".to_string())
        );
    }

    #[test]
    fn resolve_clone_spec_rejects_unsafe_owner_repo_segments() {
        for spec in [
            "../repo",
            "./repo",
            "owner/..",
            "owner/.",
            "owner\\path/repo",
            "owner/repo\\path",
            "owner name/repo",
            "owner/repo name",
            "owner/repo/subtree",
        ] {
            assert!(
                resolve_clone_spec("https://oak.space", spec, None).is_err(),
                "expected {spec:?} to be rejected"
            );
        }
    }

    #[test]
    fn resolve_clone_spec_rejects_unsafe_bare_repo_segments() {
        for spec in ["..", ".", "repo\\path", "repo name"] {
            assert!(
                resolve_clone_spec("https://oak.space", spec, Some("owner")).is_err(),
                "expected {spec:?} to be rejected"
            );
        }
    }

    #[test]
    fn pull_known_loss_must_exactly_match_preflight_disposition() {
        let hash = "ab".repeat(32);
        let expected = std::collections::HashSet::from([hash.clone()]);
        let declared = vec![MissingContentData {
            kind: "blob".to_string(),
            hash: hash.clone(),
            reason_code: "operator_adjudicated_loss".to_string(),
        }];
        assert_eq!(
            validate_pull_missing_content(&expected, &declared).unwrap(),
            vec![hash]
        );

        let unknown = vec![MissingContentData {
            kind: "blob".to_string(),
            hash: "cd".repeat(32),
            reason_code: "operator_adjudicated_loss".to_string(),
        }];
        assert!(validate_pull_missing_content(&expected, &unknown).is_err());
        assert!(validate_pull_missing_content(&expected, &[]).is_err());
    }
}
