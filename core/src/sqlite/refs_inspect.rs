//! Local ref evidence from ONE pinned SQLite read transaction (fb-460).
//!
//! `current_branch`, every branch row, every branch head, parent inheritance,
//! the legacy `metadata.head` pointer and the effective HEAD are all read
//! inside the deferred read transaction `open_read_only` starts, so they can
//! never mix two states. Nothing is written, migrated or hydrated.
use super::file_inspect::{
    bounded_metadata_text, database, failure, require_read_only, Checked, Failure, InspectionStatus,
};
use super::SqliteRepository;
use crate::{Hash, OakError, Result};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

pub const DEFAULT_REFS_MAX_BRANCHES: u64 = 1_000;
pub const MAX_REFS_MAX_BRANCHES: u64 = 100_000;
const REFERENCE_BYTES: i64 = 64 * 1024;
const PARENT_HOPS: usize = 128;

#[derive(Debug, Serialize)]
pub struct RefsInspection {
    pub snapshot: RefsSnapshot,
    /// `metadata.current_branch`; `None` (or empty) means detached.
    pub current_branch: Option<String>,
    pub attached: bool,
    /// Legacy `metadata.head` pointer. Authoritative only when detached.
    pub legacy_head: Option<String>,
    pub effective_head: EffectiveHead,
    pub branches: Vec<BranchRef>,
    /// Distinct names across `branches` rows and `branch_heads` rows.
    pub branch_count: u64,
    pub branches_truncated: bool,
    pub max_branches: u64,
    /// Every inconsistency observed in this snapshot. Empty means the refs
    /// agree with each other, not that they agree with any remote.
    pub disagreements: Vec<RefDisagreement>,
}

#[derive(Debug, Serialize)]
pub struct RefsSnapshot {
    pub source: &'static str,
    pub single_read_transaction: bool,
}

#[derive(Debug, Serialize)]
pub struct EffectiveHead {
    pub commit: Option<String>,
    /// `current_branch_head`, `parent_inheritance`, `detached_legacy_head`,
    /// or `absent`.
    pub source: &'static str,
    /// Branches walked from `current_branch` to the branch owning the head.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub branch_chain: Vec<String>,
    pub status: InspectionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Whether the effective head's commit row exists locally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_present: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct BranchRef {
    pub name: String,
    /// False for head-only names such as the local copy of `main`.
    pub has_branch_row: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_branch: Option<String>,
    /// This branch's own `branch_heads` row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Own head, else the first ancestor head found through `parent_branch`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherited_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_commit_present: Option<bool>,
    pub current: bool,
}

#[derive(Debug, Serialize)]
pub struct RefDisagreement {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub detail: String,
}

impl SqliteRepository {
    /// Read every local ref in one pinned snapshot. Requires `open_read_only`.
    pub fn inspect_refs(&self, max_branches: u64) -> Result<RefsInspection> {
        if max_branches == 0 || max_branches > MAX_REFS_MAX_BRANCHES {
            return Err(OakError::InvalidArgument(format!(
                "--max-branches must be between 1 and {MAX_REFS_MAX_BRANCHES}"
            )));
        }
        let conn = self.conn.lock().unwrap();
        require_read_only(&conn, "refs inspection")?;
        inspect(&conn, max_branches).map_err(|Failure(status, reason)| {
            OakError::Database(format!("refs inspection {status:?}: {reason}"))
        })
    }
}

