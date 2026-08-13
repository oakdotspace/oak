//! `oak export` — replay an oak repository's history into a fresh git repo.
//!
//! Inverse of `oak clone <git-url>`. Walks linear ancestry from the chosen
//! oak branch's HEAD, materializes each commit's manifest into the working
//! tree of a brand-new git repo, and runs `git commit` with the original
//! oak author + timestamp so the resulting git history preserves attribution.
//!
//! Limitations (v1, intentional):
//!   - Linear ancestry only: `merge_parent_hash` is dropped. Squash-merge
//!     commits on `main` still export, but the "pre-squash" branch tip they
//!     point at is not stitched into the git graph.
//!   - Symlinks are best-effort on Windows.
//!   - One branch per export. To export multiple branches into one git repo,
//!     run `oak export` again into the same destination on a different branch
//!     (manually — flag not exposed).

use std::fs;
use std::path::{Component, Path};
use std::process::Command;

use indicatif::{ProgressBar, ProgressStyle};
use oak_core::{FileMode, Hash, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::output;

pub fn run(
    cwd: &Path,
    dest: &Path,
    branch_override: Option<&str>,
    branch_name_override: Option<&str>,
    force: bool,
) -> Result<()> {
    // 1. Open local oak repo.
    let ctx = crate::resolve::resolve(cwd)?;
    let oak_repo = SqliteRepository::open(&ctx.db_path()?)?;

    // 2. Resolve which oak branch to export.
    let branch_name = match branch_override {
        Some(n) => n.to_string(),
        None => oak_repo.get_current_branch_name()?.ok_or_else(|| {
            OakError::Server(
                "No current branch set; pass --branch <NAME> to choose one explicitly.".into(),
            )
        })?,
    };
    let head = oak_repo.get_branch_head(&branch_name)?.ok_or_else(|| {
        OakError::Server(format!("Branch '{branch_name}' exists but has no commits"))
    })?;

    // 3. Resolve destination.
    let dest = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        cwd.join(dest)
    };
    if dest.exists() {
        let non_empty = dest
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if non_empty && !force {
            return Err(OakError::Server(format!(
                "Destination '{}' already exists and is not empty (pass --force to write into it anyway)",
                dest.display()
            )));
        }
    } else {
        fs::create_dir_all(&dest)?;
    }

    // 4. Walk linear ancestry HEAD → ... → root (oldest last in this vec).
    let mut ancestry: Vec<Hash> = Vec::new();
    let mut cursor = Some(head);
    while let Some(h) = cursor {
        let commit = oak_repo
            .get_commit(&h)?
            .ok_or_else(|| OakError::Server(format!("Missing commit {h}", h = h.as_str())))?;
        ancestry.push(h);
        cursor = commit.parent_hash;
    }
    ancestry.reverse(); // oldest first
    if ancestry.is_empty() {
        return Err(OakError::Server(format!(
            "Branch '{branch_name}' has no commits to export"
        )));
    }

    // 5. `git init`.
    let git_branch_name = branch_name_override.unwrap_or(&branch_name);
    let status = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(format!("--initial-branch={git_branch_name}"))
        .arg(&dest)
        .status()
        .map_err(|e| {
            OakError::Server(format!(
                "Failed to invoke `git init` (is git installed and on PATH?): {e}"
            ))
        })?;
    if !status.success() {
        return Err(OakError::Server(format!(
            "`git init` failed with status {status}"
        )));
    }

    output::info(&format!(
        "Exporting {} commit(s) from oak branch '{}' to git repo at '{}'...",
        ancestry.len(),
        branch_name,
        dest.display()
    ));

    // 6. Replay each commit oldest → newest.
    let pb = ProgressBar::new(ancestry.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  Exporting [{bar:30.cyan/dim}] {pos}/{len} commits")
            .unwrap()
            .progress_chars("━╸─"),
    );

    for hash in &ancestry {
        let commit = oak_repo.get_commit(hash)?.ok_or_else(|| {
            OakError::Server(format!("Commit {} disappeared mid-walk", hash.as_str()))
        })?;
        let manifest = oak_repo
            .get_manifest(&commit.manifest_hash)?
            .ok_or_else(|| {
                OakError::Server(format!(
                    "Manifest {} missing for commit {}",
                    commit.manifest_hash.as_str(),
                    hash.as_str()
                ))
            })?;
        reject_git_internal_paths(&manifest)?;

        // a. Wipe the working tree (preserving .git) so the new manifest
        //    drives the snapshot exactly — handles deletes/renames cleanly.
        clear_working_tree(&dest)?;

        // b. Materialize every manifest entry.
        for entry in &manifest.entries {
            let full = dest.join(&entry.path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent)?;
            }
            let blob = oak_repo.get_blob(&entry.blob_hash)?.ok_or_else(|| {
                OakError::Server(format!(
                    "Blob {} missing for path '{}'",
                    entry.blob_hash.as_str(),
                    entry.path
                ))
            })?;
            write_entry(&full, &blob.content, entry.mode)?;
        }

        // c. Stage and commit. `git add -A` walks the work tree and stages
        //    every change including deletions, which is the only way to make
        //    the wipe-and-rewrite step register correctly.
        let status = Command::new("git")
            .arg("-C")
            .arg(&dest)
            .args(["add", "-A"])
            .status()?;
        if !status.success() {
            return Err(OakError::Server(format!(
                "`git add -A` failed at commit {}",
                hash.short()
            )));
        }

        let (author_name, author_email) = parse_author(&commit.author);
        let message = commit
            .message
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| format!("oak: {}", hash.short()));
        let ts = commit.timestamp.format("%Y-%m-%d %H:%M:%S %z").to_string();

        let status = Command::new("git")
            .arg("-C")
            .arg(&dest)
            .args(["commit", "--quiet", "--allow-empty", "-m"])
            .arg(&message)
            .env("GIT_AUTHOR_NAME", &author_name)
            .env("GIT_AUTHOR_EMAIL", &author_email)
            .env("GIT_AUTHOR_DATE", &ts)
            .env("GIT_COMMITTER_NAME", &author_name)
            .env("GIT_COMMITTER_EMAIL", &author_email)
            .env("GIT_COMMITTER_DATE", &ts)
            .status()?;
        if !status.success() {
            return Err(OakError::Server(format!(
                "`git commit` failed at oak commit {}",
                hash.short()
            )));
        }

        pb.inc(1);
    }
    pb.finish_and_clear();

    output::success(&format!(
        "Exported {} commit(s) to git branch '{}' at {}",
        ancestry.len(),
        git_branch_name,
        dest.display()
    ));
    output::info("Next: `cd` into the destination and run `git log` to verify, or `git remote add` + `git push` to publish.");

    Ok(())
}

