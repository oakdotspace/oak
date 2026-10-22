//! Canonical identity for an immutable set of file-state changes.
//!
//! `CanonicalChangeSetV1` deliberately hashes file identities, not formatted
//! JSON or presentation hunks. Its preimage is domain-separated and every
//! variable-width value is framed with an unsigned 64-bit little-endian byte
//! length. Repository authority (owner/name and the base commit) is kept out
//! of this portable identity and belongs in the receipt that captured it.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::{build_tree, hash_bytes, normalize_path, FileMode, Hash, Manifest, ManifestEntry};
use crate::{OakError, Result};

const DOMAIN: &[u8] = b"oak-canonical-change-set-v1";
pub const CHANGE_SET_OBJECT_FORMAT_V1: &str = "oak_v1_blake3";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeScopeV1 {
    Full,
    Paths { paths: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangeStateV1 {
    pub blob_hash: Hash,
    pub mode: FileMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CanonicalPathChangeV1 {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<ChangeStateV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<ChangeStateV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CanonicalChangeSetV1 {
    schema_version: u8,
    object_format: String,
    id: Hash,
    base_tree: Hash,
    result_tree: Hash,
    scope: ChangeScopeV1,
    changes: Vec<CanonicalPathChangeV1>,
}

impl CanonicalChangeSetV1 {
    /// Derive a change set from two complete manifests.
    ///
    /// Both claimed manifest hashes must match trees rebuilt through Oak's
    /// validating tree builder. Callers therefore cannot certify arbitrary
    /// tree hashes merely by supplying plausible deltas.
    pub fn from_manifests(
        base: &Manifest,
        result: &Manifest,
        scope: ChangeScopeV1,
    ) -> Result<Self> {
        let base_tree = verified_tree_hash(base, "base")?;
        let result_tree = verified_tree_hash(result, "result")?;
        let scope = canonical_scope(scope)?;

        let base_entries = canonical_entries(&base.entries);
        let result_entries = canonical_entries(&result.entries);
        let mut paths: Vec<&String> = base_entries.keys().chain(result_entries.keys()).collect();
        paths.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        paths.dedup();

        let changes: Vec<CanonicalPathChangeV1> = paths
            .into_iter()
            .filter_map(|path| {
                let before = base_entries.get(path).map(|entry| ChangeStateV1 {
                    blob_hash: entry.blob_hash.clone(),
                    mode: entry.mode,
                });
                let after = result_entries.get(path).map(|entry| ChangeStateV1 {
                    blob_hash: entry.blob_hash.clone(),
                    mode: entry.mode,
                });
                (before != after).then(|| CanonicalPathChangeV1 {
                    path: path.clone(),
                    before,
                    after,
                })
            })
            .collect();
        if let Some(path) = changes
            .iter()
            .map(|change| change.path.as_str())
            .find(|path| !scope_covers(&scope, path))
        {
            return Err(OakError::InvalidArgument(format!(
                "change path {path:?} is outside the declared change-set scope"
            )));
        }

        let mut preimage = Vec::new();
        field(&mut preimage, DOMAIN);
        field(&mut preimage, CHANGE_SET_OBJECT_FORMAT_V1.as_bytes());
        field(&mut preimage, base_tree.as_str().as_bytes());
        field(&mut preimage, result_tree.as_str().as_bytes());
        encode_scope(&mut preimage, &scope);
        preimage.extend_from_slice(&(changes.len() as u64).to_le_bytes());
        for change in &changes {
            field(&mut preimage, change.path.as_bytes());
            encode_state(&mut preimage, change.before.as_ref());
            encode_state(&mut preimage, change.after.as_ref());
        }

        Ok(Self {
            schema_version: 1,
            object_format: CHANGE_SET_OBJECT_FORMAT_V1.to_string(),
            id: hash_bytes(&preimage),
            base_tree,
            result_tree,
            scope,
            changes,
        })
    }

    pub fn schema_version(&self) -> u8 {
        self.schema_version
    }

    pub fn object_format(&self) -> &str {
        &self.object_format
    }

    pub fn id(&self) -> &Hash {
        &self.id
    }

    pub fn base_tree(&self) -> &Hash {
        &self.base_tree
    }

    pub fn result_tree(&self) -> &Hash {
        &self.result_tree
    }

    pub fn scope(&self) -> &ChangeScopeV1 {
        &self.scope
    }

    pub fn changes(&self) -> &[CanonicalPathChangeV1] {
        &self.changes
    }
}

fn verified_tree_hash(manifest: &Manifest, label: &str) -> Result<Hash> {
    for entry in &manifest.entries {
        let parsed = Hash::from_hex(entry.blob_hash.as_str()).map_err(|_| {
            OakError::InvalidHash(format!(
                "{label} manifest path {:?} has a non-canonical blob hash",
                entry.path
            ))
        })?;
        if parsed.as_str().len() != 64 {
            return Err(OakError::InvalidHash(format!(
                "{label} manifest path {:?} does not have a V1 BLAKE3 blob hash",
                entry.path
            )));
        }
    }
    let derived = build_tree(&manifest.entries)?.root_hash;
    if derived != manifest.hash {
        return Err(OakError::InvalidHash(format!(
            "{label} manifest claims tree {}, but its entries derive {derived}",
            manifest.hash
        )));
    }
    Ok(derived)
}

fn canonical_entries(entries: &[ManifestEntry]) -> BTreeMap<String, &ManifestEntry> {
    entries
        .iter()
        .map(|entry| (normalize_path(&entry.path), entry))
        .collect()
}

fn canonical_scope(scope: ChangeScopeV1) -> Result<ChangeScopeV1> {
    match scope {
        ChangeScopeV1::Full => Ok(ChangeScopeV1::Full),
        ChangeScopeV1::Paths { paths } => {
            if paths.is_empty() {
                return Err(OakError::InvalidArgument(
                    "path-scoped change sets require at least one path".to_string(),
                ));
            }
            let mut canonical = Vec::with_capacity(paths.len());
            for path in paths {
                crate::validate_tree_path(&path)?;
                canonical.push(normalize_path(&path));
            }
            canonical.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            canonical.dedup();
            Ok(ChangeScopeV1::Paths { paths: canonical })
        }
    }
}

fn scope_covers(scope: &ChangeScopeV1, path: &str) -> bool {
    match scope {
        ChangeScopeV1::Full => true,
        ChangeScopeV1::Paths { paths } => paths.iter().any(|scope_path| {
            path == scope_path
                || path
                    .strip_prefix(scope_path)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }),
    }
}

fn field(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u64).to_le_bytes());
    out.extend_from_slice(value);
}

fn encode_scope(out: &mut Vec<u8>, scope: &ChangeScopeV1) {
    match scope {
        ChangeScopeV1::Full => out.push(0),
        ChangeScopeV1::Paths { paths } => {
            out.push(1);
            out.extend_from_slice(&(paths.len() as u64).to_le_bytes());
            for path in paths {
                field(out, path.as_bytes());
            }
        }
    }
}

fn encode_state(out: &mut Vec<u8>, state: Option<&ChangeStateV1>) {
    let Some(state) = state else {
        out.push(0);
        return;
    };
    out.push(1);
    out.push(match state.mode {
        FileMode::Regular => 0,
        FileMode::Executable => 1,
        FileMode::Symlink => 2,
    });
    field(out, state.blob_hash.as_str().as_bytes());
}
