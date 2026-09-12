//! Durable local evidence for branch-head publications.
//!
//! This is deliberately not a replay journal. Records preserve what a client
//! attempted and what acknowledgement it observed so a later process can make
//! a read-only comparison without resending the mutation.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const DIRECTORY: &str = "publication-attempts";
const RECORD_SCHEMA_VERSION: u32 = 1;
const MAX_RECORDS: usize = 4096;
const MAX_RECORD_BYTES: u64 = 8 * 1024;
pub const DEFAULT_PAGE_LIMIT: usize = 50;
pub const MAX_PAGE_BYTES: usize = 48 * 1024;
const OBSERVATION_MAX_BYTES: usize = 64 * 1024;
const OBSERVATION_PAGE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    BeforeSend,
    SentUnconfirmed,
    Acknowledged,
    Rejected,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PublicationTransport {
    Ordinary,
    StagedFinal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "head", rename_all = "snake_case")]
pub enum ExpectedPredecessor {
    Absent,
    Exact(String),
    Unconstrained,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesiredBranchState {
    pub head: String,
    pub description: Option<String>,
    pub parent_branch: Option<String>,
    pub status: String,
    pub close_reason: Option<String>,
}

impl DesiredBranchState {
    pub fn digest(&self) -> Result<String> {
        let bytes =
            serde_json::to_vec(self).map_err(|error| OakError::Database(error.to_string()))?;
        Ok(hex_digest(&bytes))
    }
}

#[derive(Debug, Clone)]
pub struct NewAttempt<'a> {
    pub remote: &'a str,
    pub owner: &'a str,
    pub repo: &'a str,
    pub branch: &'a str,
    pub expected_predecessor: ExpectedPredecessor,
    pub target_head: &'a str,
    pub desired_branch: &'a DesiredBranchState,
    pub transport: PublicationTransport,
    /// Exact serialized request already held by the caller. It is hashed once
    /// in place; publication state never clones or stores it.
    pub request_body: &'a [u8],
    pub endpoint_path: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicationAttempt {
    pub schema_version: u32,
    pub operation_id: String,
    pub owner_pid: u32,
    pub operation_kind: String,
    pub transport: PublicationTransport,
    pub state: AttemptState,
    pub safe_remote: String,
    pub remote_fingerprint: String,
    pub owner: String,
    pub repo: String,
    pub branch: String,
    pub expected_predecessor: ExpectedPredecessor,
    pub target_head: String,
    pub desired_branch_state_digest: String,
    pub request_identity: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct AttemptStore {
    directory: PathBuf,
    max_records: usize,
}

#[derive(Debug)]
pub struct AttemptHandle {
    path: PathBuf,
    attempt: PublicationAttempt,
}

#[derive(Debug, Serialize)]
pub struct AttemptPage {
    pub pending_publication_count: usize,
    pub pending_publications: Vec<PublicationAttempt>,
    pub pending_publication_diagnostics: Vec<PublicationRecordDiagnostic>,
    pub pending_publications_has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_publications_next_after: Option<String>,
    pub publication_capacity: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublicationRecordDiagnostic {
    pub operation_id: String,
    pub problem: String,
    pub owner_liveness: &'static str,
    pub operator_action: &'static str,
}

#[derive(Debug, Serialize)]
pub struct PendingPublicationJson {
    #[serde(flatten)]
    pub attempt: PublicationAttempt,
    pub acknowledgement: &'static str,
    pub actor_causality: &'static str,
    pub observation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CurrentIdentity {
    pub remote: String,
    pub owner: String,
    pub repo: String,
    pub repository_token: Option<String>,
}

impl AttemptPage {
    pub async fn into_json_rows(
        self,
        refresh_requested: bool,
        current: Option<&CurrentIdentity>,
    ) -> Vec<PendingPublicationJson> {
        self.into_json_rows_with_timeout(refresh_requested, current, OBSERVATION_PAGE_TIMEOUT)
            .await
    }

    async fn into_json_rows_with_timeout(
        self,
        refresh_requested: bool,
        current: Option<&CurrentIdentity>,
        timeout: Duration,
    ) -> Vec<PendingPublicationJson> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rows = Vec::with_capacity(self.pending_publications.len());
        for attempt in self.pending_publications {
            let mut row = PendingPublicationJson {
                acknowledgement: match attempt.state {
                    AttemptState::Acknowledged => "validated_response",
                    AttemptState::Rejected => "explicit_rejection",
                    AttemptState::BeforeSend | AttemptState::SentUnconfirmed => "unconfirmed",
                },
                actor_causality: "unknown",
                observation: if refresh_requested {
                    "unavailable".to_string()
                } else {
                    "not_refreshed".to_string()
                },
                observed_head: None,
                observation_error: None,
                attempt,
            };
            if refresh_requested {
                observe(&mut row, current, deadline).await;
            }
            rows.push(row);
        }
        rows
    }
}

async fn observe(
    row: &mut PendingPublicationJson,
    current: Option<&CurrentIdentity>,
    deadline: tokio::time::Instant,
) {
    if let Err(problem) = validate_loaded_record(&row.attempt, Some(&row.attempt.operation_id)) {
        row.observation_error = Some(format!("record_{}", problem.code));
        return;
    }
    let token = match current.and_then(|current| {
        safe_remote_identity(&current.remote)
            .ok()
            .filter(|(safe_remote, fingerprint)| {
                safe_remote == &row.attempt.safe_remote
                    && fingerprint == &row.attempt.remote_fingerprint
                    && current.owner == row.attempt.owner
                    && current.repo == row.attempt.repo
            })
            .map(|_| {
                crate::commands::credentials::effective_token(
                    &row.attempt.safe_remote,
                    current.repository_token.clone(),
                )
            })
    }) {
        Some(token) => token,
        None => crate::commands::credentials::get_token_for_server(&row.attempt.safe_remote),
    };
    if token.is_none() {
        row.observation_error =
            Some("authorization_unavailable_for_recorded_destination".to_string());
        return;
    }

    let branch = crate::commands::branch_api_segment(&row.attempt.branch);
    let url = format!(
        "{}/api/{}/{}/branches/{branch}",
        row.attempt.safe_remote, row.attempt.owner, row.attempt.repo
    );
    let mut request = crate::http::api_client().get(url);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = match tokio::time::timeout_at(deadline, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => {
            row.observation_error = Some("network_unavailable".to_string());
            return;
        }
        Err(_) => {
            row.observation_error = Some("observation_page_deadline_exceeded".to_string());
            return;
        }
    };
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        row.observation = "absent_unknown".to_string();
        return;
    }
    if !response.status().is_success() {
        row.observation_error = Some(format!("http_{}", response.status().as_u16()));
        return;
    }
    if response
        .content_length()
        .is_some_and(|length| length > OBSERVATION_MAX_BYTES as u64)
    {
        row.observation_error = Some("response_oversized".to_string());
        return;
    }
    let body = match read_observation_body(response, deadline).await {
        Ok(body) => body,
        Err(problem) => {
            row.observation_error = Some(problem.to_string());
            return;
        }
    };
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            row.observation_error = Some("response_malformed".to_string());
            return;
        }
    };
    let Some(object) = value.as_object() else {
        row.observation_error = Some("response_malformed".to_string());
        return;
    };
    let head = match object.get("head") {
        Some(serde_json::Value::String(head)) if is_native_hash(head) => Some(head.clone()),
        Some(serde_json::Value::Null) => None,
        _ => {
            row.observation_error = Some("head_missing_or_invalid".to_string());
            return;
        }
    };
    row.observed_head = head.clone();
    let Some(head) = head else {
        row.observation = "absent_unknown".to_string();
        return;
    };
    if head != row.attempt.target_head {
        row.observation = match &row.attempt.expected_predecessor {
            ExpectedPredecessor::Exact(expected) if expected == &head => "predecessor_current",
            _ => "superseded_unknown",
        }
        .to_string();
        return;
    }

