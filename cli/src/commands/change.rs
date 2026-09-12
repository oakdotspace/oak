//! Explicit, local-only capture of immutable working-tree change objects.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use oak_core::{
    hash_bytes, CanonicalChangeSetV1, CanonicalPathChangeV1, ChangeScopeV1, Hash, HashFormat,
    IgnorePatterns, Manifest, ManifestEntry, MetadataKey, OakError, Repository, Result, SparseCone,
    SqliteRepository, StoredChangeCapture,
};
use serde::Serialize;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::resolve::Backend;

#[derive(Debug, Serialize)]
struct RepositoryAuthority {
    origin: Option<String>,
    owner: String,
    name: String,
}

#[derive(Debug, Serialize)]
struct CaptureBase {
    branch: String,
    commit: Option<Hash>,
    tree: Hash,
}

#[derive(Debug, Serialize)]
struct CaptureConsistency {
    model: &'static str,
    oak_writers_excluded: bool,
    external_writers_excluded: bool,
    stability_checks: &'static str,
}

#[derive(Debug, Serialize)]
struct CaptureStorage {
    blobs: &'static str,
    trees: &'static str,
    refs_changed: bool,
    commits_created: bool,
    worktree_changed: bool,
    remote_contacted: bool,
}

#[derive(Debug, Serialize)]
struct CaptureProofScope {
    complete_change_identity: bool,
    selected_new_blob_rows: &'static str,
    result_tree_closure: &'static str,
    unmodified_inherited_blob_rows: &'static str,
    inherited_missing_objects_possible: bool,
}

const RECEIPT_SUMMARY_LIMIT: usize = 64;

#[derive(Debug, Serialize)]
struct ExportCapture<'a> {
    capture_id: &'a str,
    repository: RepositoryAuthority,
    source_branch: &'a str,
    base_commit: Option<&'a Hash>,
    captured_at: &'a chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
struct ExportBlobPlan {
    hash: Hash,
    logical_size: u64,
}

#[derive(Debug, Serialize)]
struct ExportPayload<'a> {
    blob_count: u64,
    logical_bytes: u64,
    blobs: &'a [ExportBlobPlan],
}

#[derive(Debug, Serialize)]
struct ExportManifest<'a> {
    schema_version: u8,
    kind: &'static str,
    capture: ExportCapture<'a>,
    change_set: &'a CanonicalChangeSetV1,
    payload: ExportPayload<'a>,
}

#[derive(Debug, Serialize)]
struct ExportReceipt<'a> {
    schema_version: u8,
    kind: &'static str,
    capture_id: &'a str,
    change_id: &'a Hash,
    output: String,
    blob_count: u64,
    logical_bytes: u64,
    archive_bytes: u64,
    remote_contacted: bool,
}

