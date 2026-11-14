//! Checkout-free remote review through the server's review API
//! (oakspace `docs/checkout-free-review-api.md`):
//! `GET /api/:owner/:name/branches/:b/{diff, merge-preview}`.
//!
//! `oak branch diff --remote` and `oak branch review --remote` ask the server
//! for the file list, hunks and merge verdict instead of fetching commits and
//! hydrating blobs locally, so they work from any directory — `--repo
//! OWNER/NAME` names the repository when there is no checkout at all. Every
//! follow-up call of one invocation is pinned to the heads the first
//! response reported.
//!
//! Older servers answer the new routes with a bare 404 (the review API's own
//! 404s carry an `error_code`); that, and only that, falls back to the local
//! computation path, which needs a checkout. A 429/503 is transient and never
//! falls back.

use std::path::{Path, PathBuf};

use oak_core::{OakError, Result};
use serde::Deserialize;
use serde_json::Value;

use super::{
    branch_diff_recommendations, classify_change, evidence_kind_for_source,
    identical_files_unavailable, per_file_commands, qualify_snapshot_rows,
    review_merge_preview_json, review_page_command, review_recommendations,
    validate_changed_files_window, window_changed_files, ConflictFileJson, DiffJson,
    DiffJsonOptions, DiffMode, FileSummaryJson, LineageJson, MergePreviewJson, ReviewJson,
    EVIDENCE_SNAPSHOT, OMITTED_BYTE_BUDGET, OMITTED_REMOTE_CONTENT_BUDGET,
    OMITTED_REMOTE_CONTENT_UNAVAILABLE, SCHEMA_VERSION,
};
use crate::commands::branch::RemoteIdentity;
use crate::commands::merge_safety::{MergeSafetyJson, ServerMergeSafetyJson};
use crate::commands::triage::{
    apply_checks, derive_branch_triage_from_evidence, BranchTriageInput, CiEvidence,
    ContributionState, TargetRisk,
};
use crate::output;

/// Largest page the server serves.
const PAGE_LIMIT: usize = 1000;
/// Server cap on one response's patch bytes.
const SERVER_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Keep each content request's `paths=` list comfortably inside URL limits.
const PATHS_QUERY_BUDGET: usize = 6 * 1024;
/// Longest `Retry-After` honored with one in-process retry of a 503.
const MAX_RETRY_AFTER_SECS: u64 = 5;

/// Where the review runs: the server identity, plus whether a checkout was
/// found (only a checkout can fall back to local computation).
pub(crate) struct ReviewTarget {
    identity: RemoteIdentity,
    repo_arg: Option<String>,
    in_checkout: bool,
}

impl ReviewTarget {
    /// `--repo OWNER/NAME` wins; otherwise the checkout (or mount) at `path`.
    pub(crate) fn resolve(path: &Path, repo_arg: Option<&str>) -> Result<Self> {
        let workspace = crate::commands::branch::RemoteWorkspace::open(path);
        match repo_arg {
            Some(spec) => {
                // `--repo https://host/OWNER/NAME` names the server too.
                let (explicit_remote, spec_owner_repo) = split_repo_url(spec);
                let (owner, repo_name) = crate::commands::parse_owner_repo(&spec_owner_repo)?;
                let remote_url = explicit_remote
                    .or_else(crate::commands::push::env_remote_override)
                    .or_else(|| workspace.as_ref().ok().map(|w| w.remote.remote_url.clone()))
                    .unwrap_or_else(|| crate::commands::push::DEFAULT_REMOTE.to_string());
                let in_checkout = workspace
                    .as_ref()
                    .is_ok_and(|w| w.remote.owner == owner && w.remote.repo_name == repo_name);
                let token = match workspace.as_ref() {
                    Ok(w) if in_checkout => w.remote.token.clone(),
                    _ => crate::commands::credentials::effective_token(&remote_url),
                };
                Ok(Self {
                    identity: RemoteIdentity {
                        remote_url,
                        owner,
                        repo_name,
                        token,
                    },
                    repo_arg: Some(spec.to_string()),
                    in_checkout,
                })
            }
            None => {
                let workspace = workspace.map_err(|_| {
                    OakError::InvalidArgument(
                        "Not inside an Oak repository; pass --repo OWNER/NAME to review a remote branch without a checkout"
                            .to_string(),
                    )
                })?;
                Ok(Self {
                    identity: workspace.remote,
                    repo_arg: None,
                    in_checkout: true,
                })
            }
        }
    }

    fn api(&self, rest: &str) -> String {
        format!(
            "{}/api/{}/{}/{rest}",
            self.identity.remote_url,
            urlencoding::encode(&self.identity.owner),
            urlencoding::encode(&self.identity.repo_name),
        )
    }

    /// Suggested `oak branch … --remote` commands need `--repo` when the
    /// caller had to give one.
    fn qualify_commands(&self, commands: &mut [String]) {
        let Some(spec) = self.repo_arg.as_deref() else {
            return;
        };
        for command in commands {
            if command.starts_with("oak branch ")
                && command.contains("--remote")
                && !command.contains("--repo")
            {
                command.push_str(&format!(" --repo {spec}"));
            }
        }
    }
}

/// Split `https://host[/prefix]/OWNER/NAME` into `(Some(server), "OWNER/NAME")`;
/// anything else is returned as-is with no server.
fn split_repo_url(spec: &str) -> (Option<String>, String) {
    let trimmed = spec.trim().trim_end_matches('/');
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return (None, spec.to_string());
    }
    let mut parts = trimmed.rsplitn(3, '/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(name), Some(owner), Some(base)) if !base.ends_with(':') && !base.ends_with('/') => {
            (Some(base.to_string()), format!("{owner}/{name}"))
        }
        _ => (None, spec.to_string()),
    }
}

/// The outcome of asking the server.
enum Answer<T> {
    Served(T),
    /// The server predates the review API.
    OlderServer,
}