fn inspect(conn: &Connection, max_branches: u64) -> Checked<RefsInspection> {
    let mut disagreements = Vec::new();
    let current_branch = bounded_metadata_text(conn, "current_branch", "current branch metadata")?;
    let attached = current_branch
        .as_deref()
        .is_some_and(|name| !name.is_empty());
    let legacy_head = bounded_metadata_text(conn, "head", "legacy HEAD metadata")?;

    // name -> (row?, status, parent) ; heads name -> head
    let mut rows: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    let mut oversized = 0u64;
    {
        let mut stmt = conn
            .prepare(
                "SELECT CASE WHEN length(CAST(name AS BLOB))<=?1 THEN name END, \
                 CASE WHEN length(CAST(status AS BLOB))<=?1 THEN status END, \
                 CASE WHEN parent_branch IS NULL OR length(CAST(parent_branch AS BLOB))<=?1 \
                 THEN parent_branch END, \
                 parent_branch IS NOT NULL AND length(CAST(parent_branch AS BLOB))>?1 \
                 FROM branches",
            )
            .map_err(database)?;
        let mut query = stmt.query([REFERENCE_BYTES]).map_err(database)?;
        while let Some(row) = query.next().map_err(database)? {
            let name: Option<String> = row.get(0).map_err(database)?;
            let Some(name) = name else {
                oversized += 1;
                continue;
            };
            if row.get::<_, bool>(3).map_err(database)? {
                disagreements.push(RefDisagreement {
                    kind: "oversized_reference",
                    branch: Some(name.clone()),
                    detail: "parent_branch exceeds the 64 KiB reference budget".into(),
                });
            }
            rows.insert(
                name,
                (row.get(1).map_err(database)?, row.get(2).map_err(database)?),
            );
        }
    }
    let mut heads: BTreeMap<String, String> = BTreeMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT CASE WHEN length(CAST(branch_name AS BLOB))<=?1 THEN branch_name END, \
                 CASE WHEN length(CAST(head_hash AS BLOB))<=?1 THEN head_hash END \
                 FROM branch_heads",
            )
            .map_err(database)?;
        let mut query = stmt.query([REFERENCE_BYTES]).map_err(database)?;
        while let Some(row) = query.next().map_err(database)? {
            let name: Option<String> = row.get(0).map_err(database)?;
            let head: Option<String> = row.get(1).map_err(database)?;
            match (name, head) {
                (Some(name), Some(head)) => {
                    heads.insert(name, head);
                }
                (Some(name), None) => disagreements.push(RefDisagreement {
                    kind: "oversized_reference",
                    branch: Some(name),
                    detail: "branch head exceeds the 64 KiB reference budget".into(),
                }),
                (None, _) => oversized += 1,
            }
        }
    }
    if oversized > 0 {
        disagreements.push(RefDisagreement {
            kind: "oversized_reference",
            branch: None,
            detail: format!(
                "{oversized} branch name(s) exceed the 64 KiB reference budget and are omitted"
            ),
        });
    }

    let commit_present = |hash: &str| -> Checked<Option<bool>> {
        if Hash::from_hex(hash).is_err() || hash.len() != 64 {
            return Ok(None);
        }
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM commits WHERE hash=?1)",
            [hash],
            |r| r.get::<_, bool>(0),
        )
        .map(Some)
        .map_err(database)
    };
    // Effective head of one branch through parent inheritance; the chain
    // lists every branch visited, ending at the owner of the head.
    let resolve =
        |start: &str| -> std::result::Result<(Option<String>, Vec<String>), &'static str> {
            let mut seen = HashSet::new();
            let mut chain = Vec::new();
            let mut branch = start.to_string();
            for _ in 0..PARENT_HOPS {
                if !seen.insert(branch.clone()) {
                    return Err("parent_cycle");
                }
                chain.push(branch.clone());
                if let Some(head) = heads.get(&branch) {
                    return Ok((Some(head.clone()), chain));
                }
                match rows.get(&branch).and_then(|(_, parent)| parent.clone()) {
                    Some(parent) => branch = parent,
                    None => return Ok((None, chain)),
                }
            }
            Err("parent_chain_budget")
        };

    let mut names: Vec<&String> = rows.keys().chain(heads.keys()).collect();
    names.sort();
    names.dedup();
    let branch_count = names.len() as u64;
    let branches_truncated = branch_count > max_branches;
    let mut branches = Vec::new();
    for name in names.iter().take(max_branches as usize) {
        let row = rows.get(*name);
        let head = heads.get(*name).cloned();
        let mut entry = BranchRef {
            name: (*name).clone(),
            has_branch_row: row.is_some(),
            status: row.and_then(|(status, _)| status.clone()),
            parent_branch: row.and_then(|(_, parent)| parent.clone()),
            head: head.clone(),
            effective_head: None,
            inherited_from: None,
            head_commit_present: None,
            current: attached && current_branch.as_deref() == Some(name.as_str()),
        };
        match resolve(name) {
            Ok((effective, chain)) => {
                if head.is_none() && effective.is_some() {
                    entry.inherited_from = chain.last().cloned();
                }
                entry.effective_head = effective;
            }
            Err(kind) => disagreements.push(RefDisagreement {
                kind,
                branch: Some((*name).clone()),
                detail: "parent_branch chain never reaches a head".into(),
            }),
        }
        if let Some(head) = head.as_deref() {
            if Hash::from_hex(head).is_err() || head.len() != 64 {
                disagreements.push(RefDisagreement {
                    kind: "malformed_head",
                    branch: Some((*name).clone()),
                    detail: "branch head is not a 64-character hexadecimal commit hash".into(),
                });
            }
            entry.head_commit_present = commit_present(head)?;
            if entry.head_commit_present == Some(false) {
                disagreements.push(RefDisagreement {
                    kind: "head_commit_missing",
                    branch: Some((*name).clone()),
                    detail: format!("branch head {head} has no local commit row"),
                });
            }
        }
        if let Some(parent) = entry.parent_branch.as_deref() {
            if parent != "main" && !rows.contains_key(parent) && !heads.contains_key(parent) {
                disagreements.push(RefDisagreement {
                    kind: "dangling_parent",
                    branch: Some((*name).clone()),
                    detail: format!("parent_branch '{parent}' has no local branch or head row"),
                });
            }
        }
        branches.push(entry);
    }

    let mut effective = EffectiveHead {
        commit: None,
        source: "absent",
        branch_chain: Vec::new(),
        status: InspectionStatus::Verified,
        reason: None,
        commit_present: None,
    };
    if attached {
        let current = current_branch.clone().unwrap_or_default();
        if !rows.contains_key(&current) && !heads.contains_key(&current) {
            disagreements.push(RefDisagreement {
                kind: "current_branch_unknown",
                branch: Some(current.clone()),
                detail: "current_branch names a branch with no local branch or head row".into(),
            });
        }
        match resolve(&current) {
            Ok((Some(commit), chain)) => {
                effective.source = if chain.len() == 1 {
                    "current_branch_head"
                } else {
                    "parent_inheritance"
                };
                effective.commit = Some(commit);
                effective.branch_chain = chain;
            }
            Ok((None, chain)) => {
                effective.status = InspectionStatus::ObjectMissing;
                effective.reason = Some("HEAD is absent".into());
                effective.branch_chain = chain;
            }
            Err(kind) => {
                let Failure(status, reason) = if kind == "parent_cycle" {
                    failure(
                        InspectionStatus::Corrupt,
                        "HEAD branch parent chain contains a cycle",
                    )
                } else {
                    failure(
                        InspectionStatus::BudgetExceeded,
                        "HEAD parent chain exceeds 128-hop verification budget",
                    )
                };
                effective.status = status;
                effective.reason = Some(reason);
            }
        }
        if let (Some(legacy), Some(commit)) = (legacy_head.as_deref(), effective.commit.as_deref())
        {
            if legacy != commit {
                disagreements.push(RefDisagreement {
                    kind: "legacy_head_differs",
                    branch: Some(current.clone()),
                    detail: format!(
                        "metadata.head {legacy} differs from the effective HEAD {commit}; the branch head wins while attached"
                    ),
                });
            }
        }
    } else if let Some(legacy) = legacy_head.clone() {
        effective.source = "detached_legacy_head";
        effective.commit = Some(legacy);
    } else {
        effective.status = InspectionStatus::ObjectMissing;
        effective.reason = Some("HEAD is absent".into());
    }
    if let Some(commit) = effective.commit.as_deref() {
        if Hash::from_hex(commit).is_err() || commit.len() != 64 {
            effective.status = InspectionStatus::Corrupt;
            effective.reason =
                Some("effective HEAD is not a 64-character hexadecimal commit hash".into());
        } else {
            effective.commit_present = commit_present(commit)?;
            if effective.commit_present == Some(false) {
                effective.status = InspectionStatus::ObjectMissing;
                effective.reason = Some(format!("effective HEAD {commit} has no local commit row"));
            }
        }
    }

    Ok(RefsInspection {
        snapshot: RefsSnapshot {
            source: "local_sqlite_snapshot",
            single_read_transaction: true,
        },
        current_branch: current_branch.filter(|name| !name.is_empty()),
        attached,
        legacy_head,
        effective_head: effective,
        branches,
        branch_count,
        branches_truncated,
        max_branches,
        disagreements,
    })
}