fn reject_git_internal_paths(manifest: &oak_core::Manifest) -> Result<()> {
    if let Some(entry) = manifest
        .entries
        .iter()
        .find(|entry| is_git_internal_path(&entry.path))
    {
        return Err(OakError::InvalidPath(format!(
            "cannot export path '{}' because it would write inside the destination .git directory",
            entry.path
        )));
    }
    Ok(())
}

fn is_git_internal_path(path: &str) -> bool {
    let mut components = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                components.pop();
            }
            Component::Normal(part) => components.push(part),
            Component::Prefix(_) | Component::RootDir => return true,
        }
    }
    components.first() == Some(&std::ffi::OsStr::new(".git"))
}

/// Delete every top-level entry in `dest` except `.git`.
fn clear_working_tree(dest: &Path) -> Result<()> {
    for entry in fs::read_dir(dest)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        // `symlink_metadata` doesn't follow links, so dangling symlinks (rare
        // in well-formed manifests but possible) still get cleaned up.
        let md = fs::symlink_metadata(&path)?;
        if md.file_type().is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Write a single manifest entry's content at `full`. Handles regular files,
/// executable bit, and symlinks (POSIX only; Windows falls back to writing
/// the target as a regular file).
fn write_entry(full: &Path, content: &[u8], mode: FileMode) -> Result<()> {
    // If the path already exists (it shouldn't after `clear_working_tree`,
    // but the manifest could in principle name two entries colliding under
    // case-insensitive filesystems), remove it first.
    if fs::symlink_metadata(full).is_ok() {
        let _ = fs::remove_file(full);
    }
    match mode {
        FileMode::Regular => {
            fs::write(full, content)?;
        }
        FileMode::Executable => {
            fs::write(full, content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(full)?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(full, perms)?;
            }
        }
        FileMode::Symlink => {
            let target = String::from_utf8_lossy(content);
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(target.as_ref(), full)?;
            }
            #[cfg(windows)]
            {
                // Windows symlinks need admin or developer-mode; storing the
                // target string as a regular file is what git itself does
                // when `core.symlinks=false`.
                fs::write(full, target.as_bytes())?;
            }
        }
    }
    Ok(())
}