async fn get_json(target: &ReviewTarget, branch: &str, rest: &str) -> Result<Answer<Value>> {
    let url = target.api(rest);
    let timeout = crate::http::read_only_probe_timeout();
    let remote = crate::http::redact_url_userinfo(&target.identity.remote_url);
    let mut retried = false;
    loop {
        let mut req = crate::http::api_client().get(&url).timeout(timeout);
        if let Some(token) = target.identity.token.as_deref() {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let resp = req.send().await.map_err(|error| {
            OakError::Http(format!(
                "review request to {remote}: {}",
                crate::http::transport_failure_reason(&error, timeout)
            ))
        })?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        match status.as_u16() {
            200 => {
                let body: Value = resp.json().await.map_err(|e| {
                    OakError::Server(format!("review response from {remote} was not JSON: {e}"))
                })?;
                return Ok(Answer::Served(body));
            }
            401 => {
                return Err(OakError::AuthRequired {
                    remote,
                    status: 401,
                })
            }
            404 => {
                // The review API marks its own 404s (`error_code`); a bare
                // 404 is a server without the review routes.
                let body = resp.text().await.unwrap_or_default();
                let coded = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("error_code").cloned())
                    .is_some();
                if !coded {
                    return Ok(Answer::OlderServer);
                }
                let message = crate::http::json_error_message(&body)
                    .unwrap_or_else(|| format!("branch '{branch}' not found"));
                return Err(if message.starts_with("Branch ") {
                    OakError::BranchNotFound(branch.to_string())
                } else {
                    OakError::Server(message)
                });
            }
            503 if !retried && retry_after.is_some_and(|s| s <= MAX_RETRY_AFTER_SECS) => {
                // Server-wide capacity shed: transient, retry at the same width.
                retried = true;
                tokio::time::sleep(std::time::Duration::from_secs(retry_after.unwrap_or(1))).await;
                continue;
            }
            429 | 503 => {
                let body = crate::http::error_text(resp).await;
                let wait = retry_after
                    .map(|s| format!(" (retry after {s}s)"))
                    .unwrap_or_default();
                let who = if status.as_u16() == 429 {
                    "this client is over its review rate or concurrency allowance"
                } else {
                    "the server's review capacity is saturated"
                };
                return Err(OakError::Server(format!("{who}{wait}: {body}")));
            }
            _ => return Err(crate::http::server_error(resp).await),
        }
    }
}

