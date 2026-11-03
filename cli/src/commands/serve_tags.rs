//! `oak serve` parity for the hosted tags/releases API (`release_protocol`
//! v1): the same routes and JSON shapes as oak.space, kept small. Tags live
//! in the served repo's `tags` / `tag_assets` tables (migration 0017), always
//! enabled, with no per-user draft visibility (`oak serve` has one shared
//! token). An asset's chunks must already be uploaded through the ordinary
//! chunk routes; registering one proves they hash to the blob, records the
//! blob's chunk mapping, and publishes it so `/blobs/info` serves it.

use axum::body::Bytes;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use oak_core::protocol::{ChunkRefData, TagAssetData, TagData};
use oak_core::{ChunkInfo, Hash, LocalRelease, LocalReleaseAsset, Repository, SqliteRepository};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{blocking, lock_writer, open_existing, record_published_blobs, ServeError, ServeState};

const MAX_ASSETS_PER_TAG: usize = 1000;
const MAX_NOTES_BYTES: usize = 128 * 1024;

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn tag_json(owner: &str, name: &str, tag: &LocalRelease) -> Value {
    let segment = urlencoding::encode(&tag.name).into_owned();
    json!({
        "name": tag.name,
        "commit": tag.commit_hash,
        "title": tag.title,
        "notes": tag.notes,
        "is_release": tag.is_release(),
        "draft": tag.published_at.is_none(),
        "prerelease": tag.prerelease,
        "finalized": tag.finalized,
        "created_by": tag.created_by,
        "created_at": tag.created_at,
        "updated_at": tag.created_at,
        "published_at": tag.published_at,
        "finalized_at": Value::Null,
        "revision": tag.revision,
        "assets": tag.assets.iter().map(|a| asset_json(owner, name, &segment, a)).collect::<Vec<_>>(),
    })
}

fn asset_json(owner: &str, name: &str, segment: &str, a: &LocalReleaseAsset) -> Value {
    json!({
        "name": a.name,
        "blob_hash": a.blob_hash,
        "size": a.size,
        "content_type": a.content_type,
        "download_count": 0,
        "created_at": Value::Null,
        "download_path": format!(
            "/api/{owner}/{name}/tags/{segment}/assets/{}",
            urlencoding::encode(&a.name)
        ),
    })
}

fn tag_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": message.into(), "code": code })),
    )
        .into_response()
}

fn missing_tag(tag: &str) -> Response {
    tag_error(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("Tag '{tag}' not found"),
    )
}

/// Every published tag for `/pull?tags=v1`.
pub(super) fn pull_snapshot(repo: &SqliteRepository) -> oak_core::Result<Vec<TagData>> {
    let mut tags: Vec<TagData> = repo
        .list_releases()?
        .into_iter()
        .filter(|t| t.published_at.is_some())
        .map(|t| TagData {
            name: t.name,
            commit: t.commit_hash,
            title: t.title,
            notes: t.notes,
            prerelease: t.prerelease,
            finalized: t.finalized,
            created_by: t.created_by,
            created_at: t.created_at,
            published_at: t.published_at,
            revision: t.revision,
            assets: t
                .assets
                .into_iter()
                .map(|a| TagAssetData {
                    name: a.name,
                    blob_hash: a.blob_hash,
                    size: a.size,
                    content_type: a.content_type,
                })
                .collect(),
        })
        .collect();
    tags.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(tags)
}

/// Exact first-parent walk from `main`'s head.
fn is_on_main(repo: &SqliteRepository, commit: &Hash) -> oak_core::Result<bool> {
    let mut cursor = repo.get_branch_head(oak_core::DEFAULT_BRANCH)?;
    while let Some(hash) = cursor {
        if &hash == commit {
            return Ok(true);
        }
        cursor = repo.get_commit(&hash)?.and_then(|c| c.parent_hash);
    }
    Ok(false)
}

#[derive(Deserialize, Default)]
pub(super) struct ListQuery {
    kind: Option<String>,
    limit: Option<usize>,
}

pub(super) async fn list_tags(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Query(query): Query<ListQuery>,
) -> std::result::Result<Response, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let kind = query.kind.unwrap_or_else(|| "all".into());
        let limit = query.limit.unwrap_or(100).clamp(1, 1000);
        let tags: Vec<Value> = repo
            .list_releases()?
            .iter()
            .filter(|t| match kind.as_str() {
                "release" => t.is_release(),
                "tag" => !t.is_release(),
                _ => true,
            })
            .take(limit)
            .map(|t| tag_json(&owner, &name, t))
            .collect();
        Ok(
            Json(json!({ "schema_version": 1, "repo": format!("{owner}/{name}"), "tags": tags }))
                .into_response(),
        )
    })
    .await
}

