use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
pub(crate) fn derive_dest_dir_from_git_url(url: &str) -> String {
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
    let dest_existed = full_dest.exists();
    output::info(&format!("Cloning git repository '{url}'..."));
    let status = Command::new("git")
        .arg("clone")
        // `--`: a URL starting with `-` must never be read as an option.
        .arg("--")
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

    // 2–3. Turn the checkout into an Oak repository. On failure, remove what
    // this command created, so a retry starts from an empty destination.
    let converted: Result<()> = (|| {
        // 2. Initialize an empty oak repo. Reproduce the relevant init steps
        //    inline rather than calling `init::run` so we don't trigger the
        //    interactive `.oakignore` prompt — the user just answered an
        //    interactive `git clone` and another prompt mid-flow is jarring.
        // The destination was empty before `git clone`, so any `.oak/` here came
        // from the git tree itself, e.g. a tracked `.oak/workflows`. Apply the
        // same rule as `oak init` instead of refusing every such repository.
        let oak_dir = full_dest.join(".oak");
        if crate::commands::init::oak_dir_holds_repository(&oak_dir) {
            return Err(OakError::RepoAlreadyExists);
        }
        // `git clone` already wrote such entries to disk, where Oak trusts
        // them (`.oak/hooks/*` runs on `oak commit`), so refuse the repository
        // outright rather than leave them in a working Oak checkout.
        let untrusted = untrusted_oak_entries(&full_dest)?;
        if !untrusted.is_empty() {
            return Err(OakError::InvalidPath(format!(
                "refusing to import a git repository that tracks {}: only .oak/attributes, .oak/PERMISSIONS and .oak/workflows/ may come from a repository (.oak/hooks would run on `oak commit`).",
                untrusted.join(", ")
            )));
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
        let report = convert_history(&full_dest, &oak_repo)?;
        let converted = report.commits;

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
                // LF) and submodule directories populated — but `convert_history`
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
        report.warn_about_skips();
        Ok(())
    })();
    if let Err(e) = converted {
        let cleanup = if dest_existed {
            fs::read_dir(&full_dest).and_then(|entries| {
                for entry in entries {
                    let path = entry?.path();
                    if path.is_dir() && !path.is_symlink() {
                        fs::remove_dir_all(&path)?;
                    } else {
                        fs::remove_file(&path)?;
                    }
                }
                Ok(())
            })
        } else {
            fs::remove_dir_all(&full_dest)
        };
        if let Err(cleanup_err) = cleanup {
            output::warning(&format!(
                "Couldn't clean up {}: {cleanup_err}",
                full_dest.display()
            ));
        }
        return Err(e);
    }

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

/// What a freshly cloned git tree put under `.oak/` besides the files Oak
/// tracks there (`attributes`, `PERMISSIONS`, `workflows/`). The rest of that
/// directory is local state Oak trusts — notably `.oak/hooks/<event>`, which
/// `oak commit` executes — and git, unlike Oak, checks out whatever the remote
/// repository tracks.
pub(crate) fn untrusted_oak_entries(work_tree: &Path) -> Result<Vec<String>> {
    let Ok(entries) = fs::read_dir(work_tree.join(".oak")) else {
        return Ok(Vec::new());
    };
    let mut untrusted = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type()?;
        let tracked = match name.as_str() {
            "attributes" | "PERMISSIONS" => file_type.is_file(),
            "workflows" => file_type.is_dir(),
            _ => false,
        };
        if !tracked {
            untrusted.push(format!(".oak/{name}"));
        }
    }
    untrusted.sort();
    Ok(untrusted)
}

/// What an import brought across, and what it had to leave behind. Every
/// field after `commits` is something the git history held that Oak has no
/// place for; the import says so instead of failing on it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// Commits replayed onto `main`.
    pub commits: usize,
    /// Paths Oak can't track (see [`untrackable_reason`]), each with why.
    /// Keyed by the first place each one was seen.
    pub skipped_paths: BTreeMap<String, &'static str>,
    /// Submodule entries (gitlinks) left out.
    pub skipped_submodules: BTreeSet<String>,
    /// Commits that had three or more parents; Oak keeps the first two.
    pub octopus_merges: usize,
    /// The git repository was a shallow clone, so the oldest imported
    /// commits have no parents.
    pub shallow: bool,
}

