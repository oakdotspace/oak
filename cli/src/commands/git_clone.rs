use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use dialoguer::Confirm;
use gix::ObjectId;
use oak_core::{
    Branch, Commit, FileMode, Hash, MetadataKey, OakError, Result, Tree, TreeEntry, TreeEntryKind,
};
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
    // git_tree_oid -> converted oak tree hash (None = the tree has no
    // oak-visible entries, e.g. it held only submodules). Git trees are
    // immutable and content-addressed, so an unchanged subtree across commits
    // — the bulk of every commit, since most touch only a handful of files —
    // is a single cache hit instead of a full re-walk + re-hash. This is what
    // turns the import from O(commits × files) into O(unique git trees).
    let mut tree_map: HashMap<ObjectId, Option<Hash>> = HashMap::new();
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

        // Convert the commit's root tree into oak tree objects, reusing every
        // subtree we've already seen this import via `tree_map`. A `None` root
        // means the tree had no oak-visible entries (e.g. submodules only); we
        // store the canonical empty tree so the commit still references it.
        let manifest_hash =
            match convert_git_tree(&git, tree_id, &mut importer, &mut blob_map, &mut tree_map)? {
                Some(hash) => hash,
                None => importer.put_tree(Vec::new())?,
            };

        // Map git parents to oak parents. Oak's commit model carries up to two
        // parents (parent + merge_parent); octopus merges (3+) lose the extras.
        let parent_oids: Vec<ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();
        let parent_hash = parent_oids.first().and_then(|p| commit_map.get(p).cloned());
        let merge_parent_hash = parent_oids.get(1).and_then(|p| commit_map.get(p).cloned());

        let sig = commit.author().map_err(|e| OakError::Git(e.to_string()))?;
        let author = sanitize_commit_field(format!("{} <{}>", sig.name, sig.email));
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

/// Convert a git tree (and everything under it) into oak tree objects, copying
/// each new blob into oak storage, and return the oak hash of the resulting
/// tree. Returns `None` when the tree has no oak-visible entries — every child
/// was a submodule (or an empty such subtree) — so the parent omits it, exactly
/// as `build_tree` omits a directory with no files.
///
/// Memoized on the git tree oid. Git trees are content-addressed and immutable,
/// so the first time we see an oid we walk it; every later sighting (the common
/// case — consecutive commits share all but a few subtrees) returns the cached
/// oak hash without re-reading gix, rebuilding `TreeEntry`s, or re-hashing.
/// Because each subtree is hashed and stored exactly the way `build_tree` would
/// build it from the equivalent flat manifest, the resulting commit
/// `manifest_hash` is byte-for-byte identical to the previous flat-walk path.
fn convert_git_tree(
    git: &gix::Repository,
    tree_id: ObjectId,
    importer: &mut BulkImporter,
    blob_map: &mut HashMap<ObjectId, Hash>,
    tree_map: &mut HashMap<ObjectId, Option<Hash>>,
) -> Result<Option<Hash>> {
    if let Some(cached) = tree_map.get(&tree_id) {
        return Ok(cached.clone());
    }

    let obj = git
        .find_object(tree_id)
        .map_err(|e| OakError::Git(e.to_string()))?;
    let tree = obj
        .try_into_tree()
        .map_err(|e| OakError::Git(e.to_string()))?;

    let mut tree_entries: Vec<TreeEntry> = Vec::new();
    for entry_res in tree.iter() {
        let entry = entry_res.map_err(|e| OakError::Git(e.to_string()))?;
        let name = entry.filename().to_string();
        let mode = entry.mode();

        if mode.is_tree() {
            // Recurse; a subtree that converts to nothing (submodules only) is
            // omitted from this tree, matching build_tree's empty-dir handling.
            if let Some(sub_hash) =
                convert_git_tree(git, entry.id().detach(), importer, blob_map, tree_map)?
            {
                tree_entries.push(TreeEntry {
                    name,
                    kind: TreeEntryKind::Tree,
                    hash: sub_hash,
                    mode: FileMode::Regular,
                });
            }
        } else if let Some(oak_mode) = git_file_mode_to_oak(mode) {
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
            tree_entries.push(TreeEntry {
                name,
                kind: TreeEntryKind::Blob,
                hash: blob_hash,
                mode: oak_mode,
            });
        }
        // Submodules (gitlinks) are intentionally dropped — oak has no
        // submodule concept, and following them would require a recursive
        // network fetch we don't want to do silently.
    }

    let result = if tree_entries.is_empty() {
        None
    } else {
        // `Tree::new` sorts + hashes exactly as `build_tree`'s per-directory
        // recursion does, so the hash matches the old flat path bit for bit.
        let oak_tree = Tree::new(tree_entries)?;
        let hash = oak_tree.hash.clone();
        importer.put_tree_node(&oak_tree)?;
        Some(hash)
    };
    tree_map.insert(tree_id, result.clone());
    Ok(result)
}