pub(super) async fn get_tag(
    State(state): State<ServeState>,
    AxPath((owner, name, tag)): AxPath<(String, String, String)>,
) -> std::result::Result<Response, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        Ok(match repo.get_release(&tag)? {
            Some(t) => Json(json!({
                "schema_version": 1,
                "repo": format!("{owner}/{name}"),
                "tag": tag_json(&owner, &name, &t),
            }))
            .into_response(),
            None => missing_tag(&tag),
        })
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct CreateRequest {
    name: String,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
}

pub(super) async fn create_tag(
    State(state): State<ServeState>,
    AxPath((owner, name)): AxPath<(String, String)>,
    Json(req): Json<CreateRequest>,
) -> std::result::Result<Response, ServeError> {
    let root = state.root.clone();
    let writer = state.writer(&owner, &name);
    blocking(move || {
        let _guard = lock_writer(&writer);
        let repo = open_existing(&root, &owner, &name)?;
        if let Err(e) = oak_core::validate_tag_name(&req.name) {
            return Ok(tag_error(StatusCode::BAD_REQUEST, "invalid_name", e));
        }
        if req
            .notes
            .as_ref()
            .is_some_and(|n| n.len() > MAX_NOTES_BYTES)
        {
            return Err(ServeError::bad_request("notes are too long"));
        }
        let Some(main_head) = repo.get_branch_head(oak_core::DEFAULT_BRANCH)? else {
            return Ok(tag_error(
                StatusCode::CONFLICT,
                "no_main",
                "This repository has no commits on main to tag",
            ));
        };
        let commit = match req.commit.as_deref() {
            None => main_head,
            Some(c) => Hash::from_hex(c).map_err(|e| ServeError::bad_request(e.to_string()))?,
        };
        if !is_on_main(&repo, &commit)? {
            return Ok(tag_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "not_on_main",
                format!("Commit {commit} is not on main; tags can only point at commits on main"),
            ));
        }
        let title = req.title.filter(|t| !t.trim().is_empty());
        let notes = req.notes.filter(|n| !n.is_empty());
        if let Some(existing) = repo.get_release(&req.name)? {
            let same = existing.commit_hash == commit.as_str()
                && existing.title == title
                && existing.notes == notes
                && existing.prerelease == req.prerelease
                && existing.published_at.is_none() == req.draft;
            if same {
                return Ok(Json(json!({
                    "schema_version": 1,
                    "repo": format!("{owner}/{name}"),
                    "created": false,
                    "tag": tag_json(&owner, &name, &existing),
                }))
                .into_response());
            }
            return Ok((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!("Tag '{}' already exists; tags never move", existing.name),
                    "code": "tag_exists",
                    "current": tag_json(&owner, &name, &existing),
                })),
            )
                .into_response());
        }
        let created = now();
        let release = LocalRelease {
            name: req.name,
            commit_hash: commit.to_string(),
            title,
            notes,
            prerelease: req.prerelease,
            finalized: false,
            created_by: "oak-serve".into(),
            published_at: (!req.draft).then(|| created.clone()),
            created_at: created,
            revision: 1,
            synced: true,
            assets: Vec::new(),
        };
        repo.upsert_synced_release(&release)?;
        Ok((
            StatusCode::CREATED,
            Json(json!({
                "schema_version": 1,
                "repo": format!("{owner}/{name}"),
                "created": true,
                "tag": tag_json(&owner, &name, &release),
            })),
        )
            .into_response())
    })
    .await
}

fn present<'de, D, T>(d: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Deserialize, Default)]
pub(super) struct UpdateRequest {
    #[serde(default)]
    expected_revision: Option<i64>,
    #[serde(default, deserialize_with = "present")]
    title: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    notes: Option<Option<String>>,
    #[serde(default)]
    prerelease: Option<bool>,
    #[serde(default)]
    publish: bool,
    #[serde(default)]
    finalize: bool,
}