/// Export one durable local capture without contacting a remote or mutating the repository.
pub fn export(path: &Path, capture_id: &str, output: &Path, json: bool) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    if !matches!(ctx.backend, Backend::Sqlite) {
        return Err(OakError::Unsupported {
            backend: "git",
            op: "change export",
        });
    }
    let db_path = ctx.db_path()?;
    let repo = SqliteRepository::open_read_only(&db_path)?;
    match capture_schema_state(&db_path)? {
        CaptureSchemaState::Current => {}
        CaptureSchemaState::PreCapture => {
            return Err(OakError::InvalidArgument(
                "this repository schema predates durable change captures; use a compatible current Oak binary and rerun `oak change capture --json` before exporting"
                    .to_string(),
            ));
        }
        CaptureSchemaState::Inconsistent => {
            return Err(OakError::ChangeCaptureIntegrity {
                capture_id: capture_id.to_string(),
                detail: "the durable change-capture schema is incomplete or inconsistent; inspect or repair it with a compatible current Oak binary before retrying export"
                    .to_string(),
            });
        }
    }
    let capture = repo.get_change_capture(capture_id)?;
    let (blobs, blob_count, logical_bytes) = plan_export_blobs(&repo, &capture)?;
    let manifest = ExportManifest {
        schema_version: 1,
        kind: "oak_change_export",
        capture: ExportCapture {
            capture_id: &capture.capture_id,
            repository: RepositoryAuthority {
                owner: capture.provenance.owner.clone(),
                name: capture.provenance.name.clone(),
                origin: capture.provenance.origin.clone(),
            },
            source_branch: &capture.provenance.source_branch,
            base_commit: capture.provenance.base_commit.as_ref(),
            captured_at: &capture.provenance.captured_at,
        },
        change_set: &capture.change_set,
        payload: ExportPayload {
            blob_count,
            logical_bytes,
            blobs: &blobs,
        },
    };
    let manifest_json = serde_json::to_vec(&manifest)?;

    crate::atomic_file::write_atomic_private_noclobber(output, |file| {
        write_export_archive(file, &repo, capture_id, &manifest_json, &blobs)
    })?;
    let archive_bytes = std::fs::metadata(output)?.len();
    let receipt = ExportReceipt {
        schema_version: 1,
        kind: "oak_change_export",
        capture_id: &capture.capture_id,
        change_id: capture.change_set.id(),
        output: output.display().to_string(),
        blob_count,
        logical_bytes,
        archive_bytes,
        remote_contacted: false,
    };
    if json {
        crate::output::print_json(&receipt)?;
    } else {
        crate::output::success(&format!(
            "Exported capture {} (change {}) to {}",
            capture.capture_id,
            capture.change_set.id(),
            output.display()
        ));
    }
    Ok(())
}

fn plan_export_blobs(
    repo: &SqliteRepository,
    capture: &StoredChangeCapture,
) -> Result<(Vec<ExportBlobPlan>, u64, u64)> {
    let mut hashes = BTreeMap::new();
    for change in capture.change_set.changes() {
        for state in [change.before.as_ref(), change.after.as_ref()]
            .into_iter()
            .flatten()
        {
            hashes.insert(state.blob_hash.to_string(), state.blob_hash.clone());
        }
    }
    let mut logical_bytes = 0_u64;
    let mut blobs = Vec::with_capacity(hashes.len());
    for (_, hash) in hashes {
        let logical_size = match repo.get_blob_size(&hash)? {
            Some(size) => size,
            None if hash == hash_bytes(&[]) => 0,
            None => return Err(missing_export_blob(repo, &capture.capture_id, &hash)),
        };
        logical_bytes = logical_bytes.checked_add(logical_size).ok_or_else(|| {
            OakError::InvalidArgument("export logical byte count overflowed u64".to_string())
        })?;
        blobs.push(ExportBlobPlan { hash, logical_size });
    }
    let blob_count = u64::try_from(blobs.len())
        .map_err(|_| OakError::InvalidArgument("export blob count overflowed u64".to_string()))?;
    Ok((blobs, blob_count, logical_bytes))
}

fn write_export_archive(
    file: &mut std::fs::File,
    repo: &SqliteRepository,
    capture_id: &str,
    manifest_json: &[u8],
    blobs: &[ExportBlobPlan],
) -> Result<()> {
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .large_file(true);
    let mut archive = ZipWriter::new(file);
    archive
        .start_file("manifest.json", options)
        .map_err(zip_io)?;
    archive.write_all(manifest_json)?;
    for planned in blobs {
        let blob = match repo.get_blob(&planned.hash)? {
            Some(blob) => blob,
            None if planned.hash == hash_bytes(&[]) => oak_core::Blob::new(Vec::new()),
            None => return Err(missing_export_blob(repo, capture_id, &planned.hash)),
        };
        let actual_size = u64::try_from(blob.content.len()).map_err(|_| {
            export_blob_integrity(&planned.hash, "logical content length overflowed u64")
        })?;
        if blob.size != actual_size || actual_size != planned.logical_size {
            return Err(export_blob_integrity(
                &planned.hash,
                "stored, declared, and decoded logical sizes differ",
            ));
        }
        if hash_bytes(&blob.content) != planned.hash {
            return Err(export_blob_integrity(
                &planned.hash,
                "decoded logical bytes do not match the referenced BLAKE3 hash",
            ));
        }
        archive
            .start_file(format!("blobs/{}", planned.hash), options)
            .map_err(zip_io)?;
        archive.write_all(&blob.content)?;
    }
    archive.finish().map_err(zip_io)?;
    Ok(())
}

