use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use dialoguer::Confirm;
use gix::ObjectId;
use oak_core::{Branch, Commit, FileMode, Hash, ManifestEntry, MetadataKey, OakError, Result};
use oak_core::{BulkImporter, Repository, SqliteRepository};

use crate::output;

/// Heuristic: does this string look like a git remote URL we should hand off
/// to the system `git` binary, rather than treat as an `owner/name` oak spec?
///
/// The patterns cover the common SSH and HTTPS forms that GitHub, GitLab,
/// Bitbucket, and self-hosted git servers use.
pub fn looks_like_git_url(s: &str) -> bool {
    s.starts_with("git@")
        || s.starts_with("ssh://")
        || s.starts_with("git://")
        || s.ends_with(".git")
        || s.starts_with("https://github.com/")
        || s.starts_with("https://gitlab.com/")
        || s.starts_with("https://bitbucket.org/")
        || s.starts_with("http://github.com/")
        || s.starts_with("http://gitlab.com/")
        || s.starts_with("http://bitbucket.org/")
}

/// Pull the leaf repo name from a git URL, stripping any trailing `.git`.
/// Examples:
///   `git@github.com:user/repo.git`     -> `repo`
///   `https://github.com/user/repo.git` -> `repo`
///   `https://github.com/user/repo`     -> `repo`
fn derive_dest_dir_from_git_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let leaf = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    leaf.strip_suffix(".git").unwrap_or(leaf).to_string()
}

/// Clone a git repository, then convert its full history into an oak repository.
///
/// Steps:
///   1. Run `git clone <url> <dest>` via the system git binary.
///   2. Initialize an oak repo at `<dest>` (.oak directory + main branch).
///   3. Walk the git commit graph (parents-before-children) and replay every
///      commit as an oak commit on `main`, copying blob contents into oak's
///      content-addressed storage. Merge commits keep their two parents.
///   4. Prompt the user to remove the .git directory.
pub fn run(url: &str, dest: Option<PathBuf>, cwd: &Path) -> Result<()> {
    let dest = dest.unwrap_or_else(|| PathBuf::from(derive_dest_dir_from_git_url(url)));
    let full_dest = if dest.is_absolute() {
        dest
    } else {
        cwd.join(dest)
    };

    if full_dest.exists()
        && full_dest
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    {
        return Err(OakError::Server(format!(
            "Destination '{}' already exists and is not empty",
            full_dest.display()
        )));
    }

    // 1. git clone
    output::info(&format!("Cloning git repository '{url}'..."));
    let status = Command::new("git")
        .arg("clone")
        .arg(url)
        .arg(&full_dest)
        .status()
        .map_err(|e| {
            OakError::Server(format!(
                "Failed to invoke `git clone` (is git installed and on PATH?): {e}"
            ))
        })?;
    if !status.success() {
        return Err(OakError::Server(format!(
            "`git clone` failed with status {status}"
        )));
    }

    // 2. Initialize an empty oak repo. Reproduce the relevant init steps
    //    inline rather than calling `init::run` so we don't trigger the
    //    interactive `.oakignore` prompt — the user just answered an
    //    interactive `git clone` and another prompt mid-flow is jarring.
    let oak_dir = full_dest.join(".oak");
    if oak_dir.exists() {
        return Err(OakError::RepoAlreadyExists);
    }
    fs::create_dir_all(&oak_dir)?;
    let db_path = oak_dir.join("oak.db");
    let oak_repo = SqliteRepository::open(&db_path)?;
    let repo_name = full_dest
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unnamed".to_string());
    oak_repo.set_metadata(MetadataKey::RepoName, &repo_name)?;

    // `main` is the import target — `convert_history` writes every imported
    // commit with `branch_name = "main"`. It's a transient local artifact
    // (main is conceptually server-only); push knows to bootstrap-push
    // these commits when the user runs `oak push` from their personal
    // branch.
    let main_branch = Branch::new("main".to_string(), None, None);
    oak_repo.store_branch(&main_branch)?;

    // 3. Convert the git history. If the repo has no commits we end up with
    //    an empty oak repo, which is a valid state.
    let converted = convert_history(&full_dest, &oak_repo)?;

    // 4. Create the user's personal branch parented onto main, fast-forward
    //    it to main's tip, and switch to it. Matches `oak init`'s import
    //    flow so both git-import entry points leave the user in the same
    //    state.
    let branch_name = crate::commands::init::default_local_branch_name();
    let personal_branch = Branch::new(branch_name.clone(), None, Some("main".to_string()));
    oak_repo.store_branch(&personal_branch)?;
    if converted > 0 {
        if let Some(tip) = oak_repo.get_branch_head("main")? {
            oak_repo.set_branch_head(&branch_name, &tip)?;
            oak_repo.set_head(&tip)?;

            // Materialize the working tree from oak's manifest so it matches
            // what we actually stored. `git clone` left the tree as git
            // *checked it out* — with `.gitattributes` smudge filters applied
            // (e.g. `*.bat eol=crlf` writes CRLF while the object/our blob is
            // LF) and submodule directories populated — but `copy_tree_to_oak`
            // builds the manifest from git's raw blobs and drops gitlinks.
            // Without this pass those differences show up as a fully dirty
            // working tree on the very first `oak status`. `update_working_dir`
            // rewrites every tracked file from our blob and prunes untracked
            // leftovers (submodule dirs, empty dirs), leaving status clean.
            if let Some(commit) = oak_repo.get_commit(&tip)? {
                if let Some(manifest) = oak_repo.get_manifest(&commit.manifest_hash)? {
                    let lock = crate::workdir_lock::WorkdirLock::acquire(&oak_dir)?;
                    crate::commands::switch::update_working_dir(
                        &lock, &full_dest, &oak_repo, &manifest,
                    )?;
                }
            }
        }
    }
    oak_repo.set_current_branch(&branch_name)?;

    if converted == 0 {
        output::info("Git repository has no commits; oak repo initialized empty.");
    } else {
        output::success(&format!("Imported {converted} commit(s) from git into oak"));
    }
    output::item(&format!(
        "Working on branch {}{}{} (parented onto main)",
        output::colors::CYAN,
        branch_name,
        output::colors::RESET,
    ));

    // 4. Offer to remove the .git directory now that the history lives in oak.
    //    Skip the prompt entirely when stdin isn't a terminal (CI, scripts);
    //    leaving .git in place is the safe default.
    let git_dir = full_dest.join(".git");
    if git_dir.exists() {
        output::blank();
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            let prompt = format!(
                "Conversion complete. Would you like to clean up the temporary '{}' directory?",
                git_dir.display()
            );
            let remove = Confirm::new()
                .with_prompt(prompt)
                .default(false)
                .interact()
                .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
            if remove {
                fs::remove_dir_all(&git_dir)?;
                output::success(&format!("Removed {}", git_dir.display()));
            } else {
                output::info(&format!(
                    "Left '{}' in place. You can delete it later with `rm -rf {}`.",
                    git_dir.display(),
                    git_dir.display()
                ));
            }
        } else {
            output::info(&format!(
                "Conversion complete. The git directory at '{}' is no longer needed and can be deleted with `rm -rf {}`.",
                git_dir.display(),
                git_dir.display()
            ));
        }
    }

    Ok(())
}