/// Split oak's freeform `author` string into (name, email). Accepts:
///   - "Name <email>"        → (Name, email)
///   - "<email>"             → (email, email)
///   - "Name"                → (Name, name-slug@oak.local)
///
/// Git insists on a `<email>` component in commit objects, so we synthesize
/// a placeholder when the source had none.
fn parse_author(s: &str) -> (String, String) {
    let trimmed = s.trim();
    if let Some(open) = trimmed.rfind('<') {
        if let Some(rel_close) = trimmed[open..].find('>') {
            let email = trimmed[open + 1..open + rel_close].trim().to_string();
            let name = trimmed[..open].trim().to_string();
            if !email.is_empty() {
                let name = if name.is_empty() { email.clone() } else { name };
                return (name, email);
            }
        }
    }
    if trimmed.is_empty() {
        return ("Oak User".to_string(), "user@oak.local".to_string());
    }
    let slug: String = trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '.'
            }
        })
        .collect();
    // Collapse repeated dots so we don't end up with "first...last".
    let slug = collapse_dots(&slug);
    (trimmed.to_string(), format!("{slug}@oak.local"))
}

fn collapse_dots(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_dot = false;
    for c in s.chars() {
        if c == '.' {
            if !last_was_dot {
                out.push('.');
            }
            last_was_dot = true;
        } else {
            out.push(c);
            last_was_dot = false;
        }
    }
    out.trim_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::{hash_string, Manifest, ManifestEntry};

    #[test]
    fn parse_author_name_and_email() {
        let (n, e) = parse_author("Ada Lovelace <ada@example.com>");
        assert_eq!(n, "Ada Lovelace");
        assert_eq!(e, "ada@example.com");
    }

    #[test]
    fn parse_author_just_email() {
        let (n, e) = parse_author("<ada@example.com>");
        assert_eq!(n, "ada@example.com");
        assert_eq!(e, "ada@example.com");
    }

    #[test]
    fn parse_author_just_name() {
        let (n, e) = parse_author("Ada Lovelace");
        assert_eq!(n, "Ada Lovelace");
        assert_eq!(e, "ada.lovelace@oak.local");
    }

    #[test]
    fn parse_author_blank() {
        let (n, e) = parse_author("   ");
        assert_eq!(n, "Oak User");
        assert_eq!(e, "user@oak.local");
    }

    #[test]
    fn parse_author_punctuation_collapses() {
        let (_, e) = parse_author("First   Middle    Last");
        assert_eq!(e, "first.middle.last@oak.local");
    }

    // Catches the empty-angle-brackets corner case: "Name <>" should fall
    // through to the no-email synthesis path, not produce an empty email.
    #[test]
    fn parse_author_empty_angles() {
        let (n, e) = parse_author("Ada <>");
        assert_eq!(n, "Ada <>");
        assert!(e.ends_with("@oak.local"));
    }

    fn manifest_with_path(path: &str) -> Manifest {
        Manifest::new(vec![ManifestEntry {
            path: path.to_string(),
            blob_hash: hash_string("content"),
            mode: FileMode::Regular,
        }])
    }

    #[test]
    fn export_rejects_paths_inside_destination_git_dir() {
        for path in [
            ".git",
            ".git/config",
            ".git/hooks/pre-commit",
            "./.git/config",
            "src/../.git/config",
        ] {
            let err = reject_git_internal_paths(&manifest_with_path(path)).unwrap_err();
            assert!(
                err.to_string().contains("destination .git directory"),
                "expected .git export guard for {path:?}, got {err}"
            );
        }
    }

    #[test]
    fn export_allows_git_named_siblings() {
        for path in [".github/workflows/ci.yml", ".gitignore", "src/.gitkeep"] {
            reject_git_internal_paths(&manifest_with_path(path)).unwrap();
        }
    }
}