fn missing_export_blob(repo: &SqliteRepository, capture_id: &str, hash: &Hash) -> OakError {
    if super::restricted::load_restricted_blobs(repo).contains(hash.as_str()) {
        return OakError::RestrictedContent(format!(
            "capture {capture_id} references restricted blob {hash}; {}",
            super::restricted::ACCESS_HINT
        ));
    }
    let context = if super::known_loss::load_known_lost_blobs(repo).contains(hash.as_str()) {
        format!(
            "change export {capture_id}; blob is marked as {}",
            super::known_loss::OPERATOR_LOSS_REASON
        )
    } else {
        format!("change export {capture_id} from local durable capture")
    };
    OakError::IncompleteBlobData {
        context,
        missing: hash.to_string(),
    }
}

fn export_blob_integrity(hash: &Hash, detail: &str) -> OakError {
    OakError::CapturedBlobIntegrity {
        path: "change export payload".to_string(),
        hash: hash.to_string(),
        detail: detail.to_string(),
    }
}

fn zip_io(error: zip::result::ZipError) -> OakError {
    OakError::Io(io::Error::other(error))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureSchemaState {
    Current,
    PreCapture,
    Inconsistent,
}

fn capture_schema_state(db_path: &Path) -> Result<CaptureSchemaState> {
    let connection =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| OakError::Database(error.to_string()))?;
    let table_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name IN ('change_sets', 'change_captures')",
        [],
        |row| row.get(0),
    )
    .map_err(|error| OakError::Database(error.to_string()))?;
    let migration_applied: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = '0014_change_captures' AND succeeded = 1)",
            [],
            |row| row.get(0),
        )
        .map_err(|error| OakError::Database(error.to_string()))?;
    Ok(match (migration_applied, table_count) {
        (true, 2) => CaptureSchemaState::Current,
        (false, 0) => CaptureSchemaState::PreCapture,
        _ => CaptureSchemaState::Inconsistent,
    })
}

#[derive(Debug, Serialize)]
struct CaptureScopeSummary<'a> {
    kind: &'static str,
    path_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    paths: Vec<&'a str>,
    paths_omitted: usize,
    paths_complete: bool,
    identity_covers_complete_scope: bool,
    selection_semantics: &'static str,
    effective: CaptureEffectiveScope,
}

#[derive(Debug, Serialize)]
struct CaptureEffectiveScope {
    sparse_cone_active: bool,
    sparse_prefix_count: usize,
    restricted_base_paths_excluded: usize,
    known_loss_base_paths_excluded: usize,
    unavailable_base_paths_excluded: usize,
    unavailable_base_paths_present: usize,
    unavailable_path_policy: &'static str,
    selected_working_tree_paths: usize,
}

#[derive(Debug, Serialize)]
struct CaptureChangeSetSummary<'a> {
    schema_version: u8,
    object_format: &'a str,
    id: &'a Hash,
    base_tree: &'a Hash,
    result_tree: &'a Hash,
    change_count: usize,
    changes: &'a [CanonicalPathChangeV1],
    changes_omitted: usize,
    changes_complete: bool,
    identity_covers: &'static str,
}

#[derive(Debug, Serialize)]
struct CaptureReceipt<'a> {
    schema_version: u8,
    kind: &'static str,
    capture_id: &'a str,
    lookup: &'static str,
    repository: RepositoryAuthority,
    base: CaptureBase,
    scope: CaptureScopeSummary<'a>,
    change_set: CaptureChangeSetSummary<'a>,
    proof_scope: CaptureProofScope,
    consistency: CaptureConsistency,
    storage: CaptureStorage,
    warnings: Vec<String>,
}