/// Walk every reachable commit from HEAD in parents-before-children order and
/// replay it as an oak commit on the `main` branch. Returns the number of
/// commits converted (0 for empty repos).
///
/// Called from both `oak clone <git-url>` and `oak init` (when an existing
/// `.git` directory is detected at the init path). Caller is responsible for
/// having already created the `main` branch row in the oak repo — this
/// function only sets its head and the HEAD pointer.
pub(crate) fn convert_history(work_tree: &Path, oak_repo: &SqliteRepository) -> Result<usize> {
    let git = gix::open(work_tree).map_err(|e| OakError::Git(e.to_string()))?;

    // Resolve HEAD. Bare repos / empty repos return None and we just bail with
    // an empty oak history.
    let head = git.head().map_err(|e| OakError::Git(e.to_string()))?;
    let head_id = match head.try_into_peeled_id() {
        Ok(Some(id)) => id.detach(),
        Ok(None) | Err(_) => return Ok(0),
    };

    let ordered = topological_walk(&git, head_id)?;
    if ordered.is_empty() {
        return Ok(0);
    }

    let pb = indicatif::ProgressBar::new(ordered.len() as u64);
    pb.set_style(
        indicatif::ProgressStyle::default_bar()
            .template("  Converting [{bar:30.cyan/dim}] {pos}/{len} commits")
            .unwrap()
            .progress_chars("━╸─"),
    );

    // git_oid -> oak_hash, so subsequent commits can wire up parent links.
    let mut commit_map: HashMap<ObjectId, Hash> = HashMap::new();
    // git_blob_oid -> oak BLAKE3 hash. Same git blob across commits is the
    // same content, so this skips re-hashing when files don't change.
    let mut blob_map: HashMap<ObjectId, Hash> = HashMap::new();
    let mut tip: Option<Hash> = None;

    // Bulk-write through a dedicated importer: amortizes WAL fsyncs over a
    // ~1000-commit batch and caches inserted tree hashes in memory so deeply
    // repetitive subtrees skip the `SELECT EXISTS` roundtrip on every commit.
    let mut importer = oak_repo.bulk_importer(1000)?;

    for git_oid in &ordered {
        let obj = git
            .find_object(*git_oid)
            .map_err(|e| OakError::Git(e.to_string()))?;
        let commit = obj
            .try_into_commit()
            .map_err(|e| OakError::Git(e.to_string()))?;

        let tree_id = commit
            .tree_id()
            .map_err(|e| OakError::Git(e.to_string()))?
            .detach();

        // Build the manifest by walking the tree and copying every blob into
        // oak's content-addressed storage.
        let mut entries = Vec::new();
        copy_tree_to_oak(
            &git,
            tree_id,
            "",
            &mut entries,
            &mut importer,
            &mut blob_map,
        )?;
        let manifest_hash = importer.put_tree(entries)?;

        // Map git parents to oak parents. Oak's commit model carries up to two
        // parents (parent + merge_parent); octopus merges (3+) lose the extras.
        let parent_oids: Vec<ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();
        let parent_hash = parent_oids.first().and_then(|p| commit_map.get(p).cloned());
        let merge_parent_hash = parent_oids.get(1).and_then(|p| commit_map.get(p).cloned());

        let sig = commit.author().map_err(|e| OakError::Git(e.to_string()))?;
        let author = format!("{} <{}>", sig.name, sig.email);
        let message = commit
            .message_raw()
            .ok()
            .map(|m| m.to_string())
            .filter(|s| !s.trim().is_empty());
        let timestamp = commit
            .time()
            .ok()
            .and_then(|t| chrono::DateTime::from_timestamp(t.seconds, 0))
            .unwrap_or_else(chrono::Utc::now);

        // Build the Commit explicitly (rather than `put_commit`) so the original
        // git timestamp is preserved in the commit hash. The default sqlite
        // `put_commit` re-stamps `Utc::now()`, which would make every imported
        // commit show today's date.
        //
        // `files` is left empty: the manifest already captures the full state
        // and oak's diff tooling derives per-file changes from manifest pairs
        // when needed. Computing real diffs here would multiply the import
        // cost without changing behavior the user can observe.
        let commit_obj = Commit::with_timestamp(
            "main".to_string(),
            parent_hash,
            merge_parent_hash,
            manifest_hash,
            author,
            message,
            Vec::new(),
            timestamp,
        )?;
        let oak_hash = commit_obj.hash.clone();
        importer.store_commit(&commit_obj)?;

        commit_map.insert(*git_oid, oak_hash.clone());
        tip = Some(oak_hash);
        pb.inc(1);
    }
    importer.finish()?;
    pb.finish_and_clear();

    if let Some(ref h) = tip {
        oak_repo.set_branch_head("main", h)?;
        oak_repo.set_head(h)?;
    }

    Ok(ordered.len())
}

