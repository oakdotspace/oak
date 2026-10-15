//! `oak diff --remote OLD NEW` (fb-410): diff two exact remote commits
//! without a checkout, without resolving either through local refs, and
//! without moving any ref.
//!
//! Acquisition is exactly the remote-review path: one bounded `commits/info`
//! request for the two commits (and their trees) when they are not already
//! cached, then — only with `--hunks`/`--print` — bounded, hash-verified raw
//! reads of the returned files' content. Everything admitted lands in the
//! content-addressed object cache; refs, branch rows and descriptions are
//! never written.

use std::path::{Path, PathBuf};

use oak_core::{Hash, OakError, Result};

use super::{
    hunks, manifest_for_head, per_file_commands, qualify_snapshot_rows, remote_content,
    shell_quote_path, summarize_manifest_changes, validate_changed_files_window,
    window_changed_files, AcquisitionJson, DiffJson, DiffJsonOptions, EVIDENCE_SNAPSHOT,
    OMITTED_BYTE_BUDGET, OMITTED_MISSING_BLOB, OMITTED_REMOTE_CONTENT_BUDGET, SCHEMA_VERSION,
};
use crate::output;

/// Split `OLD NEW [paths...]` (or `OLD NEW -- paths...`). Both revisions must
/// be full commit hashes: a remote diff never resolves prefixes or names
/// through local state, so an abbreviated or symbolic endpoint is refused.
fn split_args(
    args: &[PathBuf],
    forced_path_count: Option<usize>,
) -> Result<(Hash, Hash, Vec<PathBuf>)> {
    let revision_count = match forced_path_count {
        Some(forced) => args.len().saturating_sub(forced),
        None => 2.min(args.len()),
    };
    if revision_count != 2 {
        return Err(OakError::InvalidArgument(
            "`oak diff --remote` takes exactly two full commit hashes: `oak diff --remote OLD NEW [-- <path>...]`".to_string(),
        ));
    }
    let parse = |arg: &PathBuf| {
        let text = arg.to_string_lossy();
        Hash::from_hex(&text).map_err(|_| {
            OakError::InvalidArgument(format!(
                "`oak diff --remote` needs full commit hashes (40 or 64 lowercase hex characters); got '{text}'. Remote endpoints are never resolved through local names or prefixes"
            ))
        })
    };
    Ok((
        parse(&args[0])?,
        parse(&args[1])?,
        args[revision_count..].to_vec(),
    ))
}

fn command_stub(old: &Hash, new: &Hash) -> String {
    format!("oak diff --remote {old} {new}")
}

/// Compute (and print) the remote endpoint diff. Returns whether any
/// differences were found, for `--exit-code`.
pub async fn remote_endpoint_diff(
    path: &Path,
    args: &[PathBuf],
    forced_path_count: Option<usize>,
    json: bool,
    options: DiffJsonOptions,
) -> Result<bool> {
    let diff = compute(path, args, forced_path_count, options).await?;
    let differs = diff.changed_file_count > 0;
    if json {
        output::print_json(&diff)?;
        return Ok(differs);
    }
    let mut out = String::new();
    let mut omitted = Vec::new();
    for file in &diff.changed_files {
        match (&file.patch, file.patch_omitted_reason) {
            (Some(patch), _) if options.line_numbers => {
                out.push_str(&hunks::number_patch_text(patch))
            }
            (Some(patch), _) => out.push_str(patch),
            (None, Some(reason)) => omitted.push(format!("{} ({reason})", file.path)),
            (None, None) => {}
        }
    }
    if let Some(patch) = out.strip_suffix('\n') {
        output::print_line(patch);
    }
    if !omitted.is_empty() {
        eprintln!(
            "oak: patch omitted for {} file(s): {}",
            omitted.len(),
            omitted.join(", ")
        );
    }
    Ok(differs)
}