/// Capture the selected working-tree changes as durable local blobs and tree
/// objects, without creating a commit or advancing any ref.
pub fn capture(path: &Path, paths: &[PathBuf], json: bool) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    if !matches!(ctx.backend, Backend::Sqlite) {
        return Err(OakError::Unsupported {
            backend: "git",
            op: "change capture",
        });
    }
    let _lock =
        crate::workdir_lock::WorkdirLock::acquire_wait(&ctx.oak_dir, Duration::from_secs(2))?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;

    let format = HashFormat::from_metadata(repo.get_metadata(MetadataKey::HashFormat)?.as_deref())?;
    if format != HashFormat::V1 {
        return Err(OakError::UnsupportedChangeCaptureFormat(
            format.as_str().to_string(),
        ));
    }

    let (owner, name) = super::read_repo_identity(&repo)?;
    let origin = sanitize_origin(repo.get_metadata(MetadataKey::RemoteUrl)?);
    let branch = repo
        .get_current_branch_name()?
        .ok_or_else(|| OakError::BranchNotFound("no current branch set".to_string()))?;
    let base_commit = super::commit::resolve_effective_head(&repo, &branch)?;
    let base_manifest = manifest_for_capture_base(&repo, base_commit.as_ref(), &branch)?;

    // A root selector covers the full tree but remains an explicit request.
    // Do not erase that intent when normalized filters become empty.
    let explicit_selection = !paths.is_empty();
    let filters = resolve_capture_filters(path, &ctx.work_tree, paths)?;
    let scope = if filters.is_empty() {
        ChangeScopeV1::Full
    } else {
        ChangeScopeV1::Paths {
            paths: filters.clone(),
        }
    };
    // Validate the base identities and requested scope before capture can
    // persist any file bytes or result tree.
    CanonicalChangeSetV1::from_manifests(&base_manifest, &base_manifest, scope.clone())?;
    // Snapshot all materialization exclusions before the first durable write.
    // A missing base path outside the sparse cone, or one whose bytes are
    // explicitly unavailable, remains part of the result even if another
    // selected path happens to have the same content identity.
    let sparse = SparseCone::from_metadata(repo.get_metadata(MetadataKey::SparsePaths)?.as_deref());
    let restricted: HashSet<String> =
        super::restricted::restricted_paths_in_manifest(&repo, &base_manifest)
            .into_iter()
            .collect();
    let known_loss: HashSet<String> =
        super::known_loss::known_lost_paths_in_manifest(&repo, &base_manifest)
            .into_iter()
            .collect();
    let unavailable: HashSet<String> = restricted.union(&known_loss).cloned().collect();
    // Base markers remain authoritative for this capture even if replacement
    // bytes now exist in the worktree. Inspect presence before any blob write;
    // an explicit request must not acknowledge excluded replacement bytes as
    // a successful empty capture. This does not recover or grant access.
    let mut excluded_paths: Vec<&String> = unavailable.iter().collect();
    excluded_paths.sort();
    let mut present_unavailable = HashSet::new();
    for candidate in excluded_paths {
        if !filters.is_empty() && !super::diff::path_matches(candidate, &filters) {
            continue;
        }
        match std::fs::symlink_metadata(ctx.work_tree.join(candidate)) {
            Ok(_) => {
                if explicit_selection {
                    let (reason, remedy) = if restricted.contains(candidate) {
                        (
                            "the base blob is restricted and unavailable locally",
                            "Ask an org admin for access, or select other paths. Capture does not grant access or clear restriction markers.",
                        )
                    } else {
                        (
                            "the base blob is recorded as known lost and unavailable locally",
                            "Select other paths and preserve these replacement bytes. Capture does not resolve known-loss markers or recover content.",
                        )
                    };
                    return Err(OakError::ChangeCaptureScopeExcluded {
                        path: candidate.clone(),
                        reason: reason.to_string(),
                        remedy: remedy.to_string(),
                    });
                }
                present_unavailable.insert(candidate.clone());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let requested_and_materialized = |candidate: &str| {
        (filters.is_empty() || super::diff::path_matches(candidate, &filters))
            && sparse.as_ref().is_none_or(|cone| cone.covers(candidate))
    };
    let restricted_excluded = restricted
        .iter()
        .filter(|path| requested_and_materialized(path))
        .count();
    let known_loss_excluded = known_loss
        .iter()
        .filter(|path| requested_and_materialized(path))
        .count();
    let unavailable_excluded = unavailable
        .iter()
        .filter(|path| requested_and_materialized(path))
        .count();
    let unavailable_present = present_unavailable
        .iter()
        .filter(|path| requested_and_materialized(path))
        .count();
    let selected =
        |candidate: &str| requested_and_materialized(candidate) && !unavailable.contains(candidate);
    let ignore = IgnorePatterns::new(&ctx.work_tree)?;
    let captured_entries =
        super::commit::scan_working_dir_for_capture(&ctx.work_tree, &repo, &ignore, &selected)?;
    verify_captured_blobs(&repo, &captured_entries)?;
    let selected_working_tree_paths = captured_entries.len();
    let captured_paths: HashSet<&str> = captured_entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    let mut result: BTreeMap<String, ManifestEntry> = base_manifest
        .entries
        .iter()
        .cloned()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    for entry in &base_manifest.entries {
        if selected(&entry.path) && !captured_paths.contains(entry.path.as_str()) {
            result.remove(&entry.path);
        }
    }
    for entry in captured_entries {
        result.insert(entry.path.clone(), entry);
    }
    let result_entries: Vec<ManifestEntry> = result.into_values().collect();
    let result_tree = repo.put_tree(result_entries.clone())?;
    verify_captured_tree(&repo, &result_tree, &result_entries)?;
    let result_manifest = Manifest {
        hash: result_tree,
        entries: result_entries,
    };
    let change_set = CanonicalChangeSetV1::from_manifests(&base_manifest, &result_manifest, scope)?;

    let observed_head = super::commit::resolve_effective_head(&repo, &branch)?;
    if observed_head != base_commit {
        return Err(OakError::WorktreeCaptureRaced {
            detail: format!(
                "effective HEAD for branch {branch:?} moved from {} to {}",
                display_optional_hash(base_commit.as_ref()),
                display_optional_hash(observed_head.as_ref())
            ),
        });
    }

    let capture_id = format!("capture-{}", uuid::Uuid::new_v4().simple());
    repo.store_change_capture(
        &capture_id,
        &change_set,
        &owner,
        &name,
        origin.as_deref(),
        &branch,
        base_commit.as_ref(),
    )?;

    let receipt = CaptureReceipt {
        schema_version: 1,
        kind: "working_tree_change_capture",
        capture_id: &capture_id,
        lookup: "durable_local_sqlite",
        repository: RepositoryAuthority {
            origin,
            owner,
            name,
        },
        base: CaptureBase {
            branch,
            commit: base_commit,
            tree: base_manifest.hash,
        },
        scope: summarize_scope(
            change_set.scope(),
            sparse.as_ref(),
            restricted_excluded,
            known_loss_excluded,
            unavailable_excluded,
            unavailable_present,
            selected_working_tree_paths,
        ),
        change_set: summarize_change_set(&change_set),
        proof_scope: CaptureProofScope {
            complete_change_identity: true,
            selected_new_blob_rows: "content_hash_and_declared_size_verified",
            result_tree_closure: "read_back_and_canonical_entries_verified",
            unmodified_inherited_blob_rows: "not_read_not_hydrated_not_verified",
            inherited_missing_objects_possible: true,
        },
        consistency: CaptureConsistency {
            model: "non_atomic_working_tree_capture",
            oak_writers_excluded: true,
            external_writers_excluded: false,
            stability_checks: "per_file_before_after_and_inventory_recheck",
        },
        storage: CaptureStorage {
            blobs: "durable_local_sqlite",
            trees: "durable_local_sqlite",
            refs_changed: false,
            commits_created: false,
            worktree_changed: false,
            remote_contacted: false,
        },
        warnings: exclusion_warnings(
            restricted_excluded,
            known_loss_excluded,
            unavailable_present,
            sparse.is_some(),
        ),
    };

    if json {
        crate::output::print_json(&receipt)?;
    } else {
        for warning in &receipt.warnings {
            crate::output::warning(warning);
        }
        crate::output::success(&format!("Captured change set {}", change_set.id()));
    }
    Ok(())
}

fn manifest_for_capture_base(
    repo: &dyn Repository,
    head: Option<&Hash>,
    branch: &str,
) -> Result<Manifest> {
    let Some(head) = head else {
        return Ok(Manifest::empty());
    };
    let commit = repo
        .get_commit(head)?
        .ok_or_else(|| OakError::IncompleteCommitData {
            context: format!("capture base for branch {branch:?}"),
            missing: head.short().to_string(),
        })?;
    repo.get_manifest(&commit.manifest_hash)?
        .ok_or_else(|| OakError::IncompleteManifestData {
            left: format!("capture base for branch {branch:?}"),
            right: "working tree".to_string(),
            missing: commit.manifest_hash.short().to_string(),
        })
}

/// Resolve capture paths lexically so selecting a symlink records the link
/// itself instead of following it to a target path. The cwd and repository
/// root may be canonicalized independently; the user-provided suffix is not.
fn resolve_capture_filters(cwd: &Path, work_tree: &Path, paths: &[PathBuf]) -> Result<Vec<String>> {
    // Validate every input before lexical normalization can discard a path
    // component or a root selection can short-circuit the remaining inputs.
    for input in paths {
        super::commit::capture_path_string(input)?;
    }
    let root = std::fs::canonicalize(work_tree).unwrap_or_else(|_| work_tree.to_path_buf());
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut filters = Vec::with_capacity(paths.len());
    let mut includes_root = false;
    for input in paths {
        let absolute = if input.is_absolute() {
            input.clone()
        } else {
            cwd.join(input)
        };
        let absolute = normalize_lexically(&absolute);
        let relative = absolute.strip_prefix(&root).map_err(|_| {
            OakError::InvalidPath(format!(
                "capture path {:?} is not inside the repository",
                input
            ))
        })?;
        let relative = super::commit::capture_path_string(relative)?;
        if relative.is_empty() {
            includes_root = true;
            continue;
        }
        oak_core::validate_tree_path(&relative)?;
        filters.push(relative);
    }
    // Validate all selectors even when their union already includes the root.
    if includes_root {
        return Ok(Vec::new());
    }
    filters.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    filters.dedup();
    Ok(filters)
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn display_optional_hash(hash: Option<&Hash>) -> &str {
    hash.map(Hash::as_str).unwrap_or("none")
}

fn summarize_scope<'a>(
    scope: &'a ChangeScopeV1,
    sparse: Option<&SparseCone>,
    restricted_count: usize,
    known_loss_count: usize,
    unavailable_count: usize,
    unavailable_present: usize,
    selected_working_tree_paths: usize,
) -> CaptureScopeSummary<'a> {
    let effective = CaptureEffectiveScope {
        sparse_cone_active: sparse.is_some(),
        sparse_prefix_count: sparse.map_or(0, |cone| cone.prefixes().len()),
        restricted_base_paths_excluded: restricted_count,
        known_loss_base_paths_excluded: known_loss_count,
        unavailable_base_paths_excluded: unavailable_count,
        unavailable_base_paths_present: unavailable_present,
        unavailable_path_policy:
            "exclude_marked_missing_base_blobs_even_when_worktree_paths_are_present",
        selected_working_tree_paths,
    };
    match scope {
        ChangeScopeV1::Full => CaptureScopeSummary {
            kind: "full",
            path_count: 0,
            paths: Vec::new(),
            paths_omitted: 0,
            paths_complete: true,
            identity_covers_complete_scope: true,
            selection_semantics:
                "requested_paths_intersect_sparse_cone_excluding_unavailable_base_paths",
            effective,
        },
        ChangeScopeV1::Paths { paths } => {
            let shown = paths.len().min(RECEIPT_SUMMARY_LIMIT);
            CaptureScopeSummary {
                kind: "paths",
                path_count: paths.len(),
                paths: paths[..shown].iter().map(String::as_str).collect(),
                paths_omitted: paths.len() - shown,
                paths_complete: shown == paths.len(),
                identity_covers_complete_scope: true,
                selection_semantics:
                    "requested_paths_intersect_sparse_cone_excluding_unavailable_base_paths",
                effective,
            }
        }
    }
}

fn exclusion_warnings(
    restricted: usize,
    known_loss: usize,
    present: usize,
    sparse: bool,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if restricted > 0 {
        warnings.push(format!(
            "Excluded {restricted} restricted base path(s): their marked blobs are unavailable locally. Capture does not grant access or clear restriction markers."
        ));
    }
    if known_loss > 0 {
        warnings.push(format!(
            "Excluded {known_loss} known-loss base path(s): their marked blobs are unavailable locally. Capture does not resolve loss markers or recover content."
        ));
    }
    if present > 0 {
        warnings.push(format!(
            "{present} excluded unavailable base path(s) are present in the working tree; their current bytes were NOT captured."
        ));
    }
    if sparse {
        warnings.push(
            "Capture is limited to the sparse cone; base paths outside it were preserved."
                .to_string(),
        );
    }
    warnings
}

fn summarize_change_set(change_set: &CanonicalChangeSetV1) -> CaptureChangeSetSummary<'_> {
    let changes = change_set.changes();
    let shown = changes.len().min(RECEIPT_SUMMARY_LIMIT);
    CaptureChangeSetSummary {
        schema_version: change_set.schema_version(),
        object_format: change_set.object_format(),
        id: change_set.id(),
        base_tree: change_set.base_tree(),
        result_tree: change_set.result_tree(),
        change_count: changes.len(),
        changes: &changes[..shown],
        changes_omitted: changes.len() - shown,
        changes_complete: shown == changes.len(),
        identity_covers: "complete_change_set",
    }
}