pub(super) async fn update_tag(
    State(state): State<ServeState>,
    AxPath((owner, name, tag)): AxPath<(String, String, String)>,
    Json(req): Json<UpdateRequest>,
) -> std::result::Result<Response, ServeError> {
    let root = state.root.clone();
    let writer = state.writer(&owner, &name);
    blocking(move || {
        let _guard = lock_writer(&writer);
        let repo = open_existing(&root, &owner, &name)?;
        let Some(mut current) = repo.get_release(&tag)? else {
            return Ok(missing_tag(&tag));
        };
        if req.expected_revision.is_some_and(|r| r != current.revision) {
            return Ok((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!("Tag '{tag}' is at revision {}", current.revision),
                    "code": "revision_mismatch",
                    "current": tag_json(&owner, &name, &current),
                })),
            )
                .into_response());
        }
        if let Some(title) = req.title {
            current.title = title.filter(|t| !t.trim().is_empty());
        }
        if let Some(notes) = req.notes {
            current.notes = notes.filter(|n| !n.is_empty());
        }
        if let Some(p) = req.prerelease {
            current.prerelease = p;
        }
        if req.publish && current.published_at.is_none() {
            current.published_at = Some(now());
        }
        if req.finalize {
            current.finalized = true;
        }
        current.revision += 1;
        repo.upsert_synced_release(&current)?;
        Ok(Json(json!({
            "schema_version": 1,
            "repo": format!("{owner}/{name}"),
            "tag": tag_json(&owner, &name, &current),
        }))
        .into_response())
    })
    .await
}