/// The raw `merge-preview` body for `branch` pinned at `head` against the live
/// `against` head, or `None` from a server without the review API.
pub(crate) async fn merge_verdict(
    identity: RemoteIdentity,
    branch: &str,
    against: &str,
    head: &str,
) -> Result<Option<Value>> {
    let target = ReviewTarget {
        identity,
        repo_arg: None,
        in_checkout: true,
    };
    let rest = format!(
        "branches/{}/merge-preview?against={}&branch_head={head}",
        urlencoding::encode(branch),
        urlencoding::encode(against),
    );
    Ok(match get_json(&target, branch, &rest).await? {
        Answer::Served(body) => Some(body),
        Answer::OlderServer => None,
    })
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct Pins {
    branch_head: String,
    target_head: Option<String>,
    diff_base: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Row {
    path: String,
    change_type: String,
    old_path: Option<String>,
    content_state: String,
    additions: Option<usize>,
    deletions: Option<usize>,
    #[serde(default)]
    merge_safety: Option<String>,
    #[serde(default)]
    patch: Option<String>,
    #[serde(default)]
    patch_omitted: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ConflictRow {
    path: String,
    kind: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DiffPage {
    pins: Pins,
    heads_current: bool,
    #[serde(default)]
    branch_commit_count: usize,
    changed_file_count: usize,
    #[serde(default)]
    restricted_paths_omitted: usize,
    files: Vec<Row>,
    #[serde(default)]
    conflict_file_count: usize,
    #[serde(default)]
    conflict_files: Vec<ConflictRow>,
    #[serde(default)]
    conflicts_restricted: usize,
    #[serde(default)]
    byte_budget_exhausted: bool,
    #[serde(default)]
    time_budget_exhausted: bool,
    #[serde(default)]
    prediction_budget_exhausted: bool,
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ServerMergeSafety {
    assessment_kind: Option<String>,
    certified: bool,
    verdict: String,
    uncertified_cause: Option<String>,
    target_head_source: String,
    assessed_target: String,
    fork_base_commit: Option<String>,
    branch_head: Option<String>,
    target_head: Option<String>,
    invariant_violation_count: usize,
    #[serde(default)]
    invariant_violations_restricted: usize,
    #[serde(default)]
    invariant_violations_sample: Vec<String>,
    #[serde(default)]
    overlap_count: usize,
    #[serde(default)]
    overlap_paths_sample: Vec<String>,
    violations_digest: Option<String>,
    #[serde(default)]
    ack_key_id: String,
    #[serde(default)]
    classifier_version: u32,
    #[serde(default)]
    reasons: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Preview {
    #[serde(default)]
    heads_current: bool,
    pins: Pins,
    prediction: String,
    reason: Option<String>,
    clean: Option<bool>,
    conflict_file_count: usize,
    conflict_files: Vec<ConflictRow>,
    #[serde(default)]
    conflicts_restricted: usize,
    #[serde(default)]
    invariant_violations: Vec<String>,
    #[serde(default)]
    invariant_violations_truncated: bool,
    merge_safety: ServerMergeSafety,
}

/// Map a server enum value onto the CLI's `&'static str` vocabulary.
/// Unknown values from a newer server pass as a fixed sentinel rather than
/// being guessed at.
fn static_value(value: &str, known: &[&'static str]) -> &'static str {
    known
        .iter()
        .copied()
        .find(|k| *k == value)
        .unwrap_or("unrecognized")
}

fn change_status(change_type: &str) -> &'static str {
    static_value(change_type, &["added", "modified", "deleted", "renamed"])
}

fn conflict_type(kind: &str) -> &'static str {
    match kind {
        "text" => "content",
        "modify_delete" => "modify_delete",
        "mode" => "mode",
        "binary" => "binary",
        "blob_unavailable" => "content_unavailable",
        _ => "unrecognized",
    }
}

fn summary_from_row(row: &Row, content_requested: bool) -> FileSummaryJson {
    let binary_or_large = matches!(row.content_state.as_str(), "binary" | "too_large");
    let stats_available = row.content_state == "text";
    let unavailable = match row.content_state.as_str() {
        "blob_unavailable" => Some(OMITTED_REMOTE_CONTENT_UNAVAILABLE),
        "not_computed" if content_requested => Some(OMITTED_REMOTE_CONTENT_BUDGET),
        _ => None,
    };
    let patch_omitted_reason = if !row.patch_omitted {
        None
    } else if stats_available {
        Some(OMITTED_BYTE_BUDGET)
    } else {
        unavailable.or(Some(OMITTED_REMOTE_CONTENT_BUDGET))
    };
    FileSummaryJson {
        path: row.path.clone(),
        status: change_status(&row.change_type),
        old_path: row.old_path.clone(),
        additions: row.additions,
        deletions: row.deletions,
        binary_or_large,
        stats_available,
        content_unavailable_reason: unavailable,
        category: classify_change(&row.path, None, None, binary_or_large),
        patch: row.patch.clone(),
        hunks: None,
        patch_omitted: row.patch_omitted && patch_omitted_reason.is_some(),
        patch_omitted_reason,
        merge_safety: row
            .merge_safety
            .as_deref()
            .map(|class| static_value(class, &["invariant_violation", "branch_change", "overlap"])),
        basis: None,
    }
}

struct DiffQuery<'a> {
    mode: &'static str,
    against: &'a str,
    paths: &'a [String],
    patches: bool,
    stats: bool,
    max_bytes: usize,
    context: usize,
    force_text: bool,
    pins: Option<&'a Pins>,
}

impl DiffQuery<'_> {
    fn render(&self, cursor: Option<&str>) -> String {
        let mut q = format!(
            "mode={}&against={}&limit={PAGE_LIMIT}",
            self.mode,
            urlencoding::encode(self.against)
        );
        if !self.paths.is_empty() {
            q.push_str(&format!(
                "&paths={}",
                urlencoding::encode(&self.paths.join(","))
            ));
        }
        if self.patches {
            q.push_str(&format!(
                "&patches=1&max_bytes={}&context={}",
                self.max_bytes.min(SERVER_MAX_BYTES),
                self.context.min(100)
            ));
        } else if self.stats {
            q.push_str("&stats=1");
        }
        if self.force_text {
            q.push_str("&force_text=1");
        }
        match cursor {
            Some(c) => q.push_str(&format!("&cursor={}", urlencoding::encode(c))),
            None => {
                if let Some(pins) = self.pins {
                    q.push_str(&format!("&branch_head={}", pins.branch_head));
                    if let Some(t) = pins.target_head.as_deref() {
                        q.push_str(&format!("&target_head={t}"));
                    } else {
                        q.push_str("&target_head_absent=1");
                    }
                }
            }
        }
        q
    }
}

/// Every page of one listing: rows in server order plus the first page's
/// header fields.
async fn fetch_listing(
    target: &ReviewTarget,
    branch: &str,
    query: &DiffQuery<'_>,
) -> Result<Answer<(DiffPage, Vec<Row>)>> {
    let mut cursor: Option<String> = None;
    let mut first: Option<DiffPage> = None;
    let mut rows = Vec::new();
    loop {
        let rest = format!(
            "branches/{}/diff?{}",
            urlencoding::encode(branch),
            query.render(cursor.as_deref())
        );
        let page = match get_json(target, branch, &rest).await? {
            Answer::Served(v) => v,
            Answer::OlderServer => return Ok(Answer::OlderServer),
        };
        let page: DiffPage = serde_json::from_value(page)
            .map_err(|e| OakError::Server(format!("unexpected review diff response: {e}")))?;
        let expected = first.as_ref().map(|p| &p.pins).or(query.pins);
        if expected.is_some_and(|p| {
            p.branch_head != page.pins.branch_head || p.target_head != page.pins.target_head
        }) {
            return Err(OakError::Server(
                "review listing heads changed between pages; retry review".into(),
            ));
        }
        rows.extend(page.files.iter().cloned());
        cursor = page.next_cursor.clone();
        let flags = (page.byte_budget_exhausted, page.time_budget_exhausted);
        match first.as_mut() {
            None => first = Some(page),
            Some(f) => {
                f.byte_budget_exhausted |= flags.0;
                f.time_budget_exhausted |= flags.1;
            }
        }
        if cursor.is_none() {
            break;
        }
    }
    Ok(Answer::Served((first.expect("at least one page"), rows)))
}

/// Read content (stats and/or patches) for exactly `paths`, pinned to
/// `pins`, in as few requests as the URL budget allows. The patch byte
/// budget is shared across the requests.
async fn fetch_content(
    target: &ReviewTarget,
    branch: &str,
    base: &DiffQuery<'_>,
    pins: &Pins,
    paths: &[String],
    max_bytes: usize,
) -> Result<(Vec<Row>, bool)> {
    let mut out = Vec::new();
    let mut budget_left = max_bytes;
    let mut exhausted = false;
    let mut group: Vec<String> = Vec::new();
    let mut group_len = 0usize;
    let mut groups = Vec::new();
    for path in paths {
        // A path containing the list separator can't be selected exactly;
        // it is left without content (reported as a budget omission).
        if path.contains(',') {
            continue;
        }
        if group_len + path.len() + 1 > PATHS_QUERY_BUDGET && !group.is_empty() {
            groups.push(std::mem::take(&mut group));
            group_len = 0;
        }
        group_len += path.len() + 1;
        group.push(path.clone());
    }
    if !group.is_empty() {
        groups.push(group);
    }
    for group in groups {
        let query = DiffQuery {
            paths: &group,
            max_bytes: budget_left,
            pins: Some(pins),
            ..*base
        };
        let (page, rows) = match fetch_listing(target, branch, &query).await? {
            Answer::Served(x) => x,
            Answer::OlderServer => {
                return Err(OakError::Server(
                    "the review API disappeared mid-review; retry".to_string(),
                ))
            }
        };
        exhausted |= page.byte_budget_exhausted || page.time_budget_exhausted;
        for row in &rows {
            if let Some(patch) = row.patch.as_ref() {
                budget_left = budget_left.saturating_sub(patch.len());
            }
        }
        out.extend(rows);
    }
    Ok((out, exhausted))
}

fn pins_caveats(page_pins: &Pins, heads_current: bool, restricted: usize) -> Vec<String> {
    let mut caveats = vec![format!(
        "Computed by the server's review API at pinned heads (branch_head={}, target_head={}); no local checkout, ref change or object download was needed.",
        page_pins.branch_head,
        page_pins.target_head.as_deref().unwrap_or("none"),
    )];
    if !heads_current {
        caveats.push(
            "A branch head moved while this review was being assembled; the evidence describes the pinned heads above, not the newest ones."
                .to_string(),
        );
    }
    if restricted > 0 {
        caveats.push(format!(
            "{restricted} changed path(s) are hidden by this repository's path permissions and are counted but not listed."
        ));
    }
    caveats
}

fn mode_param(mode: DiffMode) -> &'static str {
    match mode {
        DiffMode::Tree => "tree",
        DiffMode::Contribution => "contribution",
        DiffMode::NetMerge => "net-merge",
    }
}

fn comparison_source(mode: DiffMode) -> &'static str {
    match mode {
        DiffMode::Tree => "server_snapshot",
        DiffMode::Contribution => "fork_point",
        DiffMode::NetMerge => "predicted_merge_result",
    }
}

fn repo_paths(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|p| {
            p.to_string_lossy()
                .trim_start_matches("./")
                .trim_matches('/')
                .to_string()
        })
        .filter(|p| !p.is_empty() && p != ".")
        .collect()
}

// ---------------------------------------------------------------------------
// oak branch diff --remote
// ---------------------------------------------------------------------------

async fn server_branch_diff(
    target: &ReviewTarget,
    path: &Path,
    branch: &str,
    against: &str,
    diff_mode: DiffMode,
    paths: &[PathBuf],
    options: DiffJsonOptions,
) -> Result<Answer<DiffJson>> {
    let DiffJsonOptions {
        changed_files_limit,
        changed_files_offset,
        hunks,
        max_bytes,
        context,
        force_text,
        line_numbers,
    } = options;
    validate_changed_files_window(changed_files_limit)?;
    // Filters are repo-relative; inside a checkout resolve them from the
    // working directory like the local path does.
    let filters = if target.in_checkout && target.repo_arg.is_none() {
        let workspace = crate::commands::branch::RemoteWorkspace::open(path)?;
        crate::commands::diff::resolve_filters(path, &workspace.work_tree, paths)?
    } else {
        repo_paths(paths)
    };
    let listing_query = DiffQuery {
        mode: mode_param(diff_mode),
        against,
        paths: &filters,
        patches: false,
        stats: false,
        max_bytes: 0,
        context,
        force_text: false,
        pins: None,
    };
    let (page, rows) = match fetch_listing(target, branch, &listing_query).await? {
        Answer::Served(x) => x,
        Answer::OlderServer => return Ok(Answer::OlderServer),
    };

    // Content only for the returned window.
    let total = rows.len();
    let start = changed_files_offset.min(total);
    let end = changed_files_limit
        .map(|l| start.saturating_add(l).min(total))
        .unwrap_or(total);
    let window_paths: Vec<String> = rows[start..end].iter().map(|r| r.path.clone()).collect();
    let content_query = DiffQuery {
        patches: hunks,
        stats: !hunks,
        context,
        force_text,
        ..listing_query
    };
    let (content_rows, content_exhausted) = if window_paths.is_empty() {
        (Vec::new(), false)
    } else {
        fetch_content(
            target,
            branch,
            &content_query,
            &page.pins,
            &window_paths,
            max_bytes.unwrap_or(SERVER_MAX_BYTES),
        )
        .await?
    };
    let by_path: std::collections::HashMap<&str, &Row> =
        content_rows.iter().map(|r| (r.path.as_str(), r)).collect();
    let summaries: Vec<FileSummaryJson> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            if (start..end).contains(&i) {
                match by_path.get(row.path.as_str()) {
                    Some(full) => summary_from_row(full, true),
                    None => summary_from_row(row, true),
                }
            } else {
                summary_from_row(row, false)
            }
        })
        .collect();

    let next_command = changed_files_limit.map(|limit| {
        let mut command = super::diff_page_command(
            branch,
            against,
            true,
            diff_mode,
            limit,
            changed_files_offset.saturating_add(limit),
        );
        super::append_hunk_flags(&mut command, hunks, max_bytes, context, force_text);
        if hunks && line_numbers {
            command.push_str(" --line-numbers");
        }
        command
    });
    let (mut changed_files, changed_files_page) = window_changed_files(
        &summaries,
        changed_files_limit,
        changed_files_offset,
        next_command,
    );
    let evidence_kind = evidence_kind_for_source(comparison_source(diff_mode));
    qualify_snapshot_rows(&mut changed_files, evidence_kind);
    if line_numbers {
        super::hunks::attach_line_numbers(&mut changed_files);
    }
    let hunks_truncated = changed_files
        .iter()
        .any(|f| f.patch_omitted_reason == Some(OMITTED_BYTE_BUDGET));

    let mut caveats = pins_caveats(
        &page.pins,
        page.heads_current,
        page.restricted_paths_omitted,
    );
    if content_exhausted {
        caveats.push(
            "The server's per-request byte or time budget ran out before every returned file was read; those rows say so (content_unavailable_reason / patch_omitted_reason)."
                .to_string(),
        );
    }
    if page.prediction_budget_exhausted {
        caveats.push(
            "The merge prediction ran out of budget; some paths are reported as content_unavailable conflicts."
                .to_string(),
        );
    }
    if page.conflicts_restricted > 0 {
        caveats.push(format!(
            "{} predicted conflict(s) are under paths hidden by path permissions and are not named.",
            page.conflicts_restricted
        ));
    }

    let mut recommended = branch_diff_recommendations(branch, against, true, diff_mode);
    if hunks_truncated {
        let stub = super::branch_diff_command_stub(branch, against, true, diff_mode);
        let flags = super::hunk_followup_flags("--json --hunks", line_numbers);
        for command in per_file_commands(&stub, &changed_files, &flags, |file| {
            file.patch_omitted_reason == Some(OMITTED_BYTE_BUDGET)
        })
        .into_iter()
        .rev()
        {
            recommended.insert(0, command);
        }
    }
    target.qualify_commands(&mut recommended);

    Ok(Answer::Served(DiffJson {
        schema_version: SCHEMA_VERSION,
        kind: "remote_branch_diff",
        diff_mode: diff_mode.as_str(),
        evidence_kind: Some(evidence_kind),
        branch: Some(branch.to_string()),
        against: Some(against.to_string()),
        branch_head: Some(page.pins.branch_head.clone()),
        parent: Some(against.to_string()),
        changed_file_count: page.changed_file_count,
        changed_files,
        changed_files_page,
        files_identical_to_against: None,
        hunks_truncated,
        conflict_file_count: page.conflict_file_count,
        conflict_files: page.conflict_files.iter().map(|c| c.path.clone()).collect(),
        merge_lineage_evidence: None,
        acquisition: None,
        ancestry_diagnostic: None,
        caveats,
        recommended_next_commands: recommended,
    }))
}

