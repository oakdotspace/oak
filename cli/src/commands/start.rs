//! `oak start "<task summary>"` — begin an agent task in one step.
//!
//! 1. Name a branch after the summary (slugified; `-2`, `-3`, ... when the
//!    name is taken locally or on the remote), or use `--name`.
//! 2. Create it from latest available main with `oak switch -c` semantics
//!    (dirty files carried over; `--clean` discards them).
//! 3. Set the branch description to the summary.
//! 4. Push it immediately so the task is visible on the server (skipped by
//!    `--no-push` or `watch.auto_push = false`, and when the checkout is not
//!    linked to a remote yet).
//! 5. Start `oak watch` (skipped by `--no-watch` or `watch.enabled = false`).
//!
//! Push and watch failures do not undo the branch: the task has started, and
//! the receipt names what to retry.

use std::path::Path;

use oak_core::{MetadataKey, OakError, Repository, Result};
use serde::Serialize;

use crate::output;

/// Longest generated branch name (before any `-N` dedupe suffix).
const MAX_SLUG_LEN: usize = 48;

#[derive(Clone, Copy, Debug, Default)]
pub struct StartOptions {
    pub no_push: bool,
    pub no_watch: bool,
    pub clean: bool,
}

#[derive(Serialize)]
pub struct StartJson {
    pub schema_version: u32,
    pub kind: &'static str,
    pub branch: String,
    pub description: String,
    pub pushed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_skipped_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_error: Option<String>,
    pub watch_running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch_skipped_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_url: Option<String>,
    pub recommended_next_commands: Vec<String>,
}

/// Turn a task summary into a branch-name slug: lowercase ASCII
/// alphanumerics separated by single dashes, cut at a word boundary.
pub fn slugify(summary: &str) -> String {
    let mut slug = String::new();
    for c in summary.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.len() <= MAX_SLUG_LEN {
        return slug.to_string();
    }
    let cut = &slug[..MAX_SLUG_LEN];
    match cut.rfind('-') {
        Some(at) if at >= MAX_SLUG_LEN / 2 => cut[..at].to_string(),
        _ => cut.trim_end_matches('-').to_string(),
    }
}

async fn remote_branch_exists(repo: &dyn Repository, name: &str) -> bool {
    let (Some(remote), Some(owner), Some(repo_name)) = (
        repo.get_metadata(MetadataKey::RemoteUrl)
            .ok()
            .flatten()
            .and_then(|url| super::push::normalize_remote_url(&url)),
        repo.get_metadata(MetadataKey::RepoOwner).ok().flatten(),
        repo.get_metadata(MetadataKey::RepoName).ok().flatten(),
    ) else {
        return false;
    };
    let api_key = super::credentials::effective_token_for_repository(&remote, repo);
    let client = crate::http::api_client();
    let repo_path = format!("{owner}/{repo_name}");
    let lookup = super::push::fetch_remote_branch_head(
        &client,
        &remote,
        &repo_path,
        name,
        api_key.as_deref(),
    );
    // Best-effort: offline or slow servers just skip the remote check (a
    // real collision then surfaces as an ordinary push error).
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), lookup).await,
        Ok(Ok(Some(_)))
    )
}

/// Pick a free branch name for `base`: itself, then `base-2`, `base-3`, ...
async fn dedupe(repo: &dyn Repository, base: &str) -> Result<String> {
    for n in 1..100 {
        let candidate = if n == 1 {
            base.to_string()
        } else {
            format!("{base}-{n}")
        };
        if repo.get_branch(&candidate)?.is_none() && !remote_branch_exists(repo, &candidate).await {
            return Ok(candidate);
        }
    }
    Ok(format!("{base}-{}", chrono::Utc::now().timestamp()))
}

fn linked(repo: &dyn Repository) -> bool {
    [
        MetadataKey::RemoteUrl,
        MetadataKey::RepoOwner,
        MetadataKey::RepoName,
    ]
    .iter()
    .all(|key| repo.get_metadata(*key).ok().flatten().is_some())
}