    // The live branch-detail producer omits `close_reason` when it is `None`.
    // The other fields are always present, including explicit nulls, so their
    // absence remains insufficient evidence rather than a defaulted match.
    let required = ["name", "description", "parent_branch", "status"];
    if required.iter().any(|field| !object.contains_key(*field)) {
        row.observation = "target_head_current_insufficient_branch_fields".to_string();
        return;
    }
    if object.get("name").and_then(serde_json::Value::as_str) != Some(row.attempt.branch.as_str()) {
        row.observation_error = Some("branch_identity_mismatch".to_string());
        return;
    }
    let decoded = DesiredBranchState {
        head,
        description: match decode_optional_string(object.get("description")) {
            Some(value) => value,
            None => {
                row.observation_error = Some("branch_fields_malformed".to_string());
                return;
            }
        },
        parent_branch: match decode_optional_string(object.get("parent_branch")) {
            Some(value) => value,
            None => {
                row.observation_error = Some("branch_fields_malformed".to_string());
                return;
            }
        },
        status: match object.get("status").and_then(serde_json::Value::as_str) {
            Some(value) => value.to_string(),
            None => {
                row.observation_error = Some("branch_fields_malformed".to_string());
                return;
            }
        },
        close_reason: match object.get("close_reason") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(value)) => Some(value.clone()),
            Some(_) => {
                row.observation_error = Some("branch_fields_malformed".to_string());
                return;
            }
        },
    };
    row.observation = match decoded.digest() {
        Ok(digest) if digest == row.attempt.desired_branch_state_digest => {
            "current_state_satisfied"
        }
        Ok(_) => "target_head_current_metadata_mismatch",
        Err(_) => "insufficient_evidence",
    }
    .to_string();
}

