//! `oak log --remote [--branch B] --json [-n N] [--from HASH]` (fb-432).
//!
//! A read-only, bounded first-parent walk of a remote branch through the
//! existing `POST commits/info` endpoint, starting from a head pinned by one
//! branch listing (or an exact `--from` hash). Every commit is verified
//! against its claimed hash before it is reported, so the rows are an
//! exact-head-bound chain. Nothing is written locally: commits already in the
//! local store are reused, fetched ones are not persisted (a commit without
//! its trees would look like acquired history to other commands).
//!
//! Merge-source identity: for a merge commit, `merge_source_branch` is the
//! `branch_name` recorded on its verified merge-parent commit, fetched in the
//! same request as the next first parent.

use std::collections::HashMap;
use std::path::Path;

use futures_util::StreamExt;
use oak_core::{Commit, Hash, OakError, Repository, Result};
use serde::{Deserialize, Serialize};

use crate::commands::blob_fetch::{AcquisitionCounters, ReviewPreparationBudget};
use crate::commands::branch::RemoteIdentity;
use crate::output;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 200;

#[derive(Serialize)]
struct CommitInfoRequest<'a> {
    hashes: &'a [String],
}

/// Only the commit rows are decoded; the trees the endpoint always attaches
/// are counted against the byte budget and discarded.
#[derive(Deserialize)]
struct CommitInfoResponse {
    commits: Vec<oak_core::protocol::CommitData>,
}

#[derive(Serialize)]
struct RemoteLogCommitJson {
    hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_parent: Option<String>,
    /// `branch_name` of the verified merge-parent commit. Absent for
    /// non-merge commits; `merge_source_unavailable` explains a merge commit
    /// whose source could not be read within the walk's budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_source_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_source_unavailable: Option<&'static str>,
    branch: String,
    timestamp: String,
    author: String,
    description_or_subject: String,
    /// `fb-N` references parsed from the commit message, in first-seen order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    feedback_refs: Vec<String>,
    files_changed: usize,
}

#[derive(Serialize)]
struct RemoteLogJson {
    schema_version: u32,
    kind: &'static str,
    branch: Option<String>,
    /// The exact commit the walk started from.
    head: String,
    /// `remote_branch_list` (pinned from one listing) or `from_argument`.
    head_source: &'static str,
    walk: &'static str,
    limit: usize,
    returned_count: usize,
    /// The first-parent chain reached a root commit: nothing older exists.
    complete: bool,
    truncated: bool,
    /// `limit`, `byte_budget`, `time_budget`, or `commit_unavailable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    truncation_reason: Option<&'static str>,
    /// Continue the exact chain from the next unreported commit (only after
    /// `limit` or a byte/time budget; always a commit older than `head`, so
    /// it never repeats the invoking command). Absent when complete or when a
    /// parent was `commit_unavailable`, since a rerun cannot fix that.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<String>,
    commits: Vec<RemoteLogCommitJson>,
    acquisition: AcquisitionCounters,
    caveats: Vec<String>,
}

struct Walker<'a> {
    repo: &'a dyn Repository,
    remote: &'a RemoteIdentity,
    client: reqwest::Client,
    budget: ReviewPreparationBudget,
    known: HashMap<Hash, Commit>,
}