impl ImportReport {
    /// Print what was left out, if anything, as warnings.
    pub fn warn_about_skips(&self) {
        const SHOWN: usize = 10;
        if !self.skipped_paths.is_empty() {
            output::warning(&format!(
                "Left out {} path(s) Oak can't track:",
                self.skipped_paths.len()
            ));
            for (path, why) in self.skipped_paths.iter().take(SHOWN) {
                output::item(&format!("{path:?}: {why}"));
            }
            if self.skipped_paths.len() > SHOWN {
                output::item(&format!("… and {} more", self.skipped_paths.len() - SHOWN));
            }
        }
        if !self.skipped_submodules.is_empty() {
            output::warning(&format!(
                "Left out {} submodule(s); Oak has no submodules: {}",
                self.skipped_submodules.len(),
                self.skipped_submodules
                    .iter()
                    .take(SHOWN)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if self.octopus_merges > 0 {
            output::warning(&format!(
                "{} merge commit(s) had more than two parents; Oak kept the first two.",
                self.octopus_merges
            ));
        }
        if self.shallow {
            output::warning(
                "The git repository is a shallow clone: history starts at its oldest fetched commits.",
            );
        }
    }

    /// The report as JSON, for `--json` receipts.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "imported_commit_count": self.commits,
            "skipped_paths": self
                .skipped_paths
                .iter()
                .map(|(path, why)| serde_json::json!({"path": path, "reason": why}))
                .collect::<Vec<_>>(),
            "skipped_submodules": self.skipped_submodules.iter().collect::<Vec<_>>(),
            "octopus_merge_count": self.octopus_merges,
            "shallow_git_history": self.shallow,
        })
    }
}

/// Open the git repository at `work_tree` and resolve its HEAD, so an import
/// that can't start fails before anything is created.
pub(crate) fn check_importable(work_tree: &Path) -> Result<()> {
    let git = open_git(work_tree)?;
    git.head()
        .map_err(|e| OakError::Git(format!("can't read HEAD of {}: {e}", work_tree.display())))?;
    Ok(())
}

fn open_git(work_tree: &Path) -> Result<gix::Repository> {
    gix::open(work_tree).map_err(|e| {
        OakError::Git(format!(
            "can't open the git repository at {}: {e}",
            work_tree.display()
        ))
    })
}

