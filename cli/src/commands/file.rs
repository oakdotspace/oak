//! Committed-file evidence, never working-copy content. `inspect` is local
//! only; `inspect_remote` reads one exact remote commit's file through the
//! bounded, hash-verified raw route and never changes refs.
use oak_core::{sqlite::file_inspect::InspectionStatus, OakError, Result, SqliteRepository};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub fn inspect(
    cwd: &Path,
    revision: &str,
    path: &str,
    max_bytes: u64,
    output: Option<&Path>,
    json: bool,
) -> Result<bool> {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    if super::mount::mount_dest_for(cwd)?.is_some() {
        return Err(OakError::Unsupported {
            backend: "mount",
            op: "file inspect",
        });
    }
    let ctx = crate::resolve::resolve(cwd)?;
    if !matches!(ctx.backend, crate::resolve::Backend::Sqlite) {
        return Err(OakError::Unsupported {
            backend: "git",
            op: "file inspect",
        });
    }
    let output_path = output
        .map(|output| prepare_output_path(cwd, &ctx.work_tree, output))
        .transpose()?;
    let repo = SqliteRepository::open_read_only(&ctx.db_path()?)?;
    let (evidence, written) = match output_path.as_deref() {
        None => (repo.inspect_pinned_file(revision, path, max_bytes)?, None),
        Some(output_path) => write_verified_output(&repo, revision, path, max_bytes, output_path)?,
    };
    let verified = evidence.status == InspectionStatus::Verified;
    #[derive(Serialize)]
    struct Output {
        schema_version: u32,
        kind: &'static str,
        repository_root: PathBuf,
        backend: &'static str,
        evidence: oak_core::sqlite::file_inspect::FileInspection,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<OutputReceipt>,
        recommended_next_commands: Vec<&'static str>,
    }
    let output_receipt = output_path.map(|path| OutputReceipt {
        written: written.is_some(),
        bytes: written,
        not_written_reason: if written.is_some() {
            None
        } else {
            Some("content was not verified; nothing was created")
        },
        path,
        permissions: "owner_only",
        represents: "logical_bytes",
    });
    if json {
        crate::output::print_json(&Output {
            schema_version: 1,
            kind: "file_inspection",
            repository_root: ctx.work_tree,
            backend: "sqlite",
            evidence,
            output: output_receipt,
            recommended_next_commands: vec!["oak file inspect --help"],
        })?;
    } else {
        crate::output::print_line(&format!(
            "{:?}: {} at {}",
            evidence.status,
            evidence.path,
            evidence.commit.as_deref().unwrap_or(revision)
        ));
        if let Some(reason) = evidence.reason {
            crate::output::print_line(&reason);
        }
        if let Some(receipt) = output_receipt.filter(|receipt| receipt.written) {
            crate::output::print_line(&format!(
                "wrote {} verified logical bytes to {}",
                receipt.bytes.unwrap_or(0),
                receipt.path.display()
            ));
        }
    }
    Ok(verified)
}

#[derive(Serialize)]
struct OutputReceipt {
    path: PathBuf,
    written: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    not_written_reason: Option<&'static str>,
    /// The file is created owner-only (0600 on Unix), never as a symlink or
    /// with the committed mode; `evidence.mode` records the committed mode.
    permissions: &'static str,
    represents: &'static str,
}

/// Resolve `--output` against the caller's directory and refuse anything
/// that could clobber or land inside repository metadata. The final create
/// is still no-clobber, so a path raced into existence is never replaced.
pub(crate) fn prepare_output_path(cwd: &Path, work_tree: &Path, output: &Path) -> Result<PathBuf> {
    if output.as_os_str().is_empty() {
        return Err(OakError::InvalidArgument(
            "--output must name a new file".into(),
        ));
    }
    let absolute = lexical_absolute(cwd, output);
    if absolute.file_name().is_none() {
        return Err(OakError::InvalidArgument(
            "--output must name a new file".into(),
        ));
    }
    if std::fs::symlink_metadata(&absolute).is_ok() {
        return Err(OakError::InvalidArgument(format!(
            "--output {} already exists; refusing to overwrite",
            absolute.display()
        )));
    }
    let parent = absolute.parent().unwrap_or(Path::new("."));
    let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
        OakError::InvalidArgument(format!(
            "--output parent {} is not an existing directory: {error}",
            parent.display()
        ))
    })?;
    let metadata = std::fs::canonicalize(work_tree)
        .unwrap_or_else(|_| work_tree.to_path_buf())
        .join(".oak");
    if canonical_parent.starts_with(&metadata) {
        return Err(OakError::InvalidArgument(
            "--output must not be inside the repository's .oak metadata directory".into(),
        ));
    }
    Ok(absolute)
}