async fn read_observation_body(
    mut response: reqwest::Response,
    deadline: tokio::time::Instant,
) -> std::result::Result<Vec<u8>, &'static str> {
    let mut body = Vec::new();
    loop {
        let chunk = tokio::time::timeout_at(deadline, response.chunk())
            .await
            .map_err(|_| "observation_page_deadline_exceeded")?
            .map_err(|_| "response_unavailable")?;
        let Some(chunk) = chunk else { break };
        if chunk.len() > OBSERVATION_MAX_BYTES.saturating_sub(body.len()) {
            return Err("response_oversized");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn decode_optional_string(value: Option<&serde_json::Value>) -> Option<Option<String>> {
    match value {
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(value)) => Some(Some(value.clone())),
        _ => None,
    }
}

fn is_native_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

impl AttemptStore {
    pub fn checkout(oak_dir: &Path) -> Self {
        Self::new(oak_dir.join(DIRECTORY))
    }

    pub fn mount(state_dir: &Path) -> Self {
        Self::new(state_dir.join(DIRECTORY))
    }

    fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            max_records: MAX_RECORDS,
        }
    }

    #[cfg(test)]
    fn with_limit(directory: PathBuf, max_records: usize) -> Self {
        Self {
            directory,
            max_records,
        }
    }

    pub fn reserve(&self, new: NewAttempt<'_>) -> Result<AttemptHandle> {
        let directory_was_missing = !self.directory.is_dir();
        fs::create_dir_all(&self.directory)?;
        if directory_was_missing {
            if let Some(parent) = self.directory.parent() {
                sync_dir(parent)?;
            }
            sync_dir(&self.directory)?;
        }
        let _allocation_lock = crate::workdir_lock::WorkdirLock::acquire_wait(
            &self.directory,
            Duration::from_secs(2),
        )?;
        if self.record_paths()?.len() >= self.max_records {
            return Err(OakError::InvalidArgument(format!(
                "publication_state_full: this repository has {} unresolved or retained publication records; inspect `oak agent state --json` and explicitly acknowledge an exact operation before publishing again",
                self.max_records
            )));
        }

        for _ in 0..8 {
            let operation_id = uuid::Uuid::new_v4().to_string();
            let path = self.directory.join(format!("{operation_id}.json"));
            let now = chrono::Utc::now().to_rfc3339();
            let (safe_remote, remote_fingerprint) = safe_remote_identity(new.remote)?;
            let request_identity = request_identity(new.endpoint_path, new.request_body);
            let attempt = PublicationAttempt {
                schema_version: RECORD_SCHEMA_VERSION,
                operation_id,
                owner_pid: std::process::id(),
                operation_kind: "branch_head_push".to_string(),
                transport: new.transport,
                state: AttemptState::BeforeSend,
                safe_remote,
                remote_fingerprint,
                owner: new.owner.to_string(),
                repo: new.repo.to_string(),
                branch: new.branch.to_string(),
                expected_predecessor: new.expected_predecessor.clone(),
                target_head: new.target_head.to_string(),
                desired_branch_state_digest: new.desired_branch.digest()?,
                request_identity,
                created_at: now.clone(),
                updated_at: now,
            };
            let bytes = encode_record(&attempt)?;
            match crate::atomic_file::write_atomic_private_noclobber(&path, |file| {
                use std::io::Write;
                file.write_all(&bytes)?;
                Ok(())
            }) {
                Ok(()) => return Ok(AttemptHandle { path, attempt }),
                Err(OakError::Io(error)) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(OakError::InvalidArgument(
            "could not reserve a unique publication operation id; no remote state was mutated"
                .to_string(),
        ))
    }

    pub fn page(&self, after: Option<&str>, limit: usize) -> Result<AttemptPage> {
        if let Some(after) = after {
            validate_operation_id(after)?;
        }
        let mut paths = self.record_paths()?;
        paths.sort();
        let total = paths.len();
        let bounded_limit = limit.clamp(1, DEFAULT_PAGE_LIMIT);
        let mut records = Vec::new();
        let mut diagnostics = Vec::new();
        let mut page_bytes = 0usize;
        let mut entries_added = 0usize;
        let mut last_id = None;
        for path in &paths {
            let Some(id) = record_id(path) else { continue };
            if after.is_some_and(|after| id.as_str() <= after) {
                continue;
            }
            if entries_added == bounded_limit {
                break;
            }
            let entry = match load_record_checked(path, Some(&id)) {
                Ok(record) => PageEntry::Record(Box::new(record)),
                Err(problem) => PageEntry::Diagnostic(PublicationRecordDiagnostic {
                    operation_id: id.clone(),
                    problem: problem.code.to_string(),
                    owner_liveness: "unknown",
                    operator_action: "preserve this record and inspect or restore it manually; Oak will not remove it automatically",
                }),
            };
            let entry_bytes = serde_json::to_vec(&entry)
                .map_err(|error| OakError::Database(error.to_string()))?
                .len();
            if entries_added > 0 && page_bytes.saturating_add(entry_bytes) > MAX_PAGE_BYTES {
                break;
            }
            page_bytes = page_bytes.saturating_add(entry_bytes);
            match entry {
                PageEntry::Record(record) => records.push(*record),
                PageEntry::Diagnostic(diagnostic) => diagnostics.push(diagnostic),
            }
            entries_added += 1;
            last_id = Some(id);
        }
        let has_more = last_id.as_ref().is_some_and(|last| {
            paths
                .iter()
                .any(|path| record_id(path).is_some_and(|id| id > *last))
        });
        let next_after = has_more.then_some(last_id).flatten();
        Ok(AttemptPage {
            pending_publication_count: total,
            pending_publications: records,
            pending_publication_diagnostics: diagnostics,
            pending_publications_has_more: has_more,
            pending_publications_next_after: next_after,
            publication_capacity: self.max_records,
        })
    }

    pub fn forget(&self, operation_id: &str) -> Result<PublicationAttempt> {
        validate_operation_id(operation_id)?;
        let _allocation_lock = crate::workdir_lock::WorkdirLock::acquire_wait(
            &self.directory,
            Duration::from_secs(2),
        )?;
        let path = self.directory.join(format!("{operation_id}.json"));
        let record = load_record(&path, Some(operation_id))?;
        if record.operation_id != operation_id {
            return Err(OakError::InvalidArgument(
                "publication record identity mismatch; refusing to remove it".to_string(),
            ));
        }
        if crate::commands::mount::state::pid_alive(record.owner_pid) {
            return Err(OakError::InvalidArgument(format!(
                "publication operation {operation_id} is still owned by a live process; refusing to forget it"
            )));
        }
        fs::remove_file(path)?;
        sync_dir(&self.directory)?;
        Ok(record)
    }

    pub fn has_records(&self) -> Result<bool> {
        Ok(!self.record_paths()?.is_empty())
    }

    fn record_paths(&self) -> Result<Vec<PathBuf>> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if record_id(&path).is_some() {
                paths.push(path);
            }
        }
        Ok(paths)
    }
}