/// Replay the git history reachable from HEAD onto the local `main` branch.
///
/// Called from both `oak clone <git-url>` and `oak init --import-git`. The
/// caller has already created the `main` branch row; this sets its head and
/// the HEAD pointer. An empty repository (unborn HEAD) imports nothing.
///
/// Every commit keeps its author, message and committer time, and its first
/// two parents. Paths Oak can't track and submodules are left out of every
/// commit (the returned report lists them), so whatever the history holds,
/// each imported tree is one Oak can check out and push.
pub(crate) fn convert_history(
    work_tree: &Path,
    oak_repo: &SqliteRepository,
) -> Result<ImportReport> {
    let git = open_git(work_tree)?;
    let mut report = ImportReport::default();

    let head = git.head().map_err(|e| OakError::Git(e.to_string()))?;
    let head_id = match head.try_into_peeled_id() {
        Ok(Some(id)) => id.detach(),
        // Unborn HEAD: a repository with no commits yet.
        Ok(None) => return Ok(report),
        Err(e) => return Err(OakError::Git(format!("can't resolve HEAD: {e}"))),
    };

    // Commits whose parents a shallow clone never fetched. Their parents
    // are dropped instead of failing the walk on a missing object.
    let shallow: HashSet<ObjectId> = git
        .shallow_commits()
        .map_err(|e| OakError::Git(e.to_string()))?
        .map(|commits| commits.iter().copied().collect())
        .unwrap_or_default();
    report.shallow = !shallow.is_empty();

    let ordered = topological_walk(&git, head_id, &shallow)?;

    let pb = indicatif::ProgressBar::new(ordered.len() as u64);
    pb.set_style(
        indicatif::ProgressStyle::default_bar()
            .template("  Converting [{bar:30.cyan/dim}] {pos}/{len} commits")
            .unwrap()
            .progress_chars("━╸─"),
    );

    // git_oid -> oak_hash, so subsequent commits can wire up parent links.
    let mut commit_map: HashMap<ObjectId, Hash> = HashMap::new();
    let mut ctx = TreeConversion {
        git: &git,
        blob_map: HashMap::new(),
        tree_map: HashMap::new(),
        report: &mut report,
    };
    let mut tip: Option<Hash> = None;

    // Bulk-write through a dedicated importer: amortizes WAL fsyncs over a
    // ~1000-commit batch and caches inserted tree hashes in memory so deeply
    // repetitive subtrees skip the `SELECT EXISTS` roundtrip on every commit.
    let mut importer = oak_repo.bulk_importer(1000)?;

    for git_oid in &ordered {
        let commit = git
            .find_object(*git_oid)
            .map_err(|e| OakError::Git(e.to_string()))?
            .try_into_commit()
            .map_err(|e| OakError::Git(e.to_string()))?;

        let tree_id = commit
            .tree_id()
            .map_err(|e| OakError::Git(e.to_string()))?
            .detach();

        // A `None` root means the tree had no entries Oak keeps (empty, or
        // only submodules); the commit still needs a tree, so store the
        // canonical empty one.
        let manifest_hash = match ctx.convert_tree(tree_id, "", &mut importer)? {
            Some(hash) => hash,
            None => importer.put_tree(Vec::new())?,
        };

        // Oak's commit carries up to two parents (parent + merge_parent).
        // Parents beyond a shallow boundary were never fetched, so they
        // aren't in the map and drop out here.
        let parent_oids: Vec<ObjectId> = if shallow.contains(git_oid) {
            Vec::new()
        } else {
            commit.parent_ids().map(|p| p.detach()).collect()
        };
        if parent_oids.len() > 2 {
            ctx.report.octopus_merges += 1;
        }
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
        // when needed; log also derives its display/filter index lazily. This
        // keeps import bounded and repairs display for existing imports.
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

    report.commits = ordered.len();
    Ok(report)
}

/// Iterative post-order DFS over the commit graph: yields each commit only
/// after all its parents have been yielded. Iterative (not recursive) so very
/// deep histories don't blow the stack. Parents of `shallow` commits are not
/// followed: a shallow clone doesn't have them.
fn topological_walk(
    repo: &gix::Repository,
    head: ObjectId,
    shallow: &HashSet<ObjectId>,
) -> Result<Vec<ObjectId>> {
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
        if shallow.contains(&oid) {
            continue;
        }
        let commit = repo
            .find_object(oid)
            .map_err(|e| OakError::Git(format!("commit {oid} is missing: {e}")))?
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

/// Why Oak can't track the git entry `name` at `path` (its full path in the
/// repository), or `None` when it can. Git allows names Oak refuses: names
/// that aren't UTF-8, that hold control characters, a `.git` or `.oakroot`
/// component (git refuses `.git` too, but crafted histories carry it), or
/// anything under `.oak/` except the files Oak tracks there. The rule is
/// [`oak_core::worktree_path_problem`]'s, minus its Windows-only checks.
fn untrackable_reason(path: &str) -> Option<&'static str> {
    let components: Vec<&str> = path.split('/').collect();
    for (index, component) in components.iter().enumerate() {
        if oak_core::validate_entry_name(component).is_err() {
            return Some("not a valid Oak file name");
        }
        if oak_core::is_metadata_dir_component(component, ".git") {
            return Some("'.git' is reserved");
        }
        if oak_core::is_metadata_dir_component(component, ".oakroot") {
            return Some("'.oakroot' is reserved");
        }
        if oak_core::is_metadata_dir_component(component, ".oak") {
            let tracked = *component == ".oak"
                && index == 0
                && (matches!(path, ".oak/attributes" | ".oak/PERMISSIONS")
                    || (components.len() > 2 && components[1] == "workflows"));
            if !tracked {
                return Some("only .oak/attributes, .oak/PERMISSIONS and .oak/workflows/ may be tracked in .oak");
            }
        }
    }
    None
}

/// Where a git tree sits, as far as which of its entries Oak keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TreePlace {
    Root,
    RootOakDir,
    Elsewhere,
}