/// Join `path` onto `cwd` and resolve `.`/`..` lexically, so the path that
/// is safety-checked is exactly the path that is written. (`..` above the
/// root stays at the root.) Symlinked parents are then checked through
/// `canonicalize` by the callers.
pub(crate) fn lexical_absolute(cwd: &Path, path: &Path) -> PathBuf {
    use std::path::Component;
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    out.components().next_back(),
                    None | Some(Component::RootDir | Component::Prefix(_))
                ) {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Stage decoded bytes in a private temp file beside `output`, publish it
/// with a no-clobber rename only when the inspection verified, and discard it
/// otherwise. Returns the evidence and the published byte count.
fn write_verified_output(
    repo: &SqliteRepository,
    revision: &str,
    path: &str,
    max_bytes: u64,
    output: &Path,
) -> Result<(oak_core::sqlite::file_inspect::FileInspection, Option<u64>)> {
    let mut evidence = None;
    let published = crate::atomic_file::write_atomic_private_noclobber(output, |file| {
        let mut writer = std::io::BufWriter::new(file);
        let inspected = repo.inspect_pinned_file_into(revision, path, max_bytes, &mut writer)?;
        use std::io::Write;
        writer.flush()?;
        let verified = inspected.status == InspectionStatus::Verified;
        let size = inspected.verified_size;
        evidence = Some(inspected);
        if verified {
            Ok(size)
        } else {
            Err(OakError::InvalidArgument(UNVERIFIED_SENTINEL.into()))
        }
    });
    match published {
        Ok(size) => Ok((evidence.expect("inspection ran"), size)),
        Err(OakError::InvalidArgument(message)) if message == UNVERIFIED_SENTINEL => {
            Ok((evidence.expect("inspection ran"), None))
        }
        Err(error) => Err(error),
    }
}

const UNVERIFIED_SENTINEL: &str = "\0oak-file-inspect-unverified";

/// Outcome of one remote file read. Never carries response bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RemoteFileStatus {
    Verified,
    PathMissing,
    NotAFile,
    AccessDenied,
    NotFound,
    BudgetExceeded,
    HashMismatch,
    InvalidPath,
    Unavailable,
}

#[derive(Serialize)]
struct RemoteFileInspection {
    schema_version: u32,
    kind: &'static str,
    remote: String,
    requested_revision: String,
    /// `remote_branch_list` when `--at` named a branch (pinned once),
    /// `explicit_commit` when it was a full commit hash.
    head_source: &'static str,
    /// The exact commit every other field is bound to.
    commit: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    blob: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<oak_core::FileMode>,
    status: RemoteFileStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// `local_cache` when verified bytes were already present, otherwise
    /// `remote_raw`. Both are checked against the pinned manifest hash.
    #[serde(skip_serializing_if = "Option::is_none")]
    content_source: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verified_size: Option<usize>,
    /// Exact file text when verified and valid UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_omitted_reason: Option<&'static str>,
    max_bytes: u64,
    acquisition: crate::commands::blob_fetch::AcquisitionCounters,
    recommended_next_commands: Vec<String>,
}