impl Walker<'_> {
    /// Make every hash in `wanted` known (local store first, then one
    /// request for the rest). Commits the server omitted stay unknown. The
    /// inner `Err` names the budget (`byte_budget`/`time_budget`) that ran out.
    async fn acquire(&mut self, wanted: &[Hash]) -> Result<std::result::Result<(), &'static str>> {
        let mut missing: Vec<String> = Vec::new();
        for hash in wanted {
            if self.known.contains_key(hash) {
                continue;
            }
            match self.repo.get_commit(hash)? {
                Some(commit) => {
                    self.budget.record_commit_reused();
                    self.known.insert(hash.clone(), commit);
                }
                None => missing.push(hash.to_string()),
            }
        }
        if missing.is_empty() {
            return Ok(Ok(()));
        }
        missing.sort();
        missing.dedup();
        if self.budget.remaining().is_zero() {
            self.budget.record_exhaustion("time");
            return Ok(Err("time_budget"));
        }
        self.budget.record_commit_request(missing.len());
        let url = format!(
            "{}/api/{}/{}/commits/info",
            self.remote.remote_url, self.remote.owner, self.remote.repo_name
        );
        let mut request = self
            .client
            .post(&url)
            .json(&CommitInfoRequest { hashes: &missing });
        if let Some(token) = self.remote.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = match tokio::time::timeout(self.budget.remaining(), request.send()).await {
            Ok(response) => response.map_err(|e| OakError::Http(e.to_string()))?,
            Err(_) => {
                self.budget.record_exhaustion("time");
                return Ok(Err("time_budget"));
            }
        };
        if !response.status().is_success() {
            // Untrusted error bodies are not reflected.
            return Err(OakError::Server(format!(
                "remote log: commits/info failed: HTTP {} (response body omitted)",
                response.status()
            )));
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        loop {
            let Ok(next) = tokio::time::timeout(self.budget.remaining(), stream.next()).await
            else {
                self.budget.record_exhaustion("time");
                return Ok(Err("time_budget"));
            };
            let Some(frame) = next else { break };
            let frame = frame.map_err(|e| OakError::Http(e.to_string()))?;
            if frame.len() > self.budget.bytes_left {
                self.budget.record_exhaustion("bytes");
                return Ok(Err("byte_budget"));
            }
            self.budget.bytes_left -= frame.len();
            self.budget.record_downloaded_bytes(frame.len());
            bytes.extend_from_slice(&frame);
        }
        let info: CommitInfoResponse =
            serde_json::from_slice(&bytes).map_err(|e| OakError::Http(e.to_string()))?;
        for data in &info.commits {
            if !missing.contains(&data.hash) {
                return Err(OakError::Server(format!(
                    "remote log: commits/info returned unrequested commit {}",
                    data.hash
                )));
            }
            let commit = oak_core::protocol::commit_data_to_core(data)
                .map_err(|e| OakError::Server(format!("invalid commit object from server: {e}")))?;
            // Rehydration re-derived the claimed hash from the content.
            self.budget.record_verified_object();
            self.known.insert(commit.hash.clone(), commit);
        }
        Ok(Ok(()))
    }
}

fn feedback_refs(message: Option<&str>) -> Vec<String> {
    use std::sync::OnceLock;
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN
        .get_or_init(|| regex::Regex::new(r"(?i)\bfb-?(\d{1,7})\b").expect("static pattern"));
    let mut refs: Vec<String> = Vec::new();
    for capture in pattern.captures_iter(message.unwrap_or_default()) {
        let reference = format!("fb-{}", &capture[1]);
        if !refs.contains(&reference) {
            refs.push(reference);
        }
    }
    refs
}

fn continuation(branch: Option<&str>, next: &Hash, limit: usize) -> String {
    let mut command = String::from("oak log --remote --json");
    if let Some(branch) = branch {
        command.push_str(&format!(
            " --branch {}",
            crate::commands::review::shell_quote_path(branch)
        ));
    }
    command.push_str(&format!(" --from {next} -n {limit}"));
    command
}