/// State shared across every tree of one import.
struct TreeConversion<'a> {
    git: &'a gix::Repository,
    /// git blob oid -> oak blob hash. The same git blob is the same content
    /// in every commit, so it's stored once.
    blob_map: HashMap<ObjectId, Hash>,
    /// (git tree oid, [`TreePlace`]) -> converted oak tree hash, or `None`
    /// when nothing in it is kept. Git trees are content-addressed and
    /// immutable, so an unchanged subtree (most of every commit) is one cache
    /// hit instead of a re-walk: the import is O(unique trees), not
    /// O(commits × files). Which entries are kept depends on where a tree
    /// sits only at the root and the root `.oak`, hence the place in the key.
    tree_map: HashMap<(ObjectId, TreePlace), Option<Hash>>,
    report: &'a mut ImportReport,
}

impl TreeConversion<'_> {
    /// Convert the git tree at `prefix` ("" for the root, else "a/b/") into
    /// oak tree objects and return the oak hash, or `None` when nothing in it
    /// is kept, so the parent omits it the way `build_tree` omits a directory
    /// with no files. Each tree is built with `Tree::new` exactly as
    /// `build_tree` builds it from the equivalent flat manifest, so commit
    /// hashes match the flat path bit for bit.
    fn convert_tree(
        &mut self,
        tree_id: ObjectId,
        prefix: &str,
        importer: &mut BulkImporter,
    ) -> Result<Option<Hash>> {
        let place = match prefix {
            "" => TreePlace::Root,
            ".oak/" => TreePlace::RootOakDir,
            _ => TreePlace::Elsewhere,
        };
        let key = (tree_id, place);
        if let Some(cached) = self.tree_map.get(&key) {
            return Ok(cached.clone());
        }

        let tree = self
            .git
            .find_object(tree_id)
            .map_err(|e| OakError::Git(e.to_string()))?
            .try_into_tree()
            .map_err(|e| OakError::Git(e.to_string()))?;

        let mut tree_entries: Vec<TreeEntry> = Vec::new();
        for entry_res in tree.iter() {
            let entry = entry_res.map_err(|e| OakError::Git(e.to_string()))?;
            let mode = entry.mode();
            let raw_name: &[u8] = entry.filename();
            let Ok(name) = std::str::from_utf8(raw_name) else {
                self.report.skipped_paths.insert(
                    format!("{prefix}{}", String::from_utf8_lossy(raw_name)),
                    "name is not UTF-8",
                );
                continue;
            };
            let path = format!("{prefix}{name}");

            if mode.is_commit() {
                // A gitlink: Oak has no submodules, and following one would
                // mean a network fetch nobody asked for.
                self.report.skipped_submodules.insert(path);
                continue;
            }
            // A directory is judged by a path inside it, so `.oak/workflows`
            // (a directory Oak tracks files under) isn't refused as a file.
            // The root `.oak` itself holds tracked files; its entries are
            // judged one by one.
            let probe = if mode.is_tree() {
                format!("{path}/x")
            } else {
                path.clone()
            };
            let root_oak_dir = mode.is_tree() && path == ".oak";
            if let Some(why) = untrackable_reason(&probe).filter(|_| !root_oak_dir) {
                self.report.skipped_paths.insert(path, why);
                continue;
            }

            if mode.is_tree() {
                if let Some(sub_hash) =
                    self.convert_tree(entry.id().detach(), &format!("{path}/"), importer)?
                {
                    tree_entries.push(TreeEntry {
                        name: name.to_string(),
                        kind: TreeEntryKind::Tree,
                        hash: sub_hash,
                        mode: FileMode::Regular,
                    });
                }
            } else if let Some(oak_mode) = git_file_mode_to_oak(mode) {
                let blob_oid = entry.id().detach();
                let blob_hash = if let Some(cached) = self.blob_map.get(&blob_oid) {
                    cached.clone()
                } else {
                    let blob = self
                        .git
                        .find_object(blob_oid)
                        .map_err(|e| OakError::Git(e.to_string()))?;
                    let h = importer.put_blob(blob.data.to_vec())?;
                    self.blob_map.insert(blob_oid, h.clone());
                    h
                };
                tree_entries.push(TreeEntry {
                    name: name.to_string(),
                    kind: TreeEntryKind::Blob,
                    hash: blob_hash,
                    mode: oak_mode,
                });
            }
        }

        let result = if tree_entries.is_empty() {
            None
        } else {
            let oak_tree = Tree::new(tree_entries)?;
            let hash = oak_tree.hash.clone();
            importer.put_tree_node(&oak_tree)?;
            Some(hash)
        };
        self.tree_map.insert(key, result.clone());
        Ok(result)
    }
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

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=Oak Test",
                "-c",
                "user.email=test@oak.invalid",
            ])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn clone_imports_a_git_repo_that_tracks_oak_workflows() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(source.join(".oak/workflows")).unwrap();
        fs::write(source.join(".oak/workflows/ci.yml"), "on: [push]\n").unwrap();
        fs::write(source.join("README.md"), "hello\n").unwrap();
        git(tmp.path(), &["init", "-q", source.to_str().unwrap()]);
        git(&source, &["add", "-A"]);
        git(&source, &["commit", "-q", "-m", "tracks .oak/workflows"]);

        let dest = tmp.path().join("dest");
        run(source.to_str().unwrap(), Some(dest.clone()), tmp.path())
            .expect("a tracked .oak/workflows is not an existing repository");

        assert_eq!(
            fs::read_to_string(dest.join(".oak/workflows/ci.yml")).unwrap(),
            "on: [push]\n"
        );
        let repo = SqliteRepository::open(&dest.join(".oak/oak.db")).unwrap();
        let tip = repo
            .get_branch_head("main")
            .unwrap()
            .expect("imported main");
        let commit = repo.get_commit(&tip).unwrap().expect("tip commit");
        let manifest = repo
            .get_manifest(&commit.manifest_hash)
            .unwrap()
            .expect("tip manifest");
        let mut paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, [".oak/workflows/ci.yml", "README.md"]);
    }

    #[test]
    fn clone_still_refuses_a_tracked_oak_database() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(source.join(".oak")).unwrap();
        fs::write(source.join(".oak/oak.db"), "not a real database").unwrap();
        git(tmp.path(), &["init", "-q", source.to_str().unwrap()]);
        git(&source, &["add", "-A"]);
        git(&source, &["commit", "-q", "-m", "tracks a database"]);

        let error = run(
            source.to_str().unwrap(),
            Some(tmp.path().join("dest")),
            tmp.path(),
        )
        .expect_err("a tracked .oak/oak.db must not be adopted as a repository");
        assert!(matches!(error, OakError::RepoAlreadyExists), "{error}");
    }

    #[test]
    fn clone_refuses_a_git_repo_that_tracks_oak_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(source.join(".oak/hooks")).unwrap();
        fs::write(source.join(".oak/hooks/pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();
        git(tmp.path(), &["init", "-q", source.to_str().unwrap()]);
        git(&source, &["add", "-A"]);
        git(&source, &["commit", "-q", "-m", "plants a hook"]);

        let error = run(
            source.to_str().unwrap(),
            Some(tmp.path().join("dest")),
            tmp.path(),
        )
        .expect_err("a tracked .oak/hooks must be refused");
        assert!(error.to_string().contains(".oak/hooks"), "{error}");
        assert!(!tmp.path().join("dest/.oak/oak.db").exists());
    }

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