/// `oak file inspect --remote --at <branch|commit> PATH` (fb-430, fb-96):
/// resolve the head once, acquire exactly that commit's metadata, then read
/// one file through the permission-checked raw route. Bytes are admitted
/// only when they hash to the pinned manifest entry. No refs, branch rows,
/// or working files change.
pub async fn inspect_remote(
    cwd: &Path,
    revision: &str,
    path: &str,
    max_bytes: u64,
    json: bool,
) -> Result<bool> {
    use crate::commands::blob_fetch::ReviewPreparationBudget;
    use crate::commands::review::remote_content::{read_raw_verified, RawReadFailure};
    use oak_core::{Hash, Repository};

    let workspace = crate::commands::branch::RemoteWorkspace::open(cwd)?;
    let repo = &workspace.repo;
    let remote = &workspace.remote;
    let mut branch_list_requests = 0;
    let (commit, head_source) = if revision.len() == 64
        && revision.bytes().all(|b| b.is_ascii_hexdigit())
    {
        (
            Hash::from_hex(&revision.to_ascii_lowercase())?,
            "explicit_commit",
        )
    } else if revision.bytes().all(|b| b.is_ascii_hexdigit()) && revision.len() >= 4 {
        return Err(OakError::InvalidArgument(format!(
            "--at {revision}: abbreviated hashes are ambiguous remotely; pass a branch name or the full 64-character commit hash"
        )));
    } else {
        branch_list_requests += 1;
        let branches = crate::commands::branch::fetch_remote_branches(remote).await?;
        let branch = branches
            .iter()
            .find(|branch| branch.name == revision)
            .ok_or_else(|| OakError::BranchNotFound(revision.to_string()))?;
        let head = branch
            .head
            .as_deref()
            .ok_or_else(|| OakError::Server(format!("remote branch '{revision}' has no head")))?;
        (Hash::from_hex(head)?, "remote_branch_list")
    };
    let mut metadata_budget = ReviewPreparationBudget::new();
    crate::commands::blob_fetch::ensure_review_commits_with_budget(
        repo,
        &remote.remote_url,
        &remote.owner,
        &remote.repo_name,
        remote.token.as_deref(),
        std::slice::from_ref(&commit),
        &mut metadata_budget,
    )
    .await?;
    let manifest = crate::commands::review::manifest_for_head(repo, Some(&commit))?;
    let mut content_budget = ReviewPreparationBudget::with_limits(
        usize::try_from(max_bytes).unwrap_or(usize::MAX),
        std::time::Duration::from_secs(30),
    );
    let mut inspection = RemoteFileInspection {
        schema_version: 1,
        kind: "remote_file_inspection",
        remote: format!("{}/{}", remote.owner, remote.repo_name),
        requested_revision: revision.to_string(),
        head_source,
        commit: commit.to_string(),
        path: path.to_string(),
        blob: None,
        mode: None,
        status: RemoteFileStatus::PathMissing,
        reason: None,
        content_source: None,
        verified_size: None,
        content: None,
        content_omitted_reason: None,
        max_bytes,
        acquisition: Default::default(),
        recommended_next_commands: Vec::new(),
    };
    let mut bytes = None;
    let mut known_size: Option<u64> = None;
    match manifest.get(path) {
        None => {
            inspection.reason = Some(format!("'{path}' is not a file in commit {commit}"));
            if manifest.entries.iter().any(|entry| {
                entry
                    .path
                    .starts_with(&format!("{}/", path.trim_end_matches('/')))
            }) {
                inspection.status = RemoteFileStatus::NotAFile;
                inspection.reason = Some(format!("'{path}' is a directory in commit {commit}"));
            }
        }
        Some(entry) => {
            inspection.blob = Some(entry.blob_hash.to_string());
            inspection.mode = Some(entry.mode);
            let cached = if oak_core::ensure_empty_blob(repo, &entry.blob_hash)? {
                Some(Vec::new())
            } else {
                repo.get_blob(&entry.blob_hash)?
                    .map(|blob| blob.content)
                    .filter(|content| oak_core::hash_bytes(content) == entry.blob_hash)
            };
            let marked = crate::commands::restricted::load_restricted_blobs(repo)
                .contains(entry.blob_hash.as_str())
                || crate::commands::known_loss::load_known_lost_blobs(repo)
                    .contains(entry.blob_hash.as_str());
            match cached {
                // Markers are not proof of content and never authorize a read.
                None if marked => {
                    inspection.status = RemoteFileStatus::Unavailable;
                    inspection.reason = Some(
                        "blob is marked restricted or known-lost locally; no remote read was attempted"
                            .to_string(),
                    );
                }
                Some(content) if content.len() as u64 <= max_bytes => {
                    inspection.content_source = Some("local_cache");
                    content_budget.record_reused_objects(1);
                    bytes = Some(content);
                }
                Some(content) => {
                    known_size = Some(content.len() as u64);
                    inspection.status = RemoteFileStatus::BudgetExceeded;
                    inspection.reason = Some(format!(
                        "file is {} bytes, over --max-bytes {max_bytes}",
                        content.len()
                    ));
                }
                None => {
                    let client = crate::http::api_client();
                    match read_raw_verified(
                        &client,
                        remote,
                        &commit,
                        path,
                        &entry.blob_hash,
                        &mut content_budget,
                    )
                    .await
                    {
                        Ok(content) => {
                            repo.put_blob(content.clone())?;
                            inspection.content_source = Some("remote_raw");
                            bytes = Some(content);
                        }
                        Err(failure) => {
                            inspection.status = match failure {
                                RawReadFailure::InvalidPath => RemoteFileStatus::InvalidPath,
                                RawReadFailure::Budget => RemoteFileStatus::BudgetExceeded,
                                RawReadFailure::Denied => RemoteFileStatus::AccessDenied,
                                RawReadFailure::NotFound => RemoteFileStatus::NotFound,
                                RawReadFailure::Unavailable => RemoteFileStatus::Unavailable,
                                RawReadFailure::HashMismatch => RemoteFileStatus::HashMismatch,
                            };
                            inspection.reason =
                                Some(format!("remote raw read failed: {}", failure.as_str()));
                        }
                    }
                }
            }
        }
    }
    if let Some(content) = bytes.as_ref() {
        inspection.status = RemoteFileStatus::Verified;
        inspection.verified_size = Some(content.len());
        match std::str::from_utf8(content) {
            Ok(text) if !content.contains(&0) => inspection.content = Some(text.to_string()),
            _ => inspection.content_omitted_reason = Some("binary"),
        }
    }
    let mut acquisition = metadata_budget.acquisition();
    acquisition.branch_list_requests = branch_list_requests;
    let content = content_budget.acquisition();
    acquisition.raw_requests += content.raw_requests;
    acquisition.objects_verified += content.objects_verified;
    acquisition.objects_reused += content.objects_reused;
    acquisition.bytes_downloaded += content.bytes_downloaded;
    acquisition.budget_exhausted = acquisition.budget_exhausted.or(content.budget_exhausted);
    inspection.acquisition = acquisition;
    // Suggest a larger byte budget only when one exists that could succeed:
    // never for a time-budget miss, never at the maximum, never past it.
    let max = oak_core::sqlite::file_inspect::MAX_FILE_INSPECTION_BYTES;
    let larger = match (inspection.status, known_size, content.budget_exhausted) {
        (RemoteFileStatus::BudgetExceeded, Some(size), _) if size <= max => Some(size),
        (RemoteFileStatus::BudgetExceeded, None, Some("bytes")) if max_bytes < max => Some(max),
        _ => None,
    };
    if let Some(larger) = larger {
        inspection.recommended_next_commands.push(format!(
            "oak file inspect --remote --at {} {} --max-bytes {larger} --json",
            commit,
            crate::commands::review::shell_quote_path(path),
        ));
    }
    let verified = inspection.status == RemoteFileStatus::Verified;
    if json {
        crate::output::print_json(&inspection)?;
    } else if let Some(content) = bytes {
        use std::io::Write;
        // Exact bytes, not lossy text. A vanished reader (`| head`) silences
        // output like every other printer instead of failing the command.
        let mut stdout = std::io::stdout().lock();
        match stdout.write_all(&content).and_then(|()| stdout.flush()) {
            Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
                return Err(error.into())
            }
            _ => {}
        }
        eprintln!(
            "oak: {path} at {commit} ({head_source}), blob {} verified",
            inspection.blob.as_deref().unwrap_or("")
        );
    } else {
        let status = serde_json::to_value(inspection.status)?;
        eprintln!(
            "oak: {}: {path} at {commit}{}",
            status.as_str().unwrap_or("unavailable"),
            inspection
                .reason
                .as_deref()
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default()
        );
    }
    Ok(verified)
}