fn fallback_requires_checkout(target: &ReviewTarget) -> Result<()> {
    if target.in_checkout {
        Ok(())
    } else {
        Err(OakError::Server(format!(
            "{} does not serve the checkout-free review API yet; run this from a checkout of {}/{} to compute the review locally",
            crate::http::redact_url_userinfo(&target.identity.remote_url),
            target.identity.owner,
            target.identity.repo_name,
        )))
    }
}

pub async fn branch_diff_json(
    path: &Path,
    repo_arg: Option<&str>,
    branch: &str,
    against: &str,
    diff_mode: DiffMode,
    paths: &[PathBuf],
    options: DiffJsonOptions,
) -> Result<()> {
    let target = ReviewTarget::resolve(path, repo_arg)?;
    match server_branch_diff(&target, path, branch, against, diff_mode, paths, options).await? {
        Answer::Served(diff) => output::print_json(&diff),
        Answer::OlderServer => {
            fallback_requires_checkout(&target)?;
            super::remote_branch_diff_json(path, branch, against, diff_mode, paths, options).await
        }
    }
}

pub async fn branch_diff_print(
    path: &Path,
    repo_arg: Option<&str>,
    branch: &str,
    against: &str,
    diff_mode: DiffMode,
    paths: &[PathBuf],
    options: DiffJsonOptions,
) -> Result<()> {
    let target = ReviewTarget::resolve(path, repo_arg)?;
    let options = DiffJsonOptions {
        hunks: true,
        ..options
    };
    match server_branch_diff(&target, path, branch, against, diff_mode, paths, options).await? {
        Answer::Served(diff) => {
            super::print_remote_diff(&diff, options.line_numbers);
            Ok(())
        }
        Answer::OlderServer => {
            fallback_requires_checkout(&target)?;
            super::remote_branch_diff_print(path, branch, against, diff_mode, paths, options).await
        }
    }
}

