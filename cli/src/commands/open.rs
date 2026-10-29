use oak_core::{BranchStatus, MetadataKey, OakError, Result};
use oak_core::{Repository, SqliteRepository};
use serde::Serialize;
use std::path::Path;

use crate::output;

/// What `oak open` would open, resolved without side effects.
#[derive(Debug, Serialize)]
pub struct OpenTargetJson {
    pub schema_version: u32,
    /// The URL `oak open` launches.
    pub url: String,
    /// `branch` (the current branch's page), `repo` (repository home: the
    /// branch is closed, unpublished, or detached), or `site` (no checkout).
    pub target: &'static str,
    pub branch: Option<String>,
    #[serde(flatten)]
    pub identity: super::RepoIdentityJson,
    /// Always false for `--json`/`--print`: no browser was launched.
    pub launched: bool,
}

/// Resolve the URL `oak open` would open. Errors exactly where `oak open`
/// would (an unlinked checkout), and never launches anything.
pub fn resolve_target(path: &Path) -> Result<OpenTargetJson> {
    match crate::resolve::resolve(path) {
        Ok(ctx) => {
            let repo = SqliteRepository::open(&ctx.db_path()?)?;
            let (owner, name) = crate::commands::read_repo_identity(&repo)?;

            let remote_url = repo
                .get_metadata(MetadataKey::RemoteUrl)?
                .or_else(|| std::env::var("OAK_REMOTE").ok())
                .unwrap_or_else(|| "https://oak.space".to_string());
            let remote_url = crate::http::redact_url_userinfo(&remote_url);

            let repo_path = format!("{owner}/{name}");
            let repo_url = crate::commands::repo_web_url(&remote_url, &repo_path);
            let branch = repo
                .get_current_branch_name()
                .ok()
                .flatten()
                .filter(|branch| !branch.is_empty());
            let identity = super::RepoIdentityJson::from_parts(
                Some(remote_url.clone()),
                Some(owner),
                Some(name),
                Some(&ctx.work_tree),
                branch.as_deref(),
            );
            // Only deep-link to the branch page when the current branch is
            // actually viewable on the server. Two cases would otherwise 404:
            //   * a closed branch (just merged onto main), and
            //   * a fresh personal branch with no commits of its own — the
            //     state `oak merge` leaves you in, which has never been pushed.
            // In both cases the branch page has nothing useful to show, so fall
            // back to the repo home.
            let (url, target) = match branch.as_deref() {
                Some(branch) if branch_is_viewable(&repo, branch) => (
                    crate::commands::branch_web_url(&remote_url, &repo_path, branch),
                    "branch",
                ),
                _ => (repo_url, "repo"),
            };
            Ok(OpenTargetJson {
                schema_version: crate::work_state::SCHEMA_VERSION,
                url,
                target,
                branch,
                identity,
                launched: false,
            })
        }
        Err(OakError::RepoNotFound) => Ok(OpenTargetJson {
            schema_version: crate::work_state::SCHEMA_VERSION,
            url: "https://oak.space".to_string(),
            target: "site",
            branch: None,
            identity: super::RepoIdentityJson::default(),
            launched: false,
        }),
        Err(e) => Err(e),
    }
}

pub fn run(path: &Path) -> Result<()> {
    let url = resolve_target(path)?.url;

    open::that(&url).map_err(|e| OakError::Io(std::io::Error::other(e)))?;

    output::success(&format!("Opened {url}"));
    Ok(())
}

/// `oak open --print`: the URL alone on stdout; no browser.
pub fn run_print(path: &Path) -> Result<()> {
    let target = resolve_target(path)?;
    output::print_line(&target.url);
    Ok(())
}

/// `oak open --json`: the URL plus identity as one JSON document; no browser.
pub fn run_json(path: &Path) -> Result<()> {
    output::print_json(&resolve_target(path)?)
}

/// Whether the branch page on the server is worth linking to. Returns `false`
/// for branches that would 404 (or show nothing): a closed/merged branch, or a
/// branch with no commits of its own (never pushed, e.g. the fresh personal
/// branch `oak merge` switches you onto). Any local read error is treated as
/// non-viewable so we fall back to the repo home rather than a broken link.
fn branch_is_viewable(repo: &SqliteRepository, branch: &str) -> bool {
    match repo.get_branch(branch) {
        Ok(Some(b)) if b.status == BranchStatus::Closed => return false,
        Ok(Some(_)) => {}
        // Unknown branch / lookup error: don't deep-link.
        _ => return false,
    }
    matches!(repo.get_commits_for_branch(branch), Ok(commits) if !commits.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_target_outside_a_repo_is_the_site_and_never_launches() {
        let temp = tempfile::TempDir::new().unwrap();
        let target = resolve_target(temp.path()).unwrap();
        assert_eq!(target.target, "site");
        assert!(!target.launched);
        assert_eq!(target.identity.web_url, None);
    }

    #[test]
    fn resolve_target_unpublished_branch_falls_back_to_repo_home() {
        let temp = tempfile::TempDir::new().unwrap();
        crate::commands::init::run(temp.path(), false).unwrap();
        let repo = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::RemoteUrl, "https://example.test/")
            .unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "web").unwrap();
        let branch = repo.get_current_branch_name().unwrap().unwrap();

        let target = resolve_target(temp.path()).unwrap();
        assert_eq!(target.target, "repo");
        assert_eq!(target.url, "https://example.test/acme/web");
        assert_eq!(target.branch.as_deref(), Some(branch.as_str()));
        assert_eq!(
            target.identity.review_url,
            Some(format!("https://example.test/acme/web/branches/{branch}"))
        );

        std::fs::write(temp.path().join("a.txt"), b"a\n").unwrap();
        crate::commands::commit::run(temp.path()).unwrap();
        let target = resolve_target(temp.path()).unwrap();
        assert_eq!(target.target, "branch");
        assert_eq!(target.url, target.identity.review_url.clone().unwrap());
    }

    #[test]
    fn unlinked_repo_errors_before_opening_browser() {
        let temp = tempfile::TempDir::new().unwrap();
        crate::commands::init::run(temp.path(), false).unwrap();

        let err = run(temp.path()).unwrap_err();
        assert!(matches!(
            resolve_target(temp.path()).unwrap_err(),
            OakError::Config(_)
        ));
        let msg = err.to_string();
        assert!(
            matches!(err, OakError::Config(_)),
            "expected local configuration error, got: {msg}"
        );
        assert!(msg.contains("not linked to an Oak remote"), "error: {msg}");
        assert!(msg.contains("oak push --repo <org>/<repo>"), "error: {msg}");
        assert!(
            !msg.contains("Server error"),
            "local setup error should not look like a server failure: {msg}"
        );
    }
}