/// `oak start "<summary>" [--name NAME] [--no-push] [--no-watch] [--clean] [--json]`.
pub async fn run(
    path: &Path,
    summary: &str,
    name: Option<&str>,
    options: StartOptions,
    json: bool,
) -> Result<()> {
    let summary = summary.trim();
    if summary.is_empty() {
        return Err(OakError::InvalidArgument(
            "oak start needs a task summary, e.g. `oak start \"fix the login redirect\"`"
                .to_string(),
        ));
    }
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    if super::mount::mount_dest_for(path).ok().flatten().is_some() {
        return Err(OakError::InvalidArgument(
            "a mount already has its own task branch; describe it with `oak desc \"<summary>\"` instead of `oak start`"
                .to_string(),
        ));
    }
    if json {
        output::route_narration_to_stderr();
    }

    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;
    let branch = match name {
        Some(name) => {
            super::branch::validate_branch_name(name)?;
            if repo.get_branch(name)?.is_some() {
                return Err(OakError::BranchAlreadyExists(name.to_string()));
            }
            name.to_string()
        }
        None => {
            let slug = slugify(summary);
            if slug.is_empty() || slug == oak_core::DEFAULT_BRANCH {
                super::merge::next_open_personal_branch_name(repo.as_ref())?
            } else {
                super::branch::validate_branch_name(&slug)?;
                dedupe(repo.as_ref(), &slug).await?
            }
        }
    };
    drop(repo);

    // `oak switch -c` runs its own runtime for the main refresh, so it must
    // run off this one's worker threads.
    let policy = super::switch::WorktreePolicy::from_clean_flag(options.clean);
    {
        let (path, branch) = (path.to_path_buf(), branch.clone());
        tokio::task::spawn_blocking(move || super::switch::create(&path, &branch, policy))
            .await
            .map_err(|e| OakError::Io(std::io::Error::other(e)))??;
    }
    let repo = ctx.open()?;
    repo.edit_branch_description(&branch, summary)?;

    let settings = super::config::resolve_for_path(path);
    let is_linked = linked(repo.as_ref());
    let mut receipt = StartJson {
        schema_version: 1,
        kind: "start",
        branch: branch.clone(),
        description: summary.to_string(),
        pushed: false,
        push_skipped_reason: None,
        push_error: None,
        watch_running: false,
        watch_pid: None,
        watch_skipped_reason: None,
        watch_error: None,
        branch_url: None,
        recommended_next_commands: Vec::new(),
    };
    if is_linked {
        if let (Some(remote), Ok((owner, name))) = (
            repo.get_metadata(MetadataKey::RemoteUrl)?,
            super::read_repo_identity(repo.as_ref()),
        ) {
            receipt.branch_url = Some(super::branch_web_url(
                remote.trim_end_matches('/'),
                &format!("{owner}/{name}"),
                &branch,
            ));
        }
    }
    drop(repo);

    if options.no_push {
        receipt.push_skipped_reason = Some("--no-push".to_string());
    } else if !settings.auto_push.value {
        receipt.push_skipped_reason = Some(format!(
            "watch.auto_push = false ({})",
            settings.auto_push.source.describe()
        ));
    } else if !is_linked {
        receipt.push_skipped_reason = Some("not_linked".to_string());
    } else {
        // A fresh branch usually has no commits of its own, and a
        // commit-less push publishes nothing ("Already up to date"), so the
        // branch row itself — with its description — is published through
        // the same metadata sync `oak desc` uses.
        match super::push::run(path, None, false, None).await {
            Err(error) => receipt.push_error = Some(error.to_string()),
            Ok(()) => match super::branch::edit_current_branch_json(path, summary, None).await {
                Ok(desc) if desc.remote_synced == serde_json::Value::Bool(true) => {
                    receipt.pushed = true
                }
                Ok(desc) => {
                    receipt.push_error = Some(desc.remote_error.unwrap_or_else(|| {
                        format!("branch not published (remote state: {})", desc.remote_state)
                    }))
                }
                Err(error) => receipt.push_error = Some(error.to_string()),
            },
        }
    }

    if options.no_watch {
        receipt.watch_skipped_reason = Some("--no-watch".to_string());
    } else if !settings.enabled.value {
        receipt.watch_skipped_reason = Some(format!(
            "watch.enabled = false ({})",
            settings.enabled.source.describe()
        ));
    } else if !is_linked {
        receipt.watch_skipped_reason = Some("not_linked".to_string());
    } else {
        match super::watch::target(path).and_then(|t| super::watch::ensure_running(&t, true)) {
            Ok(report) => {
                receipt.watch_running = true;
                receipt.watch_pid = report.pid;
            }
            Err(error) => receipt.watch_error = Some(error.to_string()),
        }
    }

    if receipt.push_error.is_some() {
        receipt
            .recommended_next_commands
            .push("oak push --json".to_string());
    }
    if !is_linked {
        receipt
            .recommended_next_commands
            .push(super::push::PUSH_REPO_PLACEHOLDER_COMMAND.to_string());
    }
    if receipt.watch_error.is_some() {
        receipt
            .recommended_next_commands
            .push("oak watch start".to_string());
    }
    receipt
        .recommended_next_commands
        .push("oak status --json --compact".to_string());

    if json {
        return output::print_json(&receipt);
    }
    output::success(&format!("Started task on branch '{branch}': {summary}"));
    if receipt.pushed {
        output::item(&format!(
            "Pushed{}",
            receipt
                .branch_url
                .as_deref()
                .map_or(String::new(), |url| format!(": {url}"))
        ));
    } else if let Some(error) = &receipt.push_error {
        output::warning(&format!("push failed: {error} (retry with `oak push`)"));
    } else if let Some(reason) = &receipt.push_skipped_reason {
        output::item(&format!("Not pushed ({reason})"));
    }
    if receipt.watch_running {
        output::item("oak watch is keeping this branch merged with main and pushing your commits");
    } else if let Some(error) = &receipt.watch_error {
        output::warning(&format!("oak watch did not start: {error}"));
    } else if let Some(reason) = &receipt.watch_skipped_reason {
        output::item(&format!("oak watch not started ({reason})"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugify_makes_short_branch_safe_names() {
        assert_eq!(slugify("Fix the login redirect!"), "fix-the-login-redirect");
        assert_eq!(slugify("  --Add   CSV export (v2)  "), "add-csv-export-v2");
        assert_eq!(slugify("日本語"), "");
        let long = slugify(
            "Refactor the authentication middleware so that sessions expire correctly everywhere",
        );
        assert!(long.len() <= 48, "{long}");
        assert!(!long.ends_with('-'));
        assert!(long.starts_with("refactor-the-authentication-middleware"));
    }
}