/// Locate the durable adapter that owns `work_tree`. Mount identity wins over
/// an enclosing checkout so mount teardown cannot erase the only record.
pub fn store_for_work_tree(work_tree: &Path) -> Result<AttemptStore> {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    if let Some(dest) = crate::commands::mount::mount_dest_for(work_tree)? {
        let (_, state_dir) = crate::commands::mount::config_for_dest(&dest)?;
        return Ok(AttemptStore::mount(&state_dir));
    }
    let ctx = crate::resolve::resolve(work_tree)?;
    Ok(AttemptStore::checkout(&ctx.oak_dir))
}

/// Persist the existing positive pushed-head receipt in the same adapter as
/// the attempt. Only after this returns may the attempt be retired.
pub fn persist_downstream_receipt(
    work_tree: &Path,
    remote: &str,
    owner: &str,
    repo: &str,
    branch: &str,
    head: &str,
) -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    if let Some(dest) = crate::commands::mount::mount_dest_for(work_tree)? {
        let (_, state_dir) = crate::commands::mount::config_for_dest(&dest)?;
        return crate::commands::mount::state::save_pushed_head(&state_dir, head);
    }
    let ctx = crate::resolve::resolve(work_tree)?;
    crate::work_state::record_checkout_push_success(&ctx.oak_dir, remote, owner, repo, branch, head)
}

impl AttemptHandle {
    pub fn operation_id(&self) -> &str {
        &self.attempt.operation_id
    }

    pub fn mark_sent(&mut self) -> Result<()> {
        self.transition(AttemptState::BeforeSend, AttemptState::SentUnconfirmed)
    }

    pub fn mark_acknowledged(&mut self) -> Result<()> {
        self.transition(AttemptState::SentUnconfirmed, AttemptState::Acknowledged)
    }

    pub fn mark_rejected(&mut self) -> Result<()> {
        self.transition(AttemptState::SentUnconfirmed, AttemptState::Rejected)
    }

    pub fn retire(self) -> Result<()> {
        if !matches!(
            self.attempt.state,
            AttemptState::Acknowledged | AttemptState::Rejected
        ) {
            return Err(OakError::InvalidArgument(format!(
                "publication operation {} remains unresolved and cannot be retired",
                self.attempt.operation_id
            )));
        }
        let on_disk = load_record(&self.path, Some(&self.attempt.operation_id))?;
        if on_disk.operation_id != self.attempt.operation_id || on_disk.state != self.attempt.state
        {
            return Err(OakError::InvalidArgument(
                "publication record changed unexpectedly; refusing to retire it".to_string(),
            ));
        }
        fs::remove_file(&self.path)?;
        if let Some(parent) = self.path.parent() {
            sync_dir(parent)?;
        }
        Ok(())
    }

    fn transition(&mut self, from: AttemptState, to: AttemptState) -> Result<()> {
        if self.attempt.state != from {
            return Err(OakError::InvalidArgument(format!(
                "invalid publication state transition from {:?} to {:?}",
                self.attempt.state, to
            )));
        }
        let on_disk = load_record(&self.path, Some(&self.attempt.operation_id))?;
        if on_disk.operation_id != self.attempt.operation_id
            || on_disk.owner_pid != self.attempt.owner_pid
            || on_disk.state != from
        {
            return Err(OakError::InvalidArgument(
                "publication record changed unexpectedly; refusing to overwrite it".to_string(),
            ));
        }
        self.attempt.state = to;
        self.attempt.updated_at = chrono::Utc::now().to_rfc3339();
        crate::atomic_file::write_atomic_private(&self.path, encode_record(&self.attempt)?)
    }
}

fn encode_record(record: &PublicationAttempt) -> Result<Vec<u8>> {
    let bytes =
        serde_json::to_vec(record).map_err(|error| OakError::Database(error.to_string()))?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(OakError::InvalidArgument(
            "publication record exceeded its private local size bound; no remote state was mutated"
                .to_string(),
        ));
    }
    Ok(bytes)
}

#[derive(Debug, Serialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
enum PageEntry {
    Record(Box<PublicationAttempt>),
    Diagnostic(PublicationRecordDiagnostic),
}

#[derive(Debug)]
struct RecordProblem {
    code: &'static str,
}

fn load_record(path: &Path, expected_id: Option<&str>) -> Result<PublicationAttempt> {
    load_record_checked(path, expected_id).map_err(|problem| {
        OakError::InvalidArgument(format!(
            "publication record {} is {}; refusing to modify it",
            path.display(),
            problem.code
        ))
    })
}

fn load_record_checked(
    path: &Path,
    expected_id: Option<&str>,
) -> std::result::Result<PublicationAttempt, RecordProblem> {
    let metadata = fs::symlink_metadata(path).map_err(|_| RecordProblem { code: "unreadable" })?;
    if !metadata.file_type().is_file() {
        return Err(RecordProblem {
            code: "not_regular_file",
        });
    }
    let file = fs::File::open(path).map_err(|_| RecordProblem { code: "unreadable" })?;
    let mut bytes = Vec::with_capacity((metadata.len().min(MAX_RECORD_BYTES) + 1) as usize);
    use std::io::Read;
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| RecordProblem { code: "unreadable" })?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(RecordProblem { code: "oversized" });
    }
    let record: PublicationAttempt =
        serde_json::from_slice(&bytes).map_err(|_| RecordProblem { code: "malformed" })?;
    if record.schema_version != RECORD_SCHEMA_VERSION {
        return Err(RecordProblem {
            code: "unknown_schema",
        });
    }
    validate_loaded_record(&record, expected_id)?;
    Ok(record)
}

