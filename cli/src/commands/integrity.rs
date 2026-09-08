//! Remote content-integrity diagnostics and clone preflight.
//!
//! The server owns the reachability proof. This module is the single client
//! Adapter for its append-only JSON Interface, keeping doctor, blob inspection,
//! and clone from drifting into subtly different definitions of "complete".

use oak_core::{Hash, OakError, Result};
use serde::{Deserialize, Serialize};

use crate::output;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    Metadata,
    Existence,
    Bytes,
}

impl Verification {
    fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Existence => "existence",
            Self::Bytes => "bytes",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct IntegrityLocation {
    pub commit: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct IntegrityFinding {
    pub code: String,
    #[serde(default)]
    pub commit_hash: Option<String>,
    #[serde(default)]
    pub tree_hash: Option<String>,
    #[serde(default)]
    pub manifest_hash: Option<String>,
    #[serde(default)]
    pub blob_hash: Option<String>,
    #[serde(default)]
    pub chunk_hash: Option<String>,
    #[serde(default)]
    pub affected: Vec<IntegrityLocation>,
    pub recoverability: String,
    pub detail: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct IntegrityAdvisory {
    pub code: String,
    pub tree_hash: String,
    pub detail: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct IntegrityScope {
    #[serde(default)]
    pub depth: Option<u32>,
    pub commit_count: usize,
    pub manifest_count: usize,
    pub blob_count: usize,
    pub chunk_count: usize,
    #[serde(default)]
    pub verified_chunk_count: usize,
    #[serde(default)]
    pub verified_byte_count: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChunkEvidence {
    pub hash: String,
    pub offset: u64,
    pub size: u32,
    pub metadata_present: bool,
    #[serde(default)]
    pub object_present: Option<bool>,
    #[serde(default)]
    pub hash_verified: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct BlobEvidence {
    pub hash: String,
    pub metadata_present: bool,
    pub mapping_present: bool,
    #[serde(default)]
    pub chunks: Vec<ChunkEvidence>,
    #[serde(default)]
    pub affected: Vec<IntegrityLocation>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct IntegrityReport {
    pub schema_version: u32,
    pub repo: String,
    pub status: String,
    pub healthy: bool,
    #[serde(default = "bool_true")]
    pub complete: bool,
    #[serde(default)]
    pub truncated: bool,
    pub verification: Verification,
    /// Echo of the bounded server execution profile requested by clone.
    /// Older diagnostic responses omit it.
    #[serde(default)]
    pub proof_profile: Option<String>,
    /// Echo of the sparse worktree contract explicitly selected for clone.
    #[serde(default)]
    pub sparse_materialization_protocol: Option<String>,
    /// Authorized selected-branch identity bound into a clone snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_branch: Option<SelectedBranchEvidence>,
    /// Echo of the explicitly requested operator-adjudicated known-loss wire
    /// protocol. A client must not degrade without this exact acknowledgement.
    #[serde(default)]
    pub known_loss_protocol: Option<String>,
    #[serde(default)]
    pub snapshot_token: Option<String>,
    pub scope: IntegrityScope,
    #[serde(default)]
    pub findings: Vec<IntegrityFinding>,
    #[serde(default)]
    pub advisories: Vec<IntegrityAdvisory>,
    #[serde(default)]
    pub blob_evidence: Vec<BlobEvidence>,
    #[serde(default)]
    pub head_affected: bool,
    #[serde(default)]
    pub shallow_recovery_available: bool,
    #[serde(default)]
    pub recommended_next_commands: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SelectedBranchEvidence {
    pub name: String,
    pub head: String,
    pub status: String,
}

fn bool_true() -> bool {
    true
}

/// Render one data argument in a copyable POSIX-shell command. Every dynamic
/// acquisition value crosses this boundary so paths, remotes, and future
/// branch-name syntax can never become shell operators.
fn shell_argument(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '@'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn proves_complete_health(report: &IntegrityReport) -> bool {
    report.healthy
        && report.status == "healthy"
        && report.complete
        && !report.truncated
        && report.findings.is_empty()
}

fn adjudicated_known_loss_hashes(
    report: &IntegrityReport,
) -> Result<Option<std::collections::HashSet<String>>> {
    if report.known_loss_protocol.as_deref() != Some(oak_core::protocol::KNOWN_LOSS_PROTOCOL) {
        return Ok(None);
    }
    let disposition_shape = !report.healthy
        && report.status == "content_incomplete"
        && report.complete
        && !report.truncated
        && !report.head_affected
        && report.verification == Verification::Metadata
        && !report.findings.is_empty();
    if !disposition_shape {
        return Err(OakError::Server(
            "invalid known-loss proof: report_v1 requires a complete, non-truncated historical-only metadata report whose current heads are unaffected"
                .to_string(),
        ));
    }
    let mut hashes = std::collections::HashSet::with_capacity(report.findings.len());
    for finding in &report.findings {
        if finding.code != "known_lost_blob"
            || finding.recoverability != "operator_adjudicated_loss"
            || finding.commit_hash.is_some()
            || finding.tree_hash.is_some()
            || finding.manifest_hash.is_some()
            || finding.chunk_hash.is_some()
        {
            return Err(OakError::Server(format!(
                "content_incomplete: clone stopped before download because report_v1 included non-adjudicated finding {}",
                finding.code
            )));
        }
        let hash = finding.blob_hash.as_deref().ok_or_else(|| {
            OakError::Server(
                "invalid known-loss proof: known_lost_blob omitted blob_hash".to_string(),
            )
        })?;
        Hash::from_hex(hash).map_err(|error| {
            OakError::Server(format!("invalid known-loss blob hash {hash:?}: {error}"))
        })?;
        hashes.insert(hash.to_string());
    }
    Ok(Some(hashes))
}

enum FetchResult {
    Report(Box<IntegrityReport>),
    UnsupportedOrHidden,
}

struct FetchRequest<'a> {
    verification: Verification,
    depth: Option<u32>,
    blob: Option<&'a str>,
    branch: Option<&'a str>,
    selected_branch: Option<&'a str>,
    expected_head: Option<&'a str>,
    paths: Option<&'a [String]>,
    max_chunks: Option<usize>,
    max_bytes: Option<u64>,
    proof_profile: Option<&'a str>,
    sparse_materialization_protocol: Option<&'a str>,
    known_loss_protocol: Option<&'a str>,
}

pub(crate) const SPARSE_MATERIALIZATION_PROTOCOL: &str = "cone_v1";
pub(crate) const SELECTED_BRANCH_ACQUISITION: &str = "exact_head_v1";

pub(crate) fn effective_token(remote: &str) -> Option<String> {
    super::credentials::effective_token(remote, None)
}

async fn fetch_report(
    remote: &str,
    repo: &str,
    options: FetchRequest<'_>,
    auth_token: Option<&str>,
) -> Result<FetchResult> {
    let (owner, name) = super::parse_owner_repo(repo)?;
    let url = format!("{remote}/api/{owner}/{name}/integrity");
    let client = crate::http::api_client();
    let mut query = vec![("verify", options.verification.as_str().to_string())];
    if let Some(depth) = options.depth {
        query.push(("depth", depth.to_string()));
    }
    if let Some(blob) = options.blob {
        query.push(("blob", blob.to_string()));
    }
    if let Some(branch) = options.branch {
        query.push(("branch", branch.to_string()));
    }
    if let Some(branch) = options.selected_branch {
        query.push(("selected_branch", branch.to_string()));
    }
    if let Some(head) = options.expected_head {
        query.push(("expected_head", head.to_string()));
    }
    if let Some(paths) = options.paths.filter(|paths| !paths.is_empty()) {
        query.push(("paths", paths.join(",")));
    }
    if let Some(max_chunks) = options.max_chunks {
        query.push(("max_chunks", max_chunks.to_string()));
    }
    if let Some(max_bytes) = options.max_bytes {
        query.push(("max_bytes", max_bytes.to_string()));
    }
    if let Some(profile) = options.proof_profile {
        query.push(("proof_profile", profile.to_string()));
    }
    if let Some(protocol) = options.sparse_materialization_protocol {
        query.push(("sparse_materialization_protocol", protocol.to_string()));
    }
    if let Some(protocol) = options.known_loss_protocol {
        query.push(("known_loss_protocol", protocol.to_string()));
    }
    let mut request = client.get(url).query(&query);
    if let Some(token) = auth_token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = request
        .send()
        .await
        .map_err(|error| OakError::Http(error.to_string()))?;
    if response.status().as_u16() == 404 {
        return Ok(FetchResult::UnsupportedOrHidden);
    }
    if !response.status().is_success() {
        return Err(crate::http::server_error(response).await);
    }
    let mut report = response
        .json::<IntegrityReport>()
        .await
        .map_err(|error| OakError::Http(format!("invalid integrity response: {error}")))?;
    if report.verification != options.verification {
        return Err(OakError::Server(format!(
            "invalid integrity response: server returned {:?} proof for requested {:?} verification",
            report.verification, options.verification
        )));
    }
    for command in &mut report.recommended_next_commands {
        *command = command.replace("<url>", remote);
        if command.starts_with("oak clone ") && !command.contains(" --remote ") {
            command.push_str(&format!(" --remote {remote}"));
        }
    }
    Ok(FetchResult::Report(Box::new(report)))
}

fn print_report(report: &IntegrityReport, json: bool) -> Result<()> {
    if json {
        output::print_json(report)?;
        output::mark_json_payload_emitted();
        return Ok(());
    }
    if proves_complete_health(report) {
        output::success(&format!(
            "Content integrity verified for {} ({} commits, {} blobs, {} chunks)",
            report.repo,
            report.scope.commit_count,
            report.scope.blob_count,
            report.scope.chunk_count
        ));
        for advisory in &report.advisories {
            output::warning(&format!("{}: {}", advisory.code, advisory.detail));
        }
    } else {
        output::error(&format!(
            "{}: {} integrity finding(s)",
            report.status,
            report.findings.len()
        ));
        for finding in &report.findings {
            output::item(&format!("{}: {}", finding.code, finding.detail));
        }
        for command in &report.recommended_next_commands {
            output::item(&format!("Next: {command}"));
        }
    }
    Ok(())
}

fn finish_diagnostic(report: IntegrityReport, json: bool) -> Result<()> {
    print_report(&report, json)?;
    if proves_complete_health(&report) {
        Ok(())
    } else {
        Err(OakError::Server(format!(
            "content_incomplete: {} integrity finding(s) for {}",
            report.findings.len(),
            report.repo
        )))
    }
}

pub async fn doctor(
    remote: &str,
    repo: &str,
    verification: Verification,
    depth: Option<u32>,
    max_chunks: Option<usize>,
    max_bytes: Option<u64>,
    json: bool,
) -> Result<()> {
    if depth == Some(0) {
        return Err(OakError::InvalidArgument(
            "doctor --depth must be greater than zero".to_string(),
        ));
    }
    if verification == Verification::Bytes
        && (depth.is_none() || (max_chunks.is_none() && max_bytes.is_none()))
    {
        return Err(OakError::InvalidArgument(
            "doctor --verify bytes requires --depth and an explicit --max-chunks or --max-bytes budget"
                .to_string(),
        ));
    }
    let auth_token = effective_token(remote);
    match fetch_report(
        remote,
        repo,
        FetchRequest {
            verification,
            depth,
            blob: None,
            branch: None,
            selected_branch: None,
            expected_head: None,
            paths: None,
            max_chunks,
            max_bytes,
            proof_profile: None,
            sparse_materialization_protocol: None,
            known_loss_protocol: None,
        },
        auth_token.as_deref(),
    )
    .await?
    {
        FetchResult::Report(report) => {
            if proves_complete_health(&report) && depth.is_some() && report.scope.depth != depth {
                return Err(OakError::Server(format!(
                    "invalid integrity scope: server proved depth {:?}, requested {:?}",
                    report.scope.depth, depth
                )));
            }
            finish_diagnostic(*report, json)
        }
        FetchResult::UnsupportedOrHidden => Err(OakError::RemoteRepoNotFound(repo.to_string())),
    }
}

pub async fn blob_info(remote: &str, repo: &str, hash: &str, json: bool) -> Result<()> {
    blob_info_scoped(remote, repo, hash, None, None, json).await
}

pub async fn blob_info_scoped(
    remote: &str,
    repo: &str,
    hash: &str,
    depth: Option<u32>,
    branch: Option<&str>,
    json: bool,
) -> Result<()> {
    let auth_token = effective_token(remote);
    match fetch_report(
        remote,
        repo,
        FetchRequest {
            verification: Verification::Bytes,
            depth,
            blob: Some(hash),
            branch,
            selected_branch: None,
            expected_head: None,
            paths: None,
            max_chunks: Some(100_000),
            max_bytes: Some(256 * 1024 * 1024),
            proof_profile: None,
            sparse_materialization_protocol: None,
            known_loss_protocol: None,
        },
        auth_token.as_deref(),
    )
    .await?
    {
        FetchResult::Report(report) => {
            let history_only = !report.complete
                && report.status == "content_incomplete"
                && !report.findings.is_empty()
                && report
                    .findings
                    .iter()
                    .all(|finding| finding.code == "target_history_budget_exhausted");
            if history_only {
                print_report(&report, json)?;
                if !json {
                    output::warning(&format!(
                        "Blob evidence was verified across the newest {} commits; pass --depth with --branch when needed to select a wider bounded scope",
                        report.scope.commit_count
                    ));
                }
                Ok(())
            } else {
                finish_diagnostic(*report, json)
            }
        }
        FetchResult::UnsupportedOrHidden => Err(OakError::Server(
            "blob integrity evidence is unavailable or requires platform-admin access".to_string(),
        )),
    }
}

/// Fail a clone before destination creation and before the bulk pull download
/// when the current server can prove reachable content is incomplete.
///
/// A 404 retains compatibility with pre-integrity servers: the ordinary pull
/// remains authoritative and will distinguish an absent repo. Truncation is
/// never automatic; a historical-only failure merely recommends an explicit
/// `--shallow` retry.
pub async fn preflight_clone(
    remote: &str,
    repo: &str,
    shallow: bool,
    branch: Option<&str>,
    paths: Option<&[String]>,
) -> Result<()> {
    preflight_clone_with_policy(
        remote,
        repo,
        shallow,
        branch,
        paths,
        CloneIntegrityPolicy::default(),
    )
    .await
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CloneIntegrityPolicy {
    /// Permit a scoped clone against a pre-integrity server. The server may
    /// ignore depth/branch/path query parameters, so this is deliberately an
    /// explicit compatibility escape hatch rather than an automatic fallback.
    pub allow_legacy_scope: bool,
    /// Permit clone when the only finding is bounded proof exhaustion. This
    /// never overrides corruption, missing content, or contradictory reports.
    pub allow_unverified_budget: bool,
}

pub async fn preflight_clone_with_policy(
    remote: &str,
    repo: &str,
    shallow: bool,
    branch: Option<&str>,
    paths: Option<&[String]>,
    policy: CloneIntegrityPolicy,
) -> Result<()> {
    let auth_token = effective_token(remote);
    preflight_clone_with_policy_and_token(
        remote,
        repo,
        shallow,
        branch,
        paths,
        policy,
        auth_token.as_deref(),
    )
    .await
    .map(|_| ())
}

pub(crate) async fn preflight_clone_with_policy_and_token(
    remote: &str,
    repo: &str,
    shallow: bool,
    branch: Option<&str>,
    paths: Option<&[String]>,
    policy: CloneIntegrityPolicy,
    auth_token: Option<&str>,
) -> Result<ClonePreflightEvidence> {
    preflight_clone_acquisition_with_policy_and_token(
        remote, repo, shallow, branch, None, None, paths, policy, auth_token,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn preflight_clone_acquisition_with_policy_and_token(
    remote: &str,
    repo: &str,
    shallow: bool,
    branch: Option<&str>,
    selected_branch: Option<&str>,
    expected_head: Option<&str>,
    paths: Option<&[String]>,
    policy: CloneIntegrityPolicy,
    auth_token: Option<&str>,
) -> Result<ClonePreflightEvidence> {
    if selected_branch.is_none() && expected_head.is_some() {
        return Err(OakError::InvalidArgument(
            "an expected clone head requires a selected branch".to_string(),
        ));
    }
    if let Some(expected) = expected_head {
        if expected.len() != 64 || Hash::from_hex(expected).is_err() {
            return Err(OakError::InvalidArgument(
                "clone --expected-head requires a full lowercase 64-character Oak commit hash"
                    .to_string(),
            ));
        }
    }
    let (credential_accepted, sparse_materialization_v1, selected_branch_acquisition) =
        match probe_clone_preflight_capability(remote, repo, auth_token).await? {
            ClonePreflightCapability::BoundedV1 {
                credential_accepted,
                sparse_materialization_v1,
                selected_branch_acquisition,
            } => {
                if expected_head.is_some() && !selected_branch_acquisition {
                    return Err(OakError::Server(
                    "this server does not support exact selected-branch acquisition; upgrade the server before using --expected-head"
                        .to_string(),
                ));
                }
                (
                    credential_accepted,
                    sparse_materialization_v1,
                    selected_branch_acquisition,
                )
            }
            ClonePreflightCapability::LegacyAccessible => {
                if expected_head.is_some() {
                    return Err(OakError::Server(
                    "this server does not support exact selected-branch acquisition; --expected-head never falls back to legacy clone behavior"
                        .to_string(),
                ));
                }
                let scoped =
                    shallow || branch.is_some() || paths.is_some_and(|paths| !paths.is_empty());
                if !scoped || policy.allow_legacy_scope {
                    // Old servers cannot prove whether a presented credential was
                    // accepted. Pull may still use the bound token, but never
                    // persist it as valid without positive same-credential proof.
                    return Ok(ClonePreflightEvidence {
                        credential_accepted: false,
                        snapshot_token: None,
                        known_lost_blob_hashes: std::collections::HashSet::new(),
                        sparse_materialization_v1: false,
                        selected_branch: None,
                    });
                }
                return Err(OakError::Server(
                "this accessible server does not advertise bounded scoped integrity preflight; its pull endpoint may still honor depth, branch, and sparse paths, but Oak cannot prove that scope before download. Upgrade the server or retry with --allow-legacy-scope to waive only the preflight proof (downloaded objects remain hash-verified)"
                    .to_string(),
            ));
            }
        };
    let selected_branch = selected_branch.filter(|_| selected_branch_acquisition);
    let verification = clone_preflight_verification();
    let (snapshot_token, known_lost_blob_hashes, selected_branch) =
        preflight_clone_with_policy_and_verification(
            remote,
            repo,
            shallow,
            branch,
            selected_branch,
            expected_head,
            paths,
            policy,
            (
                verification,
                auth_token,
                sparse_materialization_v1 && paths.is_some_and(|paths| !paths.is_empty()),
            ),
        )
        .await?;
    Ok(ClonePreflightEvidence {
        credential_accepted,
        snapshot_token: Some(snapshot_token),
        known_lost_blob_hashes,
        sparse_materialization_v1: sparse_materialization_v1
            && paths.is_some_and(|paths| !paths.is_empty()),
        selected_branch,
    })
}

pub(crate) struct ClonePreflightEvidence {
    pub credential_accepted: bool,
    pub snapshot_token: Option<String>,
    pub known_lost_blob_hashes: std::collections::HashSet<String>,
    pub sparse_materialization_v1: bool,
    pub selected_branch: Option<SelectedBranchEvidence>,
}

fn clone_preflight_verification() -> Verification {
    // Authentication grants repository access; it must not silently select a
    // more expensive proof. Pull independently verifies downloaded hashes.
    Verification::Metadata
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClonePreflightCapability {
    BoundedV1 {
        credential_accepted: bool,
        sparse_materialization_v1: bool,
        selected_branch_acquisition: bool,
    },
    LegacyAccessible,
}

async fn probe_clone_preflight_capability(
    remote: &str,
    repo: &str,
    auth_token: Option<&str>,
) -> Result<ClonePreflightCapability> {
    let (owner, name) = super::parse_owner_repo(repo)?;
    let client = crate::http::api_client();
    let authorize = |request: reqwest::RequestBuilder| match auth_token {
        Some(token) => request.header("authorization", format!("Bearer {token}")),
        None => request,
    };
    let capability = authorize(client.get(format!(
        "{remote}/api/{owner}/{name}/integrity/capabilities"
    )))
    .send()
    .await
    .map_err(|error| OakError::Http(error.to_string()))?;
    if capability.status().is_success() {
        let value = capability
            .json::<serde_json::Value>()
            .await
            .map_err(|error| {
                OakError::Http(format!("invalid integrity capability response: {error}"))
            })?;
        if value
            .get("clone_preflight_profile")
            .and_then(|value| value.as_str())
            == Some("bounded_v1")
        {
            return Ok(ClonePreflightCapability::BoundedV1 {
                credential_accepted: value
                    .get("credential_accepted")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
                sparse_materialization_v1: value
                    .get("sparse_materialization_protocol")
                    .and_then(|value| value.as_str())
                    == Some(SPARSE_MATERIALIZATION_PROTOCOL),
                selected_branch_acquisition: value
                    .get("selected_branch_acquisition")
                    .and_then(|value| value.as_str())
                    == Some(SELECTED_BRANCH_ACQUISITION),
            });
        }
        return Err(OakError::Server(
            "server returned an unsupported integrity preflight capability".to_string(),
        ));
    }
    if capability.status().as_u16() != 404 {
        return Err(crate::http::server_error(capability).await);
    }

    // A capability 404 alone is ambiguous: old endpoint, hidden private repo,
    // or bad credentials. Probe the longstanding repo-info route with the same
    // effective credential before deciding legacy fallback is safe.
    let repo_info = authorize(client.get(format!("{remote}/api/{owner}/{name}")))
        .send()
        .await
        .map_err(|error| OakError::Http(error.to_string()))?;
    if repo_info.status().is_success() {
        return Ok(ClonePreflightCapability::LegacyAccessible);
    }
    if repo_info.status().as_u16() == 404 {
        return Err(OakError::Server(format!(
            "repository '{repo}' was not found or the presented credential was rejected; verify the remote and run `oak login --remote {remote}` before retrying"
        )));
    }
    Err(crate::http::server_error(repo_info).await)
}

#[allow(clippy::too_many_arguments)]
async fn preflight_clone_with_policy_and_verification(
    remote: &str,
    repo: &str,
    shallow: bool,
    branch: Option<&str>,
    selected_branch: Option<&str>,
    expected_head: Option<&str>,
    paths: Option<&[String]>,
    policy: CloneIntegrityPolicy,
    proof: (Verification, Option<&str>, bool),
) -> Result<(
    String,
    std::collections::HashSet<String>,
    Option<SelectedBranchEvidence>,
)> {
    let (verification, auth_token, sparse_materialization_v1) = proof;
    let depth = shallow.then_some(1);
    let result = fetch_report(
        remote,
        repo,
        FetchRequest {
            verification,
            depth,
            blob: None,
            branch,
            selected_branch,
            expected_head,
            paths,
            max_chunks: None,
            max_bytes: None,
            proof_profile: Some("bounded_v1"),
            sparse_materialization_protocol: sparse_materialization_v1
                .then_some(SPARSE_MATERIALIZATION_PROTOCOL),
            known_loss_protocol: Some(oak_core::protocol::KNOWN_LOSS_PROTOCOL),
        },
        auth_token,
    )
    .await?;
    let report = match result {
        FetchResult::Report(report) => *report,
        FetchResult::UnsupportedOrHidden => {
            return Err(OakError::Server(
                format!("repository access changed after bounded integrity capability was confirmed; verify the remote and run `oak login --remote {remote}` before retrying"),
            ));
        }
    };
    if report.repo != repo {
        return Err(OakError::Server(format!(
            "invalid integrity response: server reported repository '{}' while '{}' was requested",
            report.repo, repo
        )));
    }
    if report.proof_profile.as_deref() != Some("bounded_v1") {
        return Err(OakError::Server(
            "invalid integrity response: server did not echo the requested bounded clone preflight profile"
                .to_string(),
        ));
    }
    if sparse_materialization_v1
        && report.sparse_materialization_protocol.as_deref()
            != Some(SPARSE_MATERIALIZATION_PROTOCOL)
    {
        return Err(OakError::Server(
            "invalid integrity response: server did not acknowledge the requested sparse materialization protocol"
                .to_string(),
        ));
    }
    let snapshot_token = report.snapshot_token.clone().ok_or_else(|| {
        OakError::Server(
            "invalid integrity response: bounded preflight omitted its snapshot token".to_string(),
        )
    })?;
    let selected_evidence = match selected_branch {
        Some(name) => {
            let evidence = report.selected_branch.clone().ok_or_else(|| {
                OakError::Server(
                    "invalid integrity response: clone proof omitted selected branch identity"
                        .to_string(),
                )
            })?;
            let head_matches = expected_head.is_none_or(|head| evidence.head == head);
            if evidence.name != name
                || !head_matches
                || evidence.status != "open"
                || evidence.head.len() != 64
                || Hash::from_hex(&evidence.head).is_err()
            {
                return Err(OakError::Server(
                    "invalid integrity response: selected branch identity did not match the requested open branch and optional exact head"
                        .to_string(),
                ));
            }
            Some(evidence)
        }
        None => {
            if report.selected_branch.is_some() {
                return Err(OakError::Server(
                    "invalid integrity response: server returned unrequested selected branch identity"
                        .to_string(),
                ));
            }
            None
        }
    };
    if proves_complete_health(&report) {
        return Ok((
            snapshot_token,
            std::collections::HashSet::new(),
            selected_evidence,
        ));
    }
    if let Some(hashes) = adjudicated_known_loss_hashes(&report)? {
        output::warning(&format!(
            "Server reports {} operator-adjudicated historical blob(s) as permanently unavailable; clone will preserve the history metadata and report omitted content",
            hashes.len()
        ));
        return Ok((snapshot_token, hashes, selected_evidence));
    }
    let budget_only = !report.healthy
        && report.status == "content_incomplete"
        && !report.complete
        && report.truncated
        && report.verification == Verification::Metadata
        && !report.findings.is_empty()
        && report.findings.iter().all(|finding| {
            matches!(
                finding.code.as_str(),
                "clone_preflight_history_budget_exhausted"
                    | "clone_preflight_traversal_budget_exhausted"
                    | "clone_preflight_wall_budget_exhausted"
            )
        });
    if policy.allow_unverified_budget && budget_only {
        output::warning(
            "Clone integrity proof exhausted its bounded metadata budget; proceeding because --allow-unverified-integrity was explicitly supplied",
        );
        return Ok((
            snapshot_token,
            std::collections::HashSet::new(),
            selected_evidence,
        ));
    }
    let first = report
        .findings
        .first()
        .map(|finding| format!("{}: {}", finding.code, finding.detail))
        .unwrap_or_else(|| "server reported incomplete content".to_string());
    let mut recommended_next_commands = report.recommended_next_commands.clone();
    if budget_only {
        let mut arguments = vec!["oak".to_string(), "clone".to_string(), repo.to_string()];
        if shallow {
            arguments.push("--shallow".to_string());
        }
        if let Some(branch) = selected_branch.or(branch) {
            arguments.extend(["--branch".to_string(), branch.to_string()]);
        }
        if let Some(head) = expected_head {
            arguments.extend(["--expected-head".to_string(), head.to_string()]);
        }
        if let Some(paths) = paths {
            for path in paths {
                arguments.extend(["--path".to_string(), path.clone()]);
            }
        }
        arguments.extend([
            "--allow-unverified-integrity".to_string(),
            "--remote".to_string(),
            remote.to_string(),
        ]);
        let command = arguments
            .iter()
            .map(|argument| shell_argument(argument))
            .collect::<Vec<_>>()
            .join(" ");
        recommended_next_commands.insert(0, command);
    }
    let guidance = if recommended_next_commands.is_empty() {
        String::new()
    } else {
        format!(" Next: {}", recommended_next_commands.join("; "))
    };
    Err(OakError::Server(format!(
        "content_incomplete: clone stopped before download. {first}.{guidance}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_accepts_only_complete_operator_adjudicated_known_loss() {
        let report: IntegrityReport = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "repo": "oak/oakspace",
            "status": "content_incomplete",
            "healthy": false,
            "complete": true,
            "truncated": false,
            "verification": "metadata",
            "proof_profile": "bounded_v1",
            "known_loss_protocol": "report_v1",
            "snapshot_token": "snapshot",
            "scope": {"commit_count":1,"manifest_count":1,"blob_count":1,"chunk_count":0},
            "findings": [{
                "code": "known_lost_blob",
                "blob_hash": "ab".repeat(32),
                "recoverability": "operator_adjudicated_loss",
                "detail": "legacy bytes unavailable"
            }]
        }))
        .unwrap();

        let hashes = adjudicated_known_loss_hashes(&report)
            .expect("valid report")
            .expect("known loss disposition");
        assert_eq!(hashes, std::collections::HashSet::from(["ab".repeat(32)]));

        let mut current_loss = report.clone();
        current_loss.head_affected = true;
        assert!(
            adjudicated_known_loss_hashes(&current_loss).is_err(),
            "report_v1 must never authorize an incomplete current checkout"
        );

        let mut mixed = report;
        mixed.findings.push(IntegrityFinding {
            code: "missing_chunk_object".to_string(),
            commit_hash: None,
            tree_hash: None,
            manifest_hash: None,
            blob_hash: None,
            chunk_hash: Some("cd".repeat(32)),
            affected: Vec::new(),
            recoverability: "reupload".to_string(),
            detail: "unexpected loss".to_string(),
        });
        assert!(adjudicated_known_loss_hashes(&mixed).is_err());
    }

    async fn mount_bounded_clone_capability(server: &wiremock::MockServer) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity/capabilities"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "clone_preflight_profile": "bounded_v1",
                "credential_presented": true,
                "credential_accepted": true
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    #[test]
    fn server_wire_fixture_accepts_future_fields_and_defaulted_arrays() {
        let fixture = serde_json::json!({
            "schema_version": 1,
            "repo": "oak/repo",
            "status": "healthy",
            "healthy": true,
            "verification": "metadata",
            "scope": {"depth": 1, "commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
            "future_server_field": {"append_only": true}
        });
        let report: IntegrityReport = serde_json::from_value(fixture).unwrap();
        assert!(report.healthy);
        assert!(report.findings.is_empty());
        assert!(report.blob_evidence.is_empty());
    }

    #[tokio::test]
    async fn blob_info_requests_the_bounded_admin_target_budget() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let hash = "a".repeat(64);
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .and(query_param("verify", "bytes"))
            .and(query_param("blob", hash.as_str()))
            .and(query_param("depth", "1024"))
            .and(query_param("branch", "main"))
            .and(query_param("max_chunks", "100000"))
            .and(query_param("max_bytes", "268435456"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "healthy",
                "healthy": true,
                "complete": true,
                "truncated": false,
                "verification": "bytes",
                "scope": {
                    "commit_count": 1,
                    "manifest_count": 1,
                    "blob_count": 1,
                    "chunk_count": 1,
                    "verified_chunk_count": 1,
                    "verified_byte_count": 12
                },
                "blob_evidence": [{
                    "hash": hash,
                    "metadata_present": true,
                    "mapping_present": true,
                    "chunks": [],
                    "affected": []
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        blob_info_scoped(
            &server.uri(),
            "oak/repo",
            &hash,
            Some(1024),
            Some("main"),
            true,
        )
        .await
        .expect("bounded target inspection succeeds");
    }

    #[tokio::test]
    async fn blob_info_treats_target_history_window_as_verified_scope_not_corruption() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "content_incomplete",
                "healthy": false,
                "complete": false,
                "truncated": true,
                "verification": "bytes",
                "scope": {"commit_count": 256, "manifest_count": 256, "blob_count": 1, "chunk_count": 1},
                "findings": [{
                    "code": "target_history_budget_exhausted",
                    "recoverability": "requires_authoritative_bytes",
                    "detail": "bounded at 256"
                }]
            })))
            .mount(&server)
            .await;

        blob_info(&server.uri(), "oak/repo", &"a".repeat(64), false)
            .await
            .expect("history-only truncation is a successful bounded inspection");
    }

    #[tokio::test]
    async fn preflight_stops_before_download_and_preserves_shallow_guidance() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        mount_bounded_clone_capability(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .and(query_param("verify", "metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "content_incomplete",
                "healthy": false,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "test-snapshot",
                "scope": {"commit_count": 2, "manifest_count": 2, "blob_count": 1, "chunk_count": 0},
                "findings": [{
                    "code": "missing_blob_mapping",
                    "blob_hash": "deadbeef",
                    "affected": [{"commit": "old", "path": "legacy.bin"}],
                    "recoverability": "requires_authoritative_bytes",
                    "detail": "reachable blob deadbeef has no chunk mapping"
                }],
                "head_affected": false,
                "shallow_recovery_available": true,
                "recommended_next_commands": ["oak clone oak/repo --shallow"]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let error = preflight_clone(&server.uri(), "oak/repo", false, None, None)
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("content_incomplete"));
        assert!(message.contains("stopped before download"));
        assert!(message.contains("oak clone oak/repo --shallow"));
    }

    #[tokio::test]
    async fn preflight_capability_404_falls_back_only_after_repo_access_is_proven() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity/capabilities"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "repo"
            })))
            .expect(1)
            .mount(&server)
            .await;
        preflight_clone(&server.uri(), "oak/repo", false, None, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn preflight_capability_404_does_not_hide_private_or_unauthorized_repo() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/private/integrity/capabilities"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/oak/private"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let error = preflight_clone(&server.uri(), "oak/private", false, None, None)
            .await
            .expect_err("a hidden repo must not be treated as an old server");
        assert!(error
            .to_string()
            .contains("not found or the presented credential was rejected"));
        assert!(error
            .to_string()
            .contains(&format!("oak login --remote {}", server.uri())));
    }

    #[tokio::test]
    async fn integrity_404_after_confirmed_capability_is_never_legacy_fallback() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        mount_bounded_clone_capability(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let error = preflight_clone_with_policy(
            &server.uri(),
            "oak/repo",
            false,
            None,
            None,
            CloneIntegrityPolicy {
                allow_legacy_scope: true,
                allow_unverified_budget: false,
            },
        )
        .await
        .expect_err("a modern capability cannot downgrade after access changes");
        assert!(error.to_string().contains("access changed"));
        assert!(error
            .to_string()
            .contains(&format!("oak login --remote {}", server.uri())));
    }

    #[tokio::test]
    async fn scoped_clone_requires_explicit_legacy_server_override() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity/capabilities"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "repo"})),
            )
            .mount(&server)
            .await;
        let error = preflight_clone(&server.uri(), "oak/repo", true, Some("feature"), None)
            .await
            .expect_err("scoped legacy clone must not silently widen scope");
        assert!(error.to_string().contains("--allow-legacy-scope"));
        assert!(error
            .to_string()
            .contains("downloaded objects remain hash-verified"));

        preflight_clone_with_policy(
            &server.uri(),
            "oak/repo",
            true,
            Some("feature"),
            None,
            CloneIntegrityPolicy {
                allow_legacy_scope: true,
                allow_unverified_budget: false,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn legacy_access_never_claims_an_unverifiable_credential_was_accepted() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity/capabilities"))
            .and(header("authorization", "Bearer rejected-token"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo"))
            .and(header("authorization", "Bearer rejected-token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "repo"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let evidence = preflight_clone_with_policy_and_token(
            &server.uri(),
            "oak/repo",
            false,
            None,
            None,
            CloneIntegrityPolicy::default(),
            Some("rejected-token"),
        )
        .await
        .expect("unscoped legacy clone remains compatible");
        assert!(!evidence.credential_accepted);
        assert!(evidence.snapshot_token.is_none());
    }

    #[tokio::test]
    async fn branch_only_clone_requires_explicit_legacy_server_override() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity/capabilities"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "repo"})),
            )
            .mount(&server)
            .await;

        let error = preflight_clone(&server.uri(), "oak/repo", false, Some("feature"), None)
            .await
            .expect_err("branch-only legacy clone must not silently widen scope");
        assert!(error.to_string().contains("--allow-legacy-scope"));
    }

    #[tokio::test]
    async fn explicit_budget_override_never_overrides_corruption_findings() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "content_incomplete",
                "healthy": false,
                "complete": false,
                "truncated": true,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "test-snapshot",
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 1, "chunk_count": 100001},
                "findings": [{
                    "code": "missing_chunk_object",
                    "blob_hash": "deadbeef",
                    "affected": [],
                    "recoverability": "requires_authoritative_bytes",
                    "detail": "missing object"
                }, {
                    "code": "clone_preflight_traversal_budget_exhausted",
                    "affected": [],
                    "recoverability": "requires_authoritative_bytes",
                    "detail": "100000 probes attempted"
                }]
            })))
            .mount(&server)
            .await;

        let error = preflight_clone_with_policy_and_verification(
            &server.uri(),
            "oak/repo",
            false,
            None,
            None,
            None,
            None,
            CloneIntegrityPolicy {
                allow_legacy_scope: false,
                allow_unverified_budget: true,
            },
            (Verification::Metadata, None, false),
        )
        .await
        .expect_err("real corruption cannot be bypassed");
        assert!(error.to_string().contains("missing_chunk_object"));
    }

    #[tokio::test]
    async fn explicit_budget_override_allows_only_bounded_proof_exhaustion() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "content_incomplete",
                "healthy": false,
                "complete": false,
                "truncated": true,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "test-snapshot",
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 1, "chunk_count": 100001},
                "findings": [{
                    "code": "clone_preflight_history_budget_exhausted",
                    "affected": [],
                    "recoverability": "requires_authoritative_bytes",
                    "detail": "100000 probes attempted"
                }]
            })))
            .mount(&server)
            .await;

        preflight_clone_with_policy_and_verification(
            &server.uri(),
            "oak/repo",
            false,
            None,
            None,
            None,
            None,
            CloneIntegrityPolicy {
                allow_legacy_scope: false,
                allow_unverified_budget: true,
            },
            (Verification::Metadata, None, false),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn clone_budget_exhaustion_recommends_the_safe_explicit_override() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "content_incomplete",
                "healthy": false,
                "complete": false,
                "truncated": true,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "test-snapshot",
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 1, "chunk_count": 100001},
                "findings": [{
                    "code": "clone_preflight_history_budget_exhausted",
                    "affected": [],
                    "recoverability": "requires_authoritative_bytes",
                    "detail": "100000 probes attempted"
                }],
                "recommended_next_commands": [
                    "oak doctor --repo oak/repo --verify bytes --depth 1 --max-bytes 67108864"
                ]
            })))
            .mount(&server)
            .await;

        let error = preflight_clone_with_policy_and_verification(
            &server.uri(),
            "oak/repo",
            false,
            None,
            None,
            None,
            None,
            CloneIntegrityPolicy::default(),
            (Verification::Metadata, None, false),
        )
        .await
        .expect_err("budget exhaustion remains fail closed by default");
        assert!(
            error.to_string().contains(&format!(
                "oak clone oak/repo --allow-unverified-integrity --remote {}",
                server.uri()
            )),
            "{error}"
        );
    }

    #[tokio::test]
    async fn pinned_clone_budget_guidance_preserves_full_selection_and_pin() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::builder().start().await;
        let expected = "a".repeat(64);
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "content_incomplete",
                "healthy": false,
                "complete": false,
                "truncated": true,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "test-snapshot",
                "selected_branch": {"name": "feature", "head": expected, "status": "open"},
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
                "findings": [{
                    "code": "clone_preflight_wall_budget_exhausted",
                    "affected": [],
                    "recoverability": "unknown",
                    "detail": "synthetic bounded proof budget"
                }],
                "recommended_next_commands": []
            })))
            .mount(&server)
            .await;

        let error = preflight_clone_with_policy_and_verification(
            &server.uri(),
            "oak/repo",
            false,
            None,
            Some("feature"),
            Some(&expected),
            None,
            CloneIntegrityPolicy::default(),
            (Verification::Metadata, None, false),
        )
        .await
        .expect_err("budget exhaustion remains fail closed by default");
        let message = error.to_string();
        assert!(
            message.contains("oak clone oak/repo --branch feature"),
            "{message}"
        );
        assert!(
            message.contains(&format!("--expected-head {expected}")),
            "{message}"
        );
        assert!(!message.contains("--shallow"), "{message}");
    }

    #[tokio::test]
    async fn clone_budget_guidance_shell_quotes_every_dynamic_argument() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::builder().start().await;
        let expected = "b".repeat(64);
        let selected = "feature; printf QA_MARKER";
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "content_incomplete",
                "healthy": false,
                "complete": false,
                "truncated": true,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "test-snapshot",
                "selected_branch": {"name": selected, "head": expected, "status": "open"},
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
                "findings": [{
                    "code": "clone_preflight_history_budget_exhausted",
                    "affected": [],
                    "recoverability": "unknown",
                    "detail": "synthetic bounded proof budget"
                }],
                "recommended_next_commands": []
            })))
            .mount(&server)
            .await;

        let path_with_quote = "docs/it's $(data)".to_string();
        let error = preflight_clone_with_policy_and_verification(
            &server.uri(),
            "oak/repo",
            true,
            Some(selected),
            Some(selected),
            Some(&expected),
            Some(std::slice::from_ref(&path_with_quote)),
            CloneIntegrityPolicy::default(),
            (Verification::Metadata, None, false),
        )
        .await
        .expect_err("budget exhaustion remains fail closed by default");
        let message = error.to_string();
        assert!(
            message.contains("--branch 'feature; printf QA_MARKER'"),
            "{message}"
        );
        assert!(
            message.contains("--path 'docs/it'\\''s $(data)'"),
            "{message}"
        );
        assert!(message.contains("--shallow"), "{message}");
        assert!(
            message.contains(&format!("--expected-head {expected}")),
            "{message}"
        );
    }

    #[tokio::test]
    async fn doctor_rejects_a_weaker_verification_mode_than_requested() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "healthy",
                "healthy": true,
                "complete": true,
                "truncated": false,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "test-snapshot",
                "scope": {"depth": 1, "commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
                "findings": []
            })))
            .mount(&server)
            .await;

        let error = doctor(
            &server.uri(),
            "oak/repo",
            Verification::Existence,
            Some(1),
            Some(1),
            None,
            true,
        )
        .await
        .expect_err("metadata cannot satisfy an existence request");
        assert!(error.to_string().contains("invalid integrity response"));
    }

    #[tokio::test]
    async fn blob_info_rejects_a_weaker_verification_mode_than_requested() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "healthy",
                "healthy": true,
                "complete": true,
                "truncated": false,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "test-snapshot",
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 1, "chunk_count": 1},
                "findings": []
            })))
            .mount(&server)
            .await;

        let error = blob_info(&server.uri(), "oak/repo", &"a".repeat(64), true)
            .await
            .expect_err("metadata cannot satisfy a byte-verification request");
        assert!(error.to_string().contains("invalid integrity response"));
    }

    #[tokio::test]
    async fn anonymous_public_clone_requests_the_cheap_metadata_proof() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        mount_bounded_clone_capability(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .and(query_param("verify", "metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "healthy",
                "healthy": true,
                "complete": true,
                "truncated": false,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "test-snapshot",
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 1, "chunk_count": 1},
                "findings": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        preflight_clone(&server.uri(), "oak/repo", false, None, None)
            .await
            .expect("metadata proof is sufficient before byte-verified pull");
    }

    #[test]
    fn authentication_never_silently_strengthens_clone_preflight() {
        assert_eq!(clone_preflight_verification(), Verification::Metadata);
    }

    #[tokio::test]
    async fn doctor_rejects_silently_clamped_depth_report() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "healthy",
                "healthy": true,
                "complete": true,
                "verification": "existence",
                "scope": {"depth": 10000, "commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0}
            })))
            .mount(&server)
            .await;
        let error = doctor(
            &server.uri(),
            "oak/repo",
            Verification::Existence,
            Some(10001),
            None,
            None,
            true,
        )
        .await
        .expect_err("client must reject a silently narrowed proof");
        assert!(error.to_string().contains("invalid integrity scope"));
    }

    #[tokio::test]
    async fn preflight_proves_selected_branch_and_sparse_cone() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        mount_bounded_clone_capability(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .and(query_param("verify", "metadata"))
            .and(query_param("depth", "1"))
            .and(query_param("branch", "feature"))
            .and(query_param("paths", "src,docs/api"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "healthy",
                "healthy": true,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "test-snapshot",
                "scope": {"depth": 1, "commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
                "head_affected": false,
                "shallow_recovery_available": false
            })))
            .expect(1)
            .mount(&server)
            .await;

        preflight_clone(
            &server.uri(),
            "oak/repo",
            true,
            Some("feature"),
            Some(&["src".to_string(), "docs/api".to_string()]),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn unpinned_selected_branch_proof_rejects_non_native_head_length() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "healthy",
                "healthy": true,
                "complete": true,
                "truncated": false,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "selected",
                "selected_branch": {"name": "feature", "head": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "status": "open"},
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
                "findings": []
            })))
            .mount(&server)
            .await;

        let error = preflight_clone_with_policy_and_verification(
            &server.uri(),
            "oak/repo",
            false,
            None,
            Some("feature"),
            None,
            None,
            CloneIntegrityPolicy::default(),
            (Verification::Metadata, None, false),
        )
        .await
        .expect_err("selected head must use Oak's native full identity");
        assert!(error.to_string().contains("selected branch identity"));
    }

    #[tokio::test]
    async fn byte_doctor_requires_explicit_depth_and_budget() {
        let error = doctor(
            "http://unused.invalid",
            "oak/repo",
            Verification::Bytes,
            None,
            None,
            None,
            true,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("requires --depth"));
    }

    #[tokio::test]
    async fn doctor_rejects_contradictory_incomplete_healthy_report() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "healthy",
                "healthy": true,
                "complete": false,
                "verification": "existence",
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0}
            })))
            .mount(&server)
            .await;

        let error = doctor(
            &server.uri(),
            "oak/repo",
            Verification::Existence,
            Some(1),
            None,
            None,
            true,
        )
        .await
        .expect_err("incomplete report cannot be accepted as healthy");
        assert!(error.to_string().contains("content_incomplete"));
    }

    #[tokio::test]
    async fn clone_rejects_contradictory_truncated_healthy_report() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        mount_bounded_clone_capability(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "healthy",
                "healthy": true,
                "complete": true,
                "truncated": true,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "test-snapshot",
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0}
            })))
            .mount(&server)
            .await;

        let error = preflight_clone(&server.uri(), "oak/repo", false, None, None)
            .await
            .expect_err("truncated report cannot authorize clone");
        assert!(error.to_string().contains("content_incomplete"));
    }

    #[tokio::test]
    async fn clone_rejects_integrity_report_for_a_different_repository() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        mount_bounded_clone_capability(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/different",
                "status": "healthy",
                "healthy": true,
                "complete": true,
                "truncated": false,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "wrong-repository-snapshot",
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0},
                "findings": []
            })))
            .mount(&server)
            .await;

        let error = preflight_clone(&server.uri(), "oak/repo", false, None, None)
            .await
            .expect_err("wrong-repository evidence must not authorize clone");
        assert!(error.to_string().contains("oak/different"));
        assert!(error.to_string().contains("oak/repo"));
    }

    #[tokio::test]
    async fn doctor_rejects_contradictory_status_on_healthy_report() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "content_incomplete",
                "healthy": true,
                "complete": true,
                "truncated": false,
                "verification": "existence",
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 0, "chunk_count": 0}
            })))
            .mount(&server)
            .await;

        let error = doctor(
            &server.uri(),
            "oak/repo",
            Verification::Existence,
            Some(1),
            None,
            None,
            true,
        )
        .await
        .expect_err("non-healthy status cannot authorize doctor success");
        assert!(error.to_string().contains("content_incomplete"));
    }

    #[tokio::test]
    async fn clone_rejects_healthy_report_with_findings() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        mount_bounded_clone_capability(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/oak/repo/integrity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": 1,
                "repo": "oak/repo",
                "status": "healthy",
                "healthy": true,
                "complete": true,
                "truncated": false,
                "verification": "metadata",
                "proof_profile": "bounded_v1",
                "snapshot_token": "test-snapshot",
                "scope": {"commit_count": 1, "manifest_count": 1, "blob_count": 1, "chunk_count": 0},
                "findings": [{
                    "code": "missing_blob_mapping",
                    "blob_hash": "deadbeef",
                    "affected": [],
                    "recoverability": "requires_authoritative_bytes",
                    "detail": "contradictory finding"
                }]
            })))
            .mount(&server)
            .await;

        let error = preflight_clone(&server.uri(), "oak/repo", false, None, None)
            .await
            .expect_err("findings cannot authorize clone");
        assert!(error.to_string().contains("content_incomplete"));
    }

    #[tokio::test]
    async fn doctor_rejects_zero_depth_before_network() {
        let error = doctor(
            "http://unused.invalid",
            "oak/repo",
            Verification::Existence,
            Some(0),
            None,
            None,
            true,
        )
        .await
        .expect_err("zero depth must be invalid");
        assert!(error.to_string().contains("greater than zero"));
    }
}