// ---------------------------------------------------------------------------
// oak branch review --remote
// ---------------------------------------------------------------------------

fn lineage(pins: &Pins, against: &str, branch_commit_count: usize) -> LineageJson {
    LineageJson {
        branch_parent: Some(against.to_string()),
        branch_own_head: Some(pins.branch_head.clone()),
        branch_effective_head: Some(pins.branch_head.clone()),
        against_head: pins.target_head.clone(),
        comparison_head: pins.target_head.clone(),
        comparison_source: "server_review_api".to_string(),
        fork_point: pins.diff_base.clone(),
        branch_commit_count,
        caveats: Vec::new(),
    }
}

fn merge_preview_from_server(
    branch: &str,
    against: &str,
    preview: Preview,
    net: Option<(&DiffPage, Vec<FileSummaryJson>)>,
    branch_commit_count: usize,
) -> MergePreviewJson {
    let mut safety = preview.merge_safety;
    let bound = safety.assessed_target == against
        && safety.branch_head.as_deref() == Some(preview.pins.branch_head.as_str())
        && safety.target_head == preview.pins.target_head;
    let initialization = safety.assessment_kind.as_deref() == Some("target_initialization")
        && preview.heads_current
        && safety.target_head_source == "server_live"
        && against == oak_core::DEFAULT_BRANCH
        && safety.assessed_target == against
        && safety.branch_head.as_deref() == Some(preview.pins.branch_head.as_str())
        && safety.target_head.is_none()
        && preview.pins.target_head.is_none()
        && safety.fork_base_commit.is_none()
        && preview.pins.diff_base.is_none();
    // A null head is not proof. Older or inconsistent responses stay conservative.
    if preview.pins.target_head.is_none() && !initialization {
        safety.certified = false;
        safety.verdict = "uncertified".into();
        safety.uncertified_cause = Some("target_initialization_unproven".into());
    }
    if !bound {
        safety.certified = false;
        safety.verdict = "uncertified".into();
        safety.uncertified_cause = Some("assessment_pins_mismatch".into());
    }
    let available = preview.prediction == "available";
    let violations_hidden = safety.invariant_violation_count > preview.invariant_violations.len();
    let mut caveats = vec![
        "Merge prediction and four-tree safety classification computed by the server at the pinned heads."
            .to_string(),
    ];
    if let Some(reason) = preview.reason.as_deref() {
        caveats.push(format!("server prediction note: {reason}"));
    }
    if violations_hidden {
        caveats.push(format!(
            "{} of {} invariant violation(s) are not listed (path permissions or the server's listing cap); the verdict counts all of them.",
            safety.invariant_violation_count - preview.invariant_violations.len(),
            safety.invariant_violation_count
        ));
    }
    if preview.conflicts_restricted > 0 {
        caveats.push(format!(
            "{} predicted conflict(s) are under paths hidden by path permissions and are not named.",
            preview.conflicts_restricted
        ));
    }
    if safety.ack_key_id.is_empty() {
        caveats.push(
            "This server has no merge acknowledgement key configured; overrides are unavailable."
                .to_string(),
        );
    }
    if initialization {
        caveats[0] = "Server verified initialization of an existing open headless target at the pinned source head; no fork commit exists. The merge must still pass live publication gates.".into();
    }
    let clean = if available { preview.clean } else { None };
    let target_risk = if !available {
        TargetRisk::Unknown
    } else if safety.invariant_violation_count > 0 {
        TargetRisk::RevertsTargetExact
    } else {
        TargetRisk::None
    };
    let recommended = super::merge_next_commands(
        branch,
        false,
        true,
        if safety.invariant_violation_count > 0 {
            super::MergeReadiness::Violation
        } else if !safety.certified {
            super::MergeReadiness::Uncertified {
                repair_history: false,
                conflicts: clean == Some(false),
            }
        } else if clean == Some(true) {
            super::MergeReadiness::Clean
        } else {
            super::MergeReadiness::Conflicts
        },
    );
    let (changed_file_count, changed_files) = match net {
        Some((page, files)) => (Some(page.changed_file_count), files),
        None => (None, Vec::new()),
    };
    // A hidden violation still blocks: never let an empty visible list read
    // as "classified clean".
    let invariant_violations = if violations_hidden && preview.invariant_violations.is_empty() {
        Some(vec![format!(
            "<{} path(s) not visible to this caller>",
            safety.invariant_violation_count
        )])
    } else {
        Some(preview.invariant_violations.clone())
    };
    let merge_would_succeed = if !available {
        None
    } else if clean == Some(false) || safety.invariant_violation_count > 0 {
        Some(false)
    } else {
        (clean == Some(true) && safety.certified).then_some(true)
    };
    let _ = preview.invariant_violations_truncated;
    MergePreviewJson {
        schema_version: SCHEMA_VERSION,
        branch: branch.to_string(),
        parent: Some(against.to_string()),
        branch_head: Some(preview.pins.branch_head.clone()),
        parent_head: preview.pins.target_head.clone(),
        prediction_available: available,
        prediction_method: "server_review_api",
        clean,
        target_risk,
        conflict_file_count: preview.conflict_file_count,
        conflict_files: preview
            .conflict_files
            .iter()
            .map(|c| ConflictFileJson {
                path: c.path.clone(),
                conflict_type: conflict_type(&c.kind),
            })
            .collect(),
        changed_file_count,
        changed_files,
        invariant_violations,
        merge_would_succeed,
        merge_safety: Some(MergeSafetyJson {
            assessment_kind: if initialization {
                "target_initialization"
            } else {
                "four_tree"
            },
            certified: safety.certified,
            verdict: static_value(&safety.verdict, &["safe", "do_not_merge", "uncertified"]),
            uncertified_cause: safety.uncertified_cause.as_deref().map(|c| {
                static_value(
                    c,
                    &[
                        "stale_pinned_target",
                        "fork_base_unavailable",
                        "classification_incomplete",
                        "target_initialization_unproven",
                        "assessment_pins_mismatch",
                    ],
                )
            }),
            target_head_source: static_value(
                &safety.target_head_source,
                &["server_live", "pinned_stale"],
            ),
            assessed_target: safety.assessed_target,
            fetched_target: None,
            fetched_target_head: None,
            fork_base_commit: safety.fork_base_commit,
            branch_head: safety.branch_head,
            target_head: safety.target_head,
            // The server classified at its own live head; this client made
            // no observation of its own to report.
            target_head_observed_at: None,
            invariant_violation_count: safety.invariant_violation_count,
            invariant_violations_sample: safety.invariant_violations_sample,
            overlap_count: safety.overlap_count,
            overlap_paths_sample: safety.overlap_paths_sample,
            reasons: safety.reasons,
            server: Some(ServerMergeSafetyJson {
                invariant_violations_restricted: safety.invariant_violations_restricted,
                violations_digest: safety.violations_digest,
                ack_key_id: safety.ack_key_id,
                classifier_version: safety.classifier_version,
            }),
        }),
        merge_lineage_evidence: lineage(&preview.pins, against, branch_commit_count),
        caveats,
        recommended_next_commands: recommended,
    }
}