async fn compute(
    path: &Path,
    args: &[PathBuf],
    forced_path_count: Option<usize>,
    options: DiffJsonOptions,
) -> Result<DiffJson> {
    let DiffJsonOptions {
        changed_files_limit,
        changed_files_offset,
        hunks: want_hunks,
        max_bytes,
        context,
        force_text,
        line_numbers,
    } = options;
    validate_changed_files_window(changed_files_limit)?;
    let (old, new, paths) = split_args(args, forced_path_count)?;
    let workspace = crate::commands::branch::RemoteWorkspace::open(path)?;
    let repo = &workspace.repo;
    let remote = &workspace.remote;
    let filters = crate::commands::diff::resolve_filters(path, &workspace.work_tree, &paths)?;

    let mut budget = crate::commands::blob_fetch::ReviewPreparationBudget::new();
    crate::commands::blob_fetch::ensure_review_commits_with_budget(
        repo,
        &remote.remote_url,
        &remote.owner,
        &remote.repo_name,
        remote.token.as_deref(),
        &[old.clone(), new.clone()],
        &mut budget,
    )
    .await?;
    let old_manifest = manifest_for_head(repo, Some(&old))?;
    let new_manifest = manifest_for_head(repo, Some(&new))?;

    let stub = command_stub(&old, &new);
    let path_suffix = if paths.is_empty() {
        String::new()
    } else {
        format!(
            " -- {}",
            paths
                .iter()
                .map(|p| shell_quote_path(&p.to_string_lossy()))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    let next_command = changed_files_limit.map(|limit| {
        let mut command = format!(
            "{stub} --json --changed-files-limit {limit} --changed-files-offset {}",
            changed_files_offset.saturating_add(limit)
        );
        if want_hunks {
            command.push_str(" --hunks");
            if let Some(max_bytes) = max_bytes {
                command.push_str(&format!(" --max-bytes {max_bytes}"));
            }
            if context != oak_core::DEFAULT_CONTEXT_LINES {
                command.push_str(&format!(" -U {context}"));
            }
            if force_text {
                command.push_str(" --text");
            }
            if line_numbers {
                command.push_str(" --line-numbers");
            }
        }
        command.push_str(&path_suffix);
        command
    });
    let window = || -> Result<_> {
        let changes = crate::commands::diff::filter_changes(
            super::diff_manifests_with_content_renames(repo, &old_manifest, &new_manifest)?,
            &filters,
        );
        let summaries = summarize_manifest_changes(repo, &changes)?;
        let (page, page_json) = window_changed_files(
            &summaries,
            changed_files_limit,
            changed_files_offset,
            next_command.clone(),
        );
        Ok((changes, summaries.len(), page, page_json))
    };
    let (mut changes, mut changed_file_count, mut changed_files, mut changed_files_page) =
        window()?;

    let mut hydration = None;
    if want_hunks && !changed_files.is_empty() {
        let sides = [
            remote_content::DiffSide {
                head: &new,
                manifest: &new_manifest,
            },
            remote_content::DiffSide {
                head: &old,
                manifest: &old_manifest,
            },
        ];
        let files: Vec<(String, Vec<String>)> = changed_files
            .iter()
            .map(|file| {
                let mut lookups = vec![file.path.clone()];
                lookups.extend(file.old_path.clone());
                (file.path.clone(), lookups)
            })
            .collect();
        let report =
            remote_content::hydrate_diff_files(repo, remote, &sides, &files, &mut budget).await?;
        if report.downloaded_objects > 0 {
            (
                changes,
                changed_file_count,
                changed_files,
                changed_files_page,
            ) = window()?;
        }
        hydration = Some(report);
    }
    let hunks_truncated = if want_hunks {
        super::attach_patches_from_store(
            repo,
            &Some(old_manifest.clone()),
            &changes,
            &mut changed_files,
            max_bytes,
            context,
            force_text,
        )?
    } else {
        false
    };
    let mut unavailable_after_hydration = 0usize;
    if let Some(report) = hydration.as_ref() {
        for file in &mut changed_files {
            if file.patch_omitted_reason == Some(OMITTED_MISSING_BLOB) {
                if let Some(reason) = report.unavailable.get(&file.path) {
                    file.patch_omitted_reason = Some(reason);
                }
                unavailable_after_hydration += 1;
            }
        }
    }
    qualify_snapshot_rows(&mut changed_files, EVIDENCE_SNAPSHOT);
    if line_numbers {
        hunks::attach_line_numbers(&mut changed_files);
    }

    let mut caveats = vec![
        "Both endpoints are exact commits read from the remote by full hash; local refs, branch metadata and the working tree were not read or changed (verified objects may be cached).".to_string(),
        "This is a snapshot (tree) comparison of two commits, not a branch contribution or merge prediction.".to_string(),
    ];
    if unavailable_after_hydration > 0 {
        caveats.push(format!(
            "{unavailable_after_hydration} returned file(s) have no patch because their content could not be verified from the remote within bounded preparation (see patch_omitted_reason)."
        ));
    }
    let mut recommended_next_commands = Vec::new();
    if hunks_truncated {
        let narrower = changed_files.len() > 1;
        let flags = super::hunk_followup_flags("--json --hunks", line_numbers);
        recommended_next_commands.extend(per_file_commands(
            &stub,
            &changed_files,
            &flags,
            |file| {
                file.patch_omitted_reason == Some(OMITTED_BYTE_BUDGET)
                    || (narrower
                        && file.patch_omitted_reason == Some(OMITTED_REMOTE_CONTENT_BUDGET))
            },
        ));
    }
    if !want_hunks {
        recommended_next_commands.push(format!("{stub} --json --hunks{path_suffix}"));
    }
    recommended_next_commands.push(format!("{stub} --print{path_suffix}"));

    Ok(DiffJson {
        schema_version: SCHEMA_VERSION,
        kind: "remote_endpoint_diff",
        diff_mode: "tree",
        evidence_kind: Some(EVIDENCE_SNAPSHOT),
        branch: Some(new.to_string()),
        against: Some(old.to_string()),
        branch_head: Some(new.to_string()),
        parent: None,
        changed_file_count,
        changed_files,
        changed_files_page,
        files_identical_to_against: None,
        hunks_truncated,
        conflict_files: Vec::new(),
        conflict_file_count: 0,
        merge_lineage_evidence: None,
        acquisition: Some(AcquisitionJson::from_budget(&budget)),
        ancestry_diagnostic: None,
        caveats,
        recommended_next_commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_must_be_two_full_hashes() {
        let full = |c: char| PathBuf::from(c.to_string().repeat(64));
        let (old, new, paths) =
            split_args(&[full('a'), full('b'), PathBuf::from("src")], None).unwrap();
        assert_eq!((old.as_str().len(), new.as_str().len()), (64, 64));
        assert_eq!(paths, vec![PathBuf::from("src")]);
        let (_, _, paths) =
            split_args(&[full('a'), full('b'), PathBuf::from("x")], Some(1)).unwrap();
        assert_eq!(paths, vec![PathBuf::from("x")]);
        for bad in [
            vec![full('a')],
            vec![full('a'), PathBuf::from("main")],
            vec![full('a'), PathBuf::from("abcd1234")],
            vec![full('a'), full('b'), full('c')],
        ] {
            let forced = (bad.len() == 3).then_some(0);
            assert!(split_args(&bad, forced).is_err(), "{bad:?}");
        }
    }
}