/// Iterative post-order DFS over the commit graph: yields each commit only
/// after all its parents have been yielded. Iterative (not recursive) so very
/// deep histories don't blow the stack.
fn topological_walk(repo: &gix::Repository, head: ObjectId) -> Result<Vec<ObjectId>> {
    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut order: Vec<ObjectId> = Vec::new();
    // Each stack entry is (oid, processed). First pass schedules parents;
    // second pass (after parents emit) appends the oid itself.
    let mut stack: Vec<(ObjectId, bool)> = vec![(head, false)];

    while let Some((oid, processed)) = stack.pop() {
        if processed {
            order.push(oid);
            continue;
        }
        if !visited.insert(oid) {
            continue;
        }
        stack.push((oid, true));
        let obj = repo
            .find_object(oid)
            .map_err(|e| OakError::Git(e.to_string()))?;
        let commit = obj
            .try_into_commit()
            .map_err(|e| OakError::Git(e.to_string()))?;
        for p in commit.parent_ids() {
            let p_oid = p.detach();
            if !visited.contains(&p_oid) {
                stack.push((p_oid, false));
            }
        }
    }

    Ok(order)
}

/// Walk a git tree recursively, copy each blob into oak storage, and append a
/// `ManifestEntry` per file. Submodules and other non-blob entries are skipped.
fn copy_tree_to_oak(
    git: &gix::Repository,
    tree_id: ObjectId,
    prefix: &str,
    entries: &mut Vec<ManifestEntry>,
    importer: &mut BulkImporter,
    blob_map: &mut HashMap<ObjectId, Hash>,
) -> Result<()> {
    let obj = git
        .find_object(tree_id)
        .map_err(|e| OakError::Git(e.to_string()))?;
    let tree = obj
        .try_into_tree()
        .map_err(|e| OakError::Git(e.to_string()))?;

    for entry_res in tree.iter() {
        let entry = entry_res.map_err(|e| OakError::Git(e.to_string()))?;
        let name = entry.filename().to_string();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let mode = entry.mode();

        if mode.is_tree() {
            copy_tree_to_oak(git, entry.id().detach(), &path, entries, importer, blob_map)?;
        } else if mode.is_blob() || mode.is_link() {
            let oak_mode = if mode.is_executable() {
                FileMode::Executable
            } else if mode.is_link() {
                FileMode::Symlink
            } else {
                FileMode::Regular
            };
            let blob_oid = entry.id().detach();
            let blob_hash = if let Some(cached) = blob_map.get(&blob_oid) {
                cached.clone()
            } else {
                let blob_obj = git
                    .find_object(blob_oid)
                    .map_err(|e| OakError::Git(e.to_string()))?;
                let content = blob_obj.data.to_vec();
                let h = importer.put_blob(content)?;
                blob_map.insert(blob_oid, h.clone());
                h
            };
            entries.push(ManifestEntry {
                path,
                blob_hash,
                mode: oak_mode,
            });
        }
        // Submodules (gitlinks) are intentionally dropped — oak has no
        // submodule concept, and following them would require a recursive
        // network fetch we don't want to do silently.
    }

    Ok(())
}