fn unavailable_preview(branch: &str, against: &str, pins: &Pins, count: usize) -> MergePreviewJson {
    MergePreviewJson {
        schema_version: SCHEMA_VERSION,
        branch: branch.to_string(),
        parent: Some(against.to_string()),
        branch_head: Some(pins.branch_head.clone()),
        parent_head: pins.target_head.clone(),
        prediction_available: false,
        prediction_method: "server_review_api",
        clean: None,
        target_risk: TargetRisk::Unknown,
        conflict_file_count: 0,
        conflict_files: Vec::new(),
        changed_file_count: None,
        changed_files: Vec::new(),
        invariant_violations: None,
        merge_safety: None,
        merge_would_succeed: None,
        merge_lineage_evidence: lineage(pins, against, count),
        caveats: vec!["merge prediction skipped for summary analysis depth".to_string()],
        recommended_next_commands: Vec::new(),
    }
}

async fn server_branch_review(
    target: &ReviewTarget,
    branch: &str,
    merge_preview: bool,
    against: &str,
    changed_files_limit: Option<usize>,
    changed_files_offset: usize,
) -> Result<Answer<ReviewJson>> {
    validate_changed_files_window(changed_files_limit)?;
    // Pin once: the snapshot listing fixes both heads, and every later call
    // carries them.
    let tree_query = DiffQuery {
        mode: "tree",
        against,
        paths: &[],
        patches: false,
        stats: true,
        max_bytes: 0,
        context: 3,
        force_text: false,
        pins: None,
    };
    let (tree_page, tree_rows) = match fetch_listing(target, branch, &tree_query).await? {
        Answer::Served(x) => x,
        Answer::OlderServer => return Ok(Answer::OlderServer),
    };
    let pins = tree_page.pins.clone();
    let net_query = DiffQuery {
        mode: "net-merge",
        stats: false,
        pins: Some(&pins),
        ..tree_query
    };
    let preview_rest = format!(
        "branches/{}/merge-preview?against={}&branch_head={}{}",
        urlencoding::encode(branch),
        urlencoding::encode(against),
        pins.branch_head,
        pins.target_head
            .as_deref()
            .map(|t| format!("&target_head={t}"))
            .unwrap_or_else(|| "&target_head_absent=1".into()),
    );
    let (net, preview) = tokio::join!(fetch_listing(target, branch, &net_query), async {
        if merge_preview {
            get_json(target, branch, &preview_rest).await.map(Some)
        } else {
            Ok(None)
        }
    });
    let (net_page, net_rows) = match net? {
        Answer::Served(x) => x,
        Answer::OlderServer => return Ok(Answer::OlderServer),
    };
    let preview: Option<Preview> = match preview? {
        Some(Answer::Served(v)) => Some(
            serde_json::from_value(v)
                .map_err(|e| OakError::Server(format!("unexpected merge-preview response: {e}")))?,
        ),
        Some(Answer::OlderServer) => return Ok(Answer::OlderServer),
        None => None,
    };

    // Every component must describe the same immutable pair, including an absent target.
    let same_heads = |other: &Pins| {
        other.branch_head == pins.branch_head && other.target_head == pins.target_head
    };
    if !same_heads(&net_page.pins) || preview.as_ref().is_some_and(|p| !same_heads(&p.pins)) {
        return Err(OakError::Server(
            "review heads changed between responses; retry review".into(),
        ));
    }
    let tree_files: Vec<FileSummaryJson> = tree_rows
        .iter()
        .map(|r| summary_from_row(r, true))
        .collect();
    let net_files: Vec<FileSummaryJson> = net_rows
        .iter()
        .map(|r| summary_from_row(r, false))
        .collect();
    let branch_commit_count = tree_page.branch_commit_count;

    // Contribution, from the server's diffs: no own commits → empty; nothing
    // a merge would change (no net-merge rows or conflicts) → superseded.
    let contribution = if branch_commit_count == 0 {
        ContributionState::Empty
    } else if tree_page.changed_file_count == 0
        || (net_page.changed_file_count == 0 && net_page.conflict_file_count == 0)
    {
        ContributionState::SupersededExact
    } else {
        ContributionState::Contributes
    };

    let preview_json = match preview {
        Some(p) => merge_preview_from_server(
            branch,
            against,
            p,
            Some((&net_page, net_files.clone())),
            branch_commit_count,
        ),
        None => unavailable_preview(branch, against, &pins, branch_commit_count),
    };
    let missing_blob_paths: Vec<String> = tree_files
        .iter()
        .filter(|f| !f.stats_available && f.content_unavailable_reason.is_some())
        .map(|f| f.path.clone())
        .collect();
    let empty = oak_core::Manifest::empty();
    let mut triage = derive_branch_triage_from_evidence(
        BranchTriageInput {
            branch,
            remote: true,
            branch_commit_count,
            changed_files: &[],
            branch_manifest: &empty,
            against_manifest: &empty,
            missing_blob_paths: &missing_blob_paths,
            local_snapshot_missing: false,
            against_head_missing: pins.target_head.is_none()
                && !preview_json
                    .merge_safety
                    .as_ref()
                    .is_some_and(|s| s.certified && s.assessment_kind == "target_initialization"),
            merge_preview: &preview_json,
        },
        contribution,
    )?;
    // `changed_files` above feeds only the unique-contribution sample; fill
    // it from the server's net-merge rows instead.
    triage.unique_contribution.changed_file_count = net_page.changed_file_count;
    triage.unique_contribution.changed_paths_sample =
        net_rows.iter().take(20).map(|r| r.path.clone()).collect();
    let checks =
        CiEvidence::exact_for_remote(&target.identity, branch, Some(&pins.branch_head)).await;
    apply_checks(&mut triage, checks);

    let page_command = |limit: usize| {
        review_page_command(
            branch,
            true,
            merge_preview,
            limit,
            changed_files_offset.saturating_add(limit),
        )
    };
    let preview_out = merge_preview.then(|| {
        review_merge_preview_json(
            preview_json,
            &tree_files,
            changed_files_limit,
            changed_files_offset,
            changed_files_limit.map(page_command),
        )
    });
    let (mut changed_files, changed_files_page) = window_changed_files(
        &tree_files,
        changed_files_limit,
        changed_files_offset,
        changed_files_limit.map(page_command),
    );
    qualify_snapshot_rows(&mut changed_files, EVIDENCE_SNAPSHOT);

    let mut caveats = pins_caveats(
        &pins,
        tree_page.heads_current,
        tree_page.restricted_paths_omitted,
    );
    if crate::commands::triage::window_incomplete(&triage.checks) {
        caveats.push(crate::commands::triage::CHECKS_WINDOW_INCOMPLETE_CAVEAT.to_string());
    }
    let mut recommended = review_recommendations(
        branch,
        true,
        preview_out.as_ref(),
        triage.recommended_action,
        &triage.recommended_action_detail.command,
    );
    target.qualify_commands(&mut recommended);
    let mut preview_out = preview_out;
    if let Some(p) = preview_out.as_mut() {
        target.qualify_commands(&mut p.recommended_next_commands);
    }

    Ok(Answer::Served(ReviewJson {
        schema_version: SCHEMA_VERSION,
        branch: branch.to_string(),
        against: against.to_string(),
        branch_head: Some(pins.branch_head.clone()),
        parent: Some(against.to_string()),
        worktree_dirty: None,
        dirty_file_count: None,
        changed_file_count: tree_page.changed_file_count,
        evidence_kind: EVIDENCE_SNAPSHOT,
        changed_files,
        changed_files_page,
        files_identical_to_against: identical_files_unavailable(
            against,
            vec!["not computed by the server review API".to_string()],
        ),
        merge_lineage_evidence: lineage(&pins, against, branch_commit_count),
        ancestry_diagnostic: None,
        merge_preview: preview_out,
        recommended_action: triage.recommended_action,
        recommended_action_detail: triage.recommended_action_detail,
        reason: triage.reason,
        confidence: triage.confidence,
        analysis_depth: triage.analysis_depth,
        analysis_budget_exhausted: triage.analysis_budget_exhausted,
        next_detail_command: triage.next_detail_command,
        close_allowed: triage.close_allowed,
        vcs_merge_safe: triage.vcs_merge_safe,
        merge_allowed: triage.merge_allowed,
        checks: triage.checks,
        mergeability: triage.mergeability,
        contribution: triage.contribution,
        target_risk: triage.target_risk,
        unique_contribution: triage.unique_contribution,
        missing_data: triage.missing_data,
        acquisition: None,
        caveats,
        recommended_next_commands: recommended,
    }))
}