/// Walk and print the remote log as one JSON document.
pub async fn run_json(
    path: &Path,
    branch: Option<&str>,
    from: Option<&str>,
    limit: Option<usize>,
) -> Result<()> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(OakError::InvalidArgument(format!(
            "`oak log --remote` needs -n between 1 and {MAX_LIMIT}; continue longer walks with the returned next_command"
        )));
    }
    let from = from
        .map(|text| {
            Hash::from_hex(text).map_err(|_| {
                OakError::InvalidArgument(format!(
                    "--from needs a full commit hash (40 or 64 lowercase hex characters); got '{text}'"
                ))
            })
        })
        .transpose()?;
    let workspace = crate::commands::branch::RemoteWorkspace::open(path)?;
    let mut walker = Walker {
        repo: &workspace.repo,
        remote: &workspace.remote,
        client: crate::http::api_client(),
        // Each commits/info round trip is serial (the next first parent is
        // only known from the previous commit), so allow more wall time than
        // a review preparation while keeping the byte cap modest.
        budget: ReviewPreparationBudget::with_limits(
            64 * 1024 * 1024,
            std::time::Duration::from_secs(60),
        ),
        known: HashMap::new(),
    };

    let (head, head_source, branch_label) = match from {
        Some(hash) => (hash, "from_argument", branch.map(str::to_string)),
        None => {
            let name = branch.unwrap_or(oak_core::DEFAULT_BRANCH);
            let branches =
                crate::commands::branch::fetch_remote_branches(&workspace.remote).await?;
            walker.budget.record_branch_list_request();
            let listed = branches
                .iter()
                .find(|candidate| candidate.name == name)
                .ok_or_else(|| OakError::BranchNotFound(name.to_string()))?;
            let head = listed
                .head
                .as_deref()
                .map(Hash::from_hex)
                .transpose()?
                .ok_or_else(|| OakError::Server(format!("remote branch '{name}' has no head")))?;
            (head, "remote_branch_list", Some(name.to_string()))
        }
    };

    // The starting commit must be readable: an unknown head or a budget
    // miss before the first row is a typed failure (exit 6), never an empty
    // "truncated" page whose continuation would repeat this very command.
    if let Err(reason) = walker.acquire(std::slice::from_ref(&head)).await? {
        return Err(OakError::Server(format!(
            "remote log: {reason} ran out before commit {head} could be read; nothing was returned"
        )));
    }
    if !walker.known.contains_key(&head) {
        return Err(OakError::Server(format!(
            "remote log: commit {head} is not available on the remote (unknown, or not readable with this credential)"
        )));
    }

    let mut rows: Vec<RemoteLogCommitJson> = Vec::new();
    let mut cursor = Some(head.clone());
    let mut truncation: Option<&'static str> = None;
    let mut next: Option<Hash> = None;
    let mut complete = false;
    while let Some(hash) = cursor.clone() {
        if truncation.is_some() {
            next = Some(hash);
            break;
        }
        if rows.len() >= limit {
            truncation = Some("limit");
            next = Some(hash);
            break;
        }
        let Some(commit) = walker.known.get(&hash).cloned() else {
            // The server did not supply this parent. Re-running from it
            // would fail the same way, so there is no continuation.
            truncation = Some("commit_unavailable");
            break;
        };
        // One request carries the merge parent (for this row's source) and
        // the next first parent (only when another row is still wanted).
        let mut wanted: Vec<Hash> = commit.merge_parent_hash.iter().cloned().collect();
        if rows.len() + 1 < limit {
            wanted.extend(commit.parent_hash.iter().cloned());
        }
        let mut merge_source_unavailable = None;
        if let Err(reason) = walker.acquire(&wanted).await? {
            merge_source_unavailable = commit.merge_parent_hash.as_ref().map(|_| reason);
            truncation = Some(reason);
        }
        let merge_source_branch = commit
            .merge_parent_hash
            .as_ref()
            .and_then(|parent| walker.known.get(parent))
            .map(|parent| parent.branch_name.clone());
        if commit.merge_parent_hash.is_some()
            && merge_source_branch.is_none()
            && merge_source_unavailable.is_none()
        {
            merge_source_unavailable = Some("commit_unavailable");
        }
        rows.push(RemoteLogCommitJson {
            hash: commit.hash.to_string(),
            parent: commit.parent_hash.as_ref().map(ToString::to_string),
            merge_parent: commit.merge_parent_hash.as_ref().map(ToString::to_string),
            merge_source_branch,
            merge_source_unavailable,
            branch: commit.branch_name.clone(),
            timestamp: commit.timestamp.to_rfc3339(),
            author: commit.author.clone(),
            description_or_subject: output::commit_description_or_subject(&commit),
            feedback_refs: feedback_refs(commit.message.as_deref()),
            files_changed: commit.files.len(),
        });
        cursor = commit.parent_hash.clone();
        if cursor.is_none() {
            complete = true;
        }
    }

    let mut caveats = vec![
        "First-parent history; commits were verified against their hashes and nothing was written locally (refs, branch metadata and the object store are unchanged).".to_string(),
        "merge_source_branch is the branch_name recorded on the verified merge-parent commit; a closed or renamed branch keeps its recorded name.".to_string(),
    ];
    caveats.push(
        "In acquisition, objects_verified counts commit objects fetched and verified against their hashes; commits_reused were already in the local store."
            .to_string(),
    );
    if walker.budget.acquisition().commit_info_requests > 0 {
        caveats.push(
            "commits/info returns each commit's trees too; bytes_downloaded counts them."
                .to_string(),
        );
    }
    let next_command = next
        .as_ref()
        .filter(|next| **next != head)
        .map(|next| continuation(branch_label.as_deref(), next, limit));
    output::print_json(&RemoteLogJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        kind: "remote_log",
        branch: branch_label,
        head: head.to_string(),
        head_source,
        walk: "first_parent",
        limit,
        returned_count: rows.len(),
        complete,
        truncated: truncation.is_some(),
        truncation_reason: truncation,
        next_command,
        commits: rows,
        acquisition: walker.budget.acquisition(),
        caveats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_refs_are_unique_and_normalized() {
        assert_eq!(
            feedback_refs(Some(
                "Fixes fb-334, FB-410 and fb453; see fb-334 again. prefix-fb-1x"
            )),
            vec!["fb-334", "fb-410", "fb-453"]
        );
        assert!(feedback_refs(None).is_empty());
    }
}