fn sanitize_origin(raw: Option<String>) -> Option<String> {
    const MAX_ORIGIN_BYTES: usize = 2_048;
    let raw = raw?;
    let raw = raw.trim();
    if raw.len() > MAX_ORIGIN_BYTES {
        return None;
    }
    let mut url = reqwest::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    let mut sanitized = url.to_string();
    if url.path() == "/" {
        sanitized.pop();
    }
    Some(sanitized)
}

fn verify_captured_blobs(repo: &dyn Repository, entries: &[ManifestEntry]) -> Result<()> {
    for entry in entries {
        let Some(blob) = repo.get_blob(&entry.blob_hash)? else {
            return Err(OakError::CapturedBlobIntegrity {
                path: entry.path.clone(),
                hash: entry.blob_hash.to_string(),
                detail: "the durable row is missing after capture".to_string(),
            });
        };
        let actual = oak_core::hash_bytes(&blob.content);
        if actual != entry.blob_hash || blob.size != blob.content.len() as u64 {
            return Err(OakError::CapturedBlobIntegrity {
                path: entry.path.clone(),
                hash: entry.blob_hash.to_string(),
                detail: "the durable row's bytes or declared size do not match its identity"
                    .to_string(),
            });
        }
    }
    Ok(())
}

fn verify_captured_tree(
    repo: &dyn Repository,
    result_tree: &Hash,
    expected: &[ManifestEntry],
) -> Result<()> {
    // Verify each expected node, not only its flattened file list: legacy
    // tree rows can carry a wrong storage identity or extra empty subtrees
    // while flattening to the same files. Exact canonical node equality also
    // verifies every subtree link without traversing untrusted stored links.
    let built = oak_core::build_tree(expected)?;
    if &built.root_hash != result_tree {
        return Err(OakError::CapturedTreeIntegrity {
            hash: result_tree.to_string(),
            detail: "the result root does not match the captured manifest".to_string(),
        });
    }
    for expected_node in built.trees {
        let fail = |detail| OakError::CapturedTreeIntegrity {
            hash: expected_node.hash.to_string(),
            detail,
        };
        let stored = repo
            .get_tree(&expected_node.hash)
            .map_err(|error| fail(format!("the durable tree node could not be read: {error}")))?
            .ok_or_else(|| fail("the durable tree node is missing".to_string()))?;
        let bytes = stored.canonical_bytes();
        if oak_core::hash_bytes(&bytes) != expected_node.hash
            || bytes != expected_node.canonical_bytes()
        {
            return Err(fail(
                "the durable tree node's canonical bytes do not match its captured identity"
                    .to_string(),
            ));
        }
    }
    Ok(())
}