pub async fn branch_review_json(
    path: &Path,
    repo_arg: Option<&str>,
    branch: &str,
    merge_preview: bool,
    against: &str,
    changed_files_limit: Option<usize>,
    changed_files_offset: usize,
) -> Result<()> {
    let target = ReviewTarget::resolve(path, repo_arg)?;
    match server_branch_review(
        &target,
        branch,
        merge_preview,
        against,
        changed_files_limit,
        changed_files_offset,
    )
    .await?
    {
        Answer::Served(review) => output::print_json(&review),
        Answer::OlderServer => {
            fallback_requires_checkout(&target)?;
            super::remote_branch_review_json(
                path,
                branch,
                merge_preview,
                against,
                changed_files_limit,
                changed_files_offset,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: &str, patch_omitted: bool) -> Row {
        Row {
            path: "docs/guide.md".into(),
            change_type: "modified".into(),
            old_path: None,
            content_state: state.into(),
            additions: (state == "text").then_some(2),
            deletions: (state == "text").then_some(1),
            merge_safety: None,
            patch: None,
            patch_omitted,
        }
    }

    #[test]
    fn content_state_splits_the_old_binary_or_large_guess() {
        let text = summary_from_row(&row("text", false), true);
        assert!(text.stats_available && !text.binary_or_large);
        assert_eq!(text.additions, Some(2));
        // fb-89/94: an unreadable blob is NOT binary_or_large.
        let missing = summary_from_row(&row("blob_unavailable", true), true);
        assert!(!missing.binary_or_large && !missing.stats_available);
        assert_eq!(
            missing.content_unavailable_reason,
            Some(OMITTED_REMOTE_CONTENT_UNAVAILABLE)
        );
        assert_eq!(
            missing.patch_omitted_reason,
            Some(OMITTED_REMOTE_CONTENT_UNAVAILABLE)
        );
        let large = summary_from_row(&row("too_large", false), true);
        assert!(large.binary_or_large);
        let budget = summary_from_row(&row("text", true), true);
        assert_eq!(budget.patch_omitted_reason, Some(OMITTED_BYTE_BUDGET));
        // Metadata-only rows outside the window make no content claim.
        let meta = summary_from_row(&row("not_computed", false), false);
        assert!(meta.content_unavailable_reason.is_none());
    }

    #[test]
    fn repo_url_names_the_server() {
        assert_eq!(
            split_repo_url("http://127.0.0.1:3000/acme/demo"),
            (Some("http://127.0.0.1:3000".into()), "acme/demo".into())
        );
        assert_eq!(split_repo_url("acme/demo"), (None, "acme/demo".into()));
        assert_eq!(
            split_repo_url("https://oak.space/acme"),
            (None, "https://oak.space/acme".into())
        );
    }

    #[test]
    fn server_enums_map_without_guessing() {
        assert_eq!(conflict_type("text"), "content");
        assert_eq!(conflict_type("blob_unavailable"), "content_unavailable");
        assert_eq!(static_value("server_live", &["server_live"]), "server_live");
        assert_eq!(static_value("new_thing", &["server_live"]), "unrecognized");
    }

    #[test]
    fn diff_query_carries_pins_only_without_a_cursor() {
        let pins = Pins {
            branch_head: "aa".repeat(32),
            target_head: Some("bb".repeat(32)),
            diff_base: None,
        };
        let paths = vec!["docs".to_string(), "src/a b.rs".to_string()];
        let q = DiffQuery {
            mode: "contribution",
            against: "main",
            paths: &paths,
            patches: true,
            stats: false,
            max_bytes: 10 * 1024 * 1024,
            context: 3,
            force_text: false,
            pins: Some(&pins),
        };
        let first = q.render(None);
        assert!(first.contains(&format!("branch_head={}", "aa".repeat(32))));
        assert!(first.contains("paths=docs%2Csrc%2Fa%20b.rs"));
        assert!(first.contains(&format!("max_bytes={SERVER_MAX_BYTES}")));
        let next = q.render(Some("abc"));
        assert!(next.contains("cursor=abc") && !next.contains("branch_head="));
    }
}