fn validate_loaded_record(
    record: &PublicationAttempt,
    expected_id: Option<&str>,
) -> std::result::Result<(), RecordProblem> {
    if validate_operation_id(&record.operation_id).is_err()
        || expected_id.is_some_and(|expected| expected != record.operation_id)
    {
        return Err(RecordProblem {
            code: "identity_mismatch",
        });
    }
    let (canonical_remote, fingerprint) =
        safe_remote_identity(&record.safe_remote).map_err(|_| RecordProblem {
            code: "remote_invalid",
        })?;
    if canonical_remote != record.safe_remote || fingerprint != record.remote_fingerprint {
        return Err(RecordProblem {
            code: "remote_identity_mismatch",
        });
    }
    if record.owner_pid == 0
        || record.operation_kind != "branch_head_push"
        || crate::commands::parse_owner_repo(&format!("{}/{}", record.owner, record.repo)).is_err()
        || (record.branch != oak_core::DEFAULT_BRANCH
            && crate::commands::branch::validate_branch_name(&record.branch).is_err())
        || !is_native_hash(&record.target_head)
        || !is_native_hash(&record.desired_branch_state_digest)
        || !is_native_hash(&record.request_identity)
        || matches!(&record.expected_predecessor, ExpectedPredecessor::Exact(head) if !is_native_hash(head))
        || chrono::DateTime::parse_from_rfc3339(&record.created_at).is_err()
        || chrono::DateTime::parse_from_rfc3339(&record.updated_at).is_err()
    {
        return Err(RecordProblem {
            code: "invalid_fields",
        });
    }
    Ok(())
}

fn record_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let id = name.strip_suffix(".json")?;
    uuid::Uuid::parse_str(id)
        .ok()
        .map(|value| value.to_string())
}

fn validate_operation_id(value: &str) -> Result<()> {
    let parsed = uuid::Uuid::parse_str(value).map_err(|_| {
        OakError::InvalidArgument("publication operation id must be a UUID".to_string())
    })?;
    if parsed.to_string() != value {
        return Err(OakError::InvalidArgument(
            "publication operation id must use canonical lowercase UUID form".to_string(),
        ));
    }
    Ok(())
}