/// Strip control characters from a git identity before it becomes an oak commit
/// author. Git lets author/committer names carry arbitrary bytes, and real
/// histories contain legacy-encoded names with embedded control characters
/// (e.g. git.git has an author with ISO-2022 escape sequences). Oak forbids
/// control characters in the author field — they'd make the `\n`-joined commit
/// hash preimage ambiguous — so without this one such commit aborts the entire
/// import. Stripping is deterministic, so re-imports stay stable.
fn sanitize_commit_field(value: String) -> String {
    if value.chars().any(char::is_control) {
        value.chars().filter(|c| !c.is_control()).collect()
    } else {
        value
    }
}

fn git_file_mode_to_oak(mode: gix::objs::tree::EntryMode) -> Option<FileMode> {
    if mode.is_executable() {
        Some(FileMode::Executable)
    } else if mode.is_link() {
        Some(FileMode::Symlink)
    } else if mode.is_blob() {
        Some(FileMode::Regular)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::objs::tree::EntryKind;

    #[test]
    fn git_file_mode_to_oak_preserves_file_modes() {
        assert_eq!(
            git_file_mode_to_oak(EntryKind::Blob.into()),
            Some(FileMode::Regular)
        );
        assert_eq!(
            git_file_mode_to_oak(EntryKind::BlobExecutable.into()),
            Some(FileMode::Executable)
        );
        assert_eq!(
            git_file_mode_to_oak(EntryKind::Link.into()),
            Some(FileMode::Symlink)
        );
    }

    #[test]
    fn git_file_mode_to_oak_skips_non_file_modes() {
        assert_eq!(git_file_mode_to_oak(EntryKind::Tree.into()), None);
        assert_eq!(git_file_mode_to_oak(EntryKind::Commit.into()), None);
    }

    #[test]
    fn sanitize_commit_field_strips_control_chars() {
        // git.git's real offender: an author with ISO-2022 escape sequences.
        let dirty = "Ville Skytt\u{1b},Ad\u{1b}(B <scop@xemacs.org>".to_string();
        let clean = sanitize_commit_field(dirty);
        assert!(!clean.chars().any(char::is_control));
        assert_eq!(clean, "Ville Skytt,Ad(B <scop@xemacs.org>");
        // A clean identity passes through untouched (and allocation-free path).
        let ok = "Jane Doe <jane@example.com>".to_string();
        assert_eq!(sanitize_commit_field(ok.clone()), ok);
    }

    /// `convert_git_tree` builds oak trees bottom-up with `Tree::new`, while the
    /// old path flattened to a manifest and called `build_tree`. They must hash
    /// identically or imported commits would get different `manifest_hash`es and
    /// break compatibility with already-pushed repos. Pin that equivalence.
    #[test]
    fn recursive_tree_construction_matches_build_tree() {
        use oak_core::{build_tree, hash_string, ManifestEntry};

        let h_c = hash_string("c-contents");
        let h_d = hash_string("d-contents");
        let h_e = hash_string("e-contents");

        // Flat manifest for: a/b/c.txt (regular), a/d.txt (exe), e.txt (regular).
        let flat = vec![
            ManifestEntry {
                path: "a/b/c.txt".into(),
                blob_hash: h_c.clone(),
                mode: FileMode::Regular,
            },
            ManifestEntry {
                path: "a/d.txt".into(),
                blob_hash: h_d.clone(),
                mode: FileMode::Executable,
            },
            ManifestEntry {
                path: "e.txt".into(),
                blob_hash: h_e.clone(),
                mode: FileMode::Regular,
            },
        ];
        let flat_root = build_tree(&flat).unwrap().root_hash;

        // The same structure assembled the way `convert_git_tree` does, leaves up.
        let tree_b = Tree::new(vec![TreeEntry {
            name: "c.txt".into(),
            kind: TreeEntryKind::Blob,
            hash: h_c,
            mode: FileMode::Regular,
        }])
        .unwrap();
        let tree_a = Tree::new(vec![
            TreeEntry {
                name: "b".into(),
                kind: TreeEntryKind::Tree,
                hash: tree_b.hash,
                mode: FileMode::Regular,
            },
            TreeEntry {
                name: "d.txt".into(),
                kind: TreeEntryKind::Blob,
                hash: h_d,
                mode: FileMode::Executable,
            },
        ])
        .unwrap();
        let root = Tree::new(vec![
            TreeEntry {
                name: "a".into(),
                kind: TreeEntryKind::Tree,
                hash: tree_a.hash,
                mode: FileMode::Regular,
            },
            TreeEntry {
                name: "e.txt".into(),
                kind: TreeEntryKind::Blob,
                hash: h_e,
                mode: FileMode::Regular,
            },
        ])
        .unwrap();

        assert_eq!(
            root.hash, flat_root,
            "recursive Tree::new construction must hash-match build_tree"
        );
    }
}