pub(super) async fn delete_tag(
    State(state): State<ServeState>,
    AxPath((owner, name, tag)): AxPath<(String, String, String)>,
) -> std::result::Result<Response, ServeError> {
    let root = state.root.clone();
    let writer = state.writer(&owner, &name);
    blocking(move || {
        let _guard = lock_writer(&writer);
        let repo = open_existing(&root, &owner, &name)?;
        let Some(current) = repo.get_release(&tag)? else {
            return Ok(missing_tag(&tag));
        };
        repo.remove_release(&tag)?;
        Ok(Json(json!({
            "schema_version": 1,
            "repo": format!("{owner}/{name}"),
            "deleted": tag_json(&owner, &name, &current),
        }))
        .into_response())
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct AssetBlob {
    hash: String,
    size: u64,
    chunks: Vec<ChunkRefData>,
}

#[derive(Deserialize)]
pub(super) struct AddAssetRequest {
    name: String,
    #[serde(default)]
    content_type: Option<String>,
    blob: AssetBlob,
}

/// Prove the uploaded chunks are exactly the declared blob, then record its
/// mapping. Errors carry the hosted server's status codes.
#[allow(clippy::result_large_err)]
fn admit_asset_blob(
    repo: &SqliteRepository,
    blob: &AssetBlob,
) -> std::result::Result<(), Response> {
    let bad = |m: String| tag_error(StatusCode::BAD_REQUEST, "invalid_blob", m);
    let blob_hash = Hash::from_hex(&blob.hash).map_err(|e| bad(e.to_string()))?;
    if blob.chunks.is_empty() {
        return Err(bad("blob must list its chunks".into()));
    }
    let mut hasher = blake3::Hasher::new();
    let mut offset = 0u64;
    let mut infos = Vec::with_capacity(blob.chunks.len());
    for chunk in &blob.chunks {
        let hash = Hash::from_hex(&chunk.hash).map_err(|e| bad(e.to_string()))?;
        if chunk.offset != offset || chunk.size == 0 {
            return Err(bad(format!("chunk {} is not contiguous", chunk.hash)));
        }
        let bytes = repo
            .get_chunk(&hash)
            .map_err(|e| tag_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?
            .ok_or_else(|| {
                tag_error(
                    StatusCode::CONFLICT,
                    "chunk_missing",
                    format!("chunk {hash} has not been uploaded; upload it and retry"),
                )
            })?;
        if bytes.len() != chunk.size as usize {
            return Err(bad(format!(
                "chunk {hash} is {} bytes, not {}",
                bytes.len(),
                chunk.size
            )));
        }
        hasher.update(&bytes);
        offset += u64::from(chunk.size);
        infos.push(ChunkInfo {
            hash,
            offset: chunk.offset,
            length: chunk.size,
        });
    }
    if offset != blob.size || hasher.finalize().to_hex().as_str() != blob_hash.as_str() {
        return Err(bad(format!(
            "the uploaded chunks do not hash to blob {}",
            blob.hash
        )));
    }
    let internal = |e: oak_core::OakError| {
        tag_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
    };
    if repo
        .get_blob_chunks(&blob_hash)
        .map_err(internal)?
        .is_none()
    {
        repo.store_blob_chunks(&blob_hash, &infos)
            .map_err(internal)?;
    }
    record_published_blobs(repo, std::slice::from_ref(&blob.hash)).map_err(internal)?;
    Ok(())
}

pub(super) async fn add_asset(
    State(state): State<ServeState>,
    AxPath((owner, name, tag)): AxPath<(String, String, String)>,
    Json(req): Json<AddAssetRequest>,
) -> std::result::Result<Response, ServeError> {
    let root = state.root.clone();
    let writer = state.writer(&owner, &name);
    blocking(move || {
        let _guard = lock_writer(&writer);
        let repo = open_existing(&root, &owner, &name)?;
        let Some(mut current) = repo.get_release(&tag)? else {
            return Ok(missing_tag(&tag));
        };
        if req.name.is_empty()
            || req.name.contains(['/', '\\'])
            || req.name.trim_start_matches('.').is_empty()
        {
            return Ok(tag_error(
                StatusCode::BAD_REQUEST,
                "invalid_name",
                "asset name must be a plain file name",
            ));
        }
        let segment = urlencoding::encode(&tag).into_owned();
        if let Some(existing) = current.assets.iter().find(|a| a.name == req.name) {
            if existing.blob_hash == req.blob.hash {
                return Ok(Json(json!({
                    "schema_version": 1,
                    "tag": tag,
                    "added": false,
                    "asset": asset_json(&owner, &name, &segment, existing),
                }))
                .into_response());
            }
            return Ok(tag_error(
                StatusCode::CONFLICT,
                "asset_exists",
                format!(
                    "Tag '{tag}' already has an asset named '{}'; delete it first",
                    req.name
                ),
            ));
        }
        if current.finalized {
            return Ok(tag_error(
                StatusCode::CONFLICT,
                "finalized",
                format!("Tag '{tag}' is finalized"),
            ));
        }
        if current.assets.len() >= MAX_ASSETS_PER_TAG {
            return Ok(tag_error(
                StatusCode::CONFLICT,
                "too_many_assets",
                "too many assets",
            ));
        }
        if let Err(response) = admit_asset_blob(&repo, &req.blob) {
            return Ok(response);
        }
        let asset = LocalReleaseAsset {
            name: req.name,
            blob_hash: req.blob.hash,
            size: req.blob.size,
            content_type: req
                .content_type
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "application/octet-stream".into()),
        };
        current.assets.push(asset.clone());
        current.revision += 1;
        repo.upsert_synced_release(&current)?;
        Ok((
            StatusCode::CREATED,
            Json(json!({
                "schema_version": 1,
                "tag": tag,
                "added": true,
                "asset": asset_json(&owner, &name, &segment, &asset),
            })),
        )
            .into_response())
    })
    .await
}

pub(super) async fn delete_asset(
    State(state): State<ServeState>,
    AxPath((owner, name, tag, asset)): AxPath<(String, String, String, String)>,
) -> std::result::Result<Response, ServeError> {
    let root = state.root.clone();
    let writer = state.writer(&owner, &name);
    blocking(move || {
        let _guard = lock_writer(&writer);
        let repo = open_existing(&root, &owner, &name)?;
        let Some(mut current) = repo.get_release(&tag)? else {
            return Ok(missing_tag(&tag));
        };
        if current.finalized {
            return Ok(tag_error(
                StatusCode::CONFLICT,
                "finalized",
                format!("Tag '{tag}' is finalized"),
            ));
        }
        let Some(index) = current.assets.iter().position(|a| a.name == asset) else {
            return Ok(tag_error(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("Tag '{tag}' has no asset named '{asset}'"),
            ));
        };
        let removed = current.assets.remove(index);
        current.revision += 1;
        repo.upsert_synced_release(&current)?;
        let segment = urlencoding::encode(&tag).into_owned();
        Ok(Json(json!({
            "schema_version": 1,
            "tag": tag,
            "added": false,
            "asset": asset_json(&owner, &name, &segment, &removed),
        }))
        .into_response())
    })
    .await
}

pub(super) async fn download_asset(
    State(state): State<ServeState>,
    AxPath((owner, name, tag, asset)): AxPath<(String, String, String, String)>,
) -> std::result::Result<Response, ServeError> {
    let root = state.root.clone();
    blocking(move || {
        let repo = open_existing(&root, &owner, &name)?;
        let Some(current) = repo.get_release(&tag)? else {
            return Ok(missing_tag(&tag));
        };
        let Some(entry) = current.assets.iter().find(|a| a.name == asset) else {
            return Ok(tag_error(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("Tag '{tag}' has no asset named '{asset}'"),
            ));
        };
        let hash = Hash::from_hex(&entry.blob_hash)?;
        let Some(bytes) = super::read_blob_content(&repo, &hash)? else {
            return Ok(tag_error(
                StatusCode::GONE,
                "content_missing",
                "asset content is missing",
            ));
        };
        Ok((
            [
                (header::CONTENT_TYPE, entry.content_type.clone()),
                (
                    header::CONTENT_DISPOSITION,
                    format!(
                        "attachment; filename*=UTF-8''{}",
                        urlencoding::encode(&entry.name)
                    ),
                ),
            ],
            Bytes::from(bytes),
        )
            .into_response())
    })
    .await
}