fn safe_remote_identity(remote: &str) -> Result<(String, String)> {
    let mut url = reqwest::Url::parse(remote).map_err(|_| {
        OakError::InvalidArgument(
            "cannot persist publication state for an invalid remote URL".to_string(),
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(OakError::InvalidArgument(
            "cannot persist publication state for a non-HTTP remote URL".to_string(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(OakError::InvalidArgument(
            "cannot persist publication state for a remote URL with query or fragment; no remote state was mutated"
                .to_string(),
        ));
    }
    url.set_username("").map_err(|_| {
        OakError::InvalidArgument("cannot redact publication remote URL".to_string())
    })?;
    url.set_password(None).map_err(|_| {
        OakError::InvalidArgument("cannot redact publication remote URL".to_string())
    })?;
    let safe = url.as_str().trim_end_matches('/').to_string();
    Ok((safe.clone(), hex_digest(safe.as_bytes())))
}

fn request_identity(endpoint_path: &str, body: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"POST\0");
    digest.update(endpoint_path.as_bytes());
    digest.update(b"\0");
    digest.update(body);
    format!("{:x}", digest.finalize())
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired() -> DesiredBranchState {
        DesiredBranchState {
            head: "a".repeat(64),
            description: Some("safe description".to_string()),
            parent_branch: Some("main".to_string()),
            status: "open".to_string(),
            close_reason: None,
        }
    }

    fn reserve(store: &AttemptStore) -> AttemptHandle {
        let desired = desired();
        let target = "a".repeat(64);
        store
            .reserve(NewAttempt {
                remote: "https://user:secret@example.com/base",
                owner: "owner",
                repo: "repo",
                branch: "topic",
                expected_predecessor: ExpectedPredecessor::Exact("b".repeat(64)),
                target_head: &target,
                desired_branch: &desired,
                transport: PublicationTransport::Ordinary,
                request_body: br#"{"large":"body"}"#,
                endpoint_path: "/api/owner/repo/push",
            })
            .unwrap()
    }

    #[test]
    fn durable_attempt_survives_restart_without_implying_acknowledgement() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        let mut handle = reserve(&store);
        let operation_id = handle.operation_id().to_string();
        handle.mark_sent().unwrap();
        drop(handle);

        let page = AttemptStore::checkout(dir.path()).page(None, 50).unwrap();
        assert_eq!(page.pending_publication_count, 1);
        assert_eq!(page.pending_publications[0].operation_id, operation_id);
        assert_eq!(
            page.pending_publications[0].state,
            AttemptState::SentUnconfirmed
        );
        assert_eq!(
            page.pending_publications[0].safe_remote,
            "https://example.com/base"
        );
        let raw = fs::read_to_string(
            dir.path()
                .join(DIRECTORY)
                .join(format!("{operation_id}.json")),
        )
        .unwrap();
        assert!(!raw.contains("secret"));
        assert!(!raw.contains("large"));
    }

    #[test]
    fn separate_records_do_not_clobber_and_page_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        let first = reserve(&store);
        let second = reserve(&store);
        assert_ne!(first.operation_id(), second.operation_id());
        let page = store.page(None, 1).unwrap();
        assert_eq!(page.pending_publication_count, 2);
        assert_eq!(page.pending_publications.len(), 1);
        assert!(page.pending_publications_has_more);
        assert!(page.pending_publications_next_after.is_some());
    }

    #[test]
    fn default_page_has_a_byte_bound_and_executable_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        for _ in 0..60 {
            let _attempt = reserve(&store);
        }
        let page = store.page(None, DEFAULT_PAGE_LIMIT).unwrap();
        assert_eq!(page.pending_publication_count, 60);
        assert!(page.pending_publications.len() <= DEFAULT_PAGE_LIMIT);
        assert!(
            serde_json::to_vec(&page.pending_publications)
                .unwrap()
                .len()
                <= MAX_PAGE_BYTES
        );
        let cursor = page.pending_publications_next_after.unwrap();
        assert_eq!(uuid::Uuid::parse_str(&cursor).unwrap().to_string(), cursor);
        assert!(!store
            .page(Some(&cursor), DEFAULT_PAGE_LIMIT)
            .unwrap()
            .pending_publications
            .is_empty());
        assert!(store.page(Some("../not-an-operation"), 50).is_err());
    }

    #[test]
    fn capacity_refuses_before_another_record_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::with_limit(dir.path().join(DIRECTORY), 1);
        let _first = reserve(&store);
        let desired = desired();
        let target = "a".repeat(64);
        let error = store
            .reserve(NewAttempt {
                remote: "https://example.com",
                owner: "owner",
                repo: "repo",
                branch: "topic",
                expected_predecessor: ExpectedPredecessor::Absent,
                target_head: &target,
                desired_branch: &desired,
                transport: PublicationTransport::Ordinary,
                request_body: b"{}",
                endpoint_path: "/api/owner/repo/push",
            })
            .unwrap_err();
        assert!(error.to_string().contains("publication_state_full"));
        assert_eq!(store.page(None, 50).unwrap().pending_publication_count, 1);
    }

    #[test]
    fn malformed_record_is_counted_without_hiding_valid_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        let valid = reserve(&store);
        let malformed_id = uuid::Uuid::new_v4().to_string();
        fs::write(
            dir.path()
                .join(DIRECTORY)
                .join(format!("{malformed_id}.json")),
            b"{not-json",
        )
        .unwrap();
        let unknown_id = uuid::Uuid::new_v4().to_string();
        let mut unknown = valid.attempt.clone();
        unknown.operation_id = unknown_id.clone();
        unknown.schema_version = RECORD_SCHEMA_VERSION + 1;
        fs::write(
            dir.path()
                .join(DIRECTORY)
                .join(format!("{unknown_id}.json")),
            serde_json::to_vec(&unknown).unwrap(),
        )
        .unwrap();
        let oversized_id = uuid::Uuid::new_v4().to_string();
        fs::write(
            dir.path()
                .join(DIRECTORY)
                .join(format!("{oversized_id}.json")),
            vec![b'x'; MAX_RECORD_BYTES as usize + 1],
        )
        .unwrap();
        let mismatched_id = uuid::Uuid::new_v4().to_string();
        fs::write(
            dir.path()
                .join(DIRECTORY)
                .join(format!("{mismatched_id}.json")),
            serde_json::to_vec(&valid.attempt).unwrap(),
        )
        .unwrap();

        let page = store.page(None, 50).unwrap();
        assert_eq!(page.pending_publication_count, 5);
        assert_eq!(page.pending_publications.len(), 1);
        assert_eq!(
            page.pending_publications[0].operation_id,
            valid.operation_id()
        );
        assert_eq!(page.pending_publication_diagnostics.len(), 4);
        let diagnostics: std::collections::HashMap<_, _> = page
            .pending_publication_diagnostics
            .iter()
            .map(|row| (row.operation_id.as_str(), row.problem.as_str()))
            .collect();
        assert_eq!(diagnostics[malformed_id.as_str()], "malformed");
        assert_eq!(diagnostics[unknown_id.as_str()], "unknown_schema");
        assert_eq!(diagnostics[oversized_id.as_str()], "oversized");
        assert_eq!(diagnostics[mismatched_id.as_str()], "identity_mismatch");
        assert!(store.has_records().unwrap());
        assert!(store.forget(&malformed_id).is_err());
    }

    #[test]
    fn live_owner_cannot_forget_unconfirmed_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        let mut handle = reserve(&store);
        handle.mark_sent().unwrap();
        let error = store.forget(handle.operation_id()).unwrap_err();
        assert!(error.to_string().contains("live process"));
    }

    #[test]
    fn allocator_does_not_reclaim_acknowledgement_before_downstream_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        let mut first = reserve(&store);
        let first_id = first.operation_id().to_string();
        first.mark_sent().unwrap();
        first.mark_acknowledged().unwrap();

        let second = reserve(&store);
        assert_ne!(second.operation_id(), first_id);
        let page = store.page(None, 50).unwrap();
        assert_eq!(page.pending_publication_count, 2);
        assert!(page.pending_publications.iter().any(|row| {
            row.operation_id == first_id && row.state == AttemptState::Acknowledged
        }));
        let error = store.forget(&first_id).unwrap_err();
        assert!(error.to_string().contains("live process"));
    }

    #[test]
    fn cross_process_reservations_do_not_clobber() {
        const CHILD: &str = "OAK_PUBLICATION_STATE_RESERVE_CHILD";
        const ROOT: &str = "OAK_PUBLICATION_STATE_RESERVE_ROOT";
        if std::env::var_os(CHILD).is_some() {
            let directory = PathBuf::from(std::env::var_os(ROOT).unwrap());
            let store = AttemptStore::checkout(&directory);
            let _attempt = reserve(&store);
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for _ in 0..2 {
            children.push(
                std::process::Command::new(&executable)
                    .args([
                        "--exact",
                        "publication_state::tests::cross_process_reservations_do_not_clobber",
                    ])
                    .env(CHILD, "1")
                    .env(ROOT, dir.path())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            );
        }
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        let page = AttemptStore::checkout(dir.path()).page(None, 50).unwrap();
        assert_eq!(page.pending_publication_count, 2);
        assert_ne!(
            page.pending_publications[0].operation_id,
            page.pending_publications[1].operation_id
        );
    }

    #[test]
    fn remote_query_is_rejected_instead_of_silently_retargeted() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        let desired = desired();
        let target = "a".repeat(64);
        let error = store
            .reserve(NewAttempt {
                remote: "https://example.com/base?token=secret",
                owner: "owner",
                repo: "repo",
                branch: "topic",
                expected_predecessor: ExpectedPredecessor::Absent,
                target_head: &target,
                desired_branch: &desired,
                transport: PublicationTransport::Ordinary,
                request_body: b"{}",
                endpoint_path: "/api/owner/repo/push",
            })
            .unwrap_err();
        assert!(error.to_string().contains("query or fragment"));
        assert!(!error.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn refresh_separates_current_state_from_acknowledgement_and_requires_fields() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::builder().start().await;
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        let desired = desired();
        let target = desired.head.clone();
        let mut handle = store
            .reserve(NewAttempt {
                remote: &server.uri(),
                owner: "owner",
                repo: "repo",
                branch: "topic",
                expected_predecessor: ExpectedPredecessor::Absent,
                target_head: &target,
                desired_branch: &desired,
                transport: PublicationTransport::Ordinary,
                request_body: b"{}",
                endpoint_path: "owner/repo",
            })
            .unwrap();
        handle.mark_sent().unwrap();
        Mock::given(method("GET"))
            .and(path("/api/owner/repo/branches/topic"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "topic",
                "head": target,
                "description": "safe description",
                "parent_branch": "main",
                "status": "open"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let current = CurrentIdentity {
            remote: server.uri(),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_token: Some("test-token".to_string()),
        };
        let rows = store
            .page(None, 50)
            .unwrap()
            .into_json_rows(true, Some(&current))
            .await;
        assert_eq!(rows[0].observation, "current_state_satisfied");
        assert_eq!(rows[0].acknowledgement, "unconfirmed");
        assert_eq!(rows[0].actor_causality, "unknown");
    }

    #[tokio::test]
    async fn refresh_does_not_default_missing_branch_fields_into_a_match() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::builder().start().await;
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        let desired = desired();
        let target = desired.head.clone();
        let mut handle = store
            .reserve(NewAttempt {
                remote: &server.uri(),
                owner: "owner",
                repo: "repo",
                branch: "topic",
                expected_predecessor: ExpectedPredecessor::Absent,
                target_head: &target,
                desired_branch: &desired,
                transport: PublicationTransport::Ordinary,
                request_body: b"{}",
                endpoint_path: "owner/repo",
            })
            .unwrap();
        handle.mark_sent().unwrap();
        Mock::given(method("GET"))
            .and(path("/api/owner/repo/branches/topic"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "head": target
            })))
            .expect(1)
            .mount(&server)
            .await;
        let current = CurrentIdentity {
            remote: server.uri(),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_token: Some("test-token".to_string()),
        };
        let rows = store
            .page(None, 50)
            .unwrap()
            .into_json_rows(true, Some(&current))
            .await;
        assert_eq!(
            rows[0].observation,
            "target_head_current_insufficient_branch_fields"
        );
        assert_eq!(rows[0].acknowledgement, "unconfirmed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_streams_to_the_actual_size_cap_without_content_length() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            for bytes in [vec![b'a'; 48 * 1024], vec![b'b'; 32 * 1024]] {
                socket
                    .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
                    .await
                    .unwrap();
                if socket.write_all(&bytes).await.is_err() {
                    return;
                }
                if socket.write_all(b"\r\n").await.is_err() {
                    return;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        let desired = desired();
        let target = desired.head.clone();
        let mut handle = store
            .reserve(NewAttempt {
                remote: &remote,
                owner: "owner",
                repo: "repo",
                branch: "topic",
                expected_predecessor: ExpectedPredecessor::Absent,
                target_head: &target,
                desired_branch: &desired,
                transport: PublicationTransport::Ordinary,
                request_body: b"{}",
                endpoint_path: "/api/owner/repo/push",
            })
            .unwrap();
        handle.mark_sent().unwrap();
        let current = CurrentIdentity {
            remote,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_token: Some("test-token".to_string()),
        };
        let rows = store
            .page(None, 50)
            .unwrap()
            .into_json_rows_with_timeout(true, Some(&current), Duration::from_secs(2))
            .await;
        assert_eq!(
            rows[0].observation_error.as_deref(),
            Some("response_oversized")
        );
        server.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_deadline_includes_waiting_for_response_headers() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            std::future::pending::<()>().await;
        });
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        let desired = desired();
        let target = desired.head.clone();
        let mut handle = store
            .reserve(NewAttempt {
                remote: &remote,
                owner: "owner",
                repo: "repo",
                branch: "topic",
                expected_predecessor: ExpectedPredecessor::Absent,
                target_head: &target,
                desired_branch: &desired,
                transport: PublicationTransport::Ordinary,
                request_body: b"{}",
                endpoint_path: "/api/owner/repo/push",
            })
            .unwrap();
        handle.mark_sent().unwrap();
        let current = CurrentIdentity {
            remote,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_token: Some("test-token".to_string()),
        };
        let rows = store
            .page(None, 50)
            .unwrap()
            .into_json_rows_with_timeout(true, Some(&current), Duration::from_millis(50))
            .await;
        assert_eq!(
            rows[0].observation_error.as_deref(),
            Some("observation_page_deadline_exceeded")
        );
        server.abort();
    }

    #[tokio::test]
    async fn refresh_distinguishes_predecessor_from_unrelated_superseding_head() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::builder().start().await;
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        let desired = desired();
        for (branch, current_head) in [
            ("predecessor", "b".repeat(64)),
            ("superseded", "c".repeat(64)),
        ] {
            let target = desired.head.clone();
            let mut handle = store
                .reserve(NewAttempt {
                    remote: &server.uri(),
                    owner: "owner",
                    repo: "repo",
                    branch,
                    expected_predecessor: ExpectedPredecessor::Exact("b".repeat(64)),
                    target_head: &target,
                    desired_branch: &desired,
                    transport: PublicationTransport::Ordinary,
                    request_body: b"{}",
                    endpoint_path: "/api/owner/repo/push",
                })
                .unwrap();
            handle.mark_sent().unwrap();
            Mock::given(method("GET"))
                .and(path(format!("/api/owner/repo/branches/{branch}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "head": current_head
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
        let current = CurrentIdentity {
            remote: server.uri(),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_token: Some("test-token".to_string()),
        };
        let rows = store
            .page(None, 50)
            .unwrap()
            .into_json_rows(true, Some(&current))
            .await;
        let observations: std::collections::HashMap<_, _> = rows
            .iter()
            .map(|row| (row.attempt.branch.as_str(), row.observation.as_str()))
            .collect();
        assert_eq!(observations["predecessor"], "predecessor_current");
        assert_eq!(observations["superseded"], "superseded_unknown");
    }

    #[tokio::test]
    async fn mismatched_checkout_identity_does_not_send_its_repository_token() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::builder().start().await;
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        let desired = desired();
        let target = desired.head.clone();
        let mut handle = store
            .reserve(NewAttempt {
                remote: &server.uri(),
                owner: "recorded-owner",
                repo: "recorded-repo",
                branch: "topic",
                expected_predecessor: ExpectedPredecessor::Absent,
                target_head: &target,
                desired_branch: &desired,
                transport: PublicationTransport::Ordinary,
                request_body: b"{}",
                endpoint_path: "recorded-owner/recorded-repo",
            })
            .unwrap();
        handle.mark_sent().unwrap();
        Mock::given(method("GET"))
            .and(path("/api/recorded-owner/recorded-repo/branches/topic"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let current = CurrentIdentity {
            remote: server.uri(),
            owner: "different-owner".to_string(),
            repo: "different-repo".to_string(),
            repository_token: Some("must-not-leak".to_string()),
        };
        let rows = store
            .page(None, 50)
            .unwrap()
            .into_json_rows(true, Some(&current))
            .await;
        assert_eq!(rows[0].observation, "unavailable");
        assert_eq!(
            rows[0].observation_error.as_deref(),
            Some("authorization_unavailable_for_recorded_destination")
        );
    }

    #[tokio::test]
    async fn tampered_recorded_destination_is_rejected_before_credentials_or_network() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let recorded = MockServer::builder().start().await;
        let attacker = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&attacker)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let store = AttemptStore::checkout(dir.path());
        let desired = desired();
        let target = desired.head.clone();
        let mut handle = store
            .reserve(NewAttempt {
                remote: &recorded.uri(),
                owner: "owner",
                repo: "repo",
                branch: "topic",
                expected_predecessor: ExpectedPredecessor::Absent,
                target_head: &target,
                desired_branch: &desired,
                transport: PublicationTransport::Ordinary,
                request_body: b"{}",
                endpoint_path: "/api/owner/repo/push",
            })
            .unwrap();
        handle.mark_sent().unwrap();
        let operation_id = handle.operation_id().to_string();
        drop(handle);
        let path = dir
            .path()
            .join(DIRECTORY)
            .join(format!("{operation_id}.json"));
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["safe_remote"] = serde_json::Value::String(attacker.uri());
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let current = CurrentIdentity {
            remote: recorded.uri(),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            repository_token: Some("must-not-leak".to_string()),
        };
        let page = store.page(None, 50).unwrap();
        assert!(page.pending_publications.is_empty());
        assert_eq!(page.pending_publication_diagnostics.len(), 1);
        assert_eq!(
            page.pending_publication_diagnostics[0].problem,
            "remote_identity_mismatch"
        );
        let rows = page.into_json_rows(true, Some(&current)).await;
        assert!(rows.is_empty());
    }
}
