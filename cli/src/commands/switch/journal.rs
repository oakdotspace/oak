//! Durable intent for a switch. The journal survives every working-tree and
//! reference write; recovery replays only the recorded paths, never a new sweep.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use oak_core::{Branch, FileMode, Hash, Manifest, MetadataKey, OakError, Repository, Result};
use serde::{Deserialize, Serialize};

use crate::materialize::{apply_manifest, ApplyOpts, DeleteScope};
use crate::workdir_lock::WorkdirLock;

const FILE: &str = "switch-journal.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum State {
    Missing,
    Directory,
    File { hash: Hash, mode: FileMode },
}

#[derive(Serialize, Deserialize)]
struct PathState {
    path: String,
    before: State,
    after: State,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Journal {
    version: u32,
    source_branch: Option<String>,
    source_head: Option<Hash>,
    target_name: String,
    target_head: Option<Hash>,
    create_branch: Option<Branch>,
    target: Manifest,
    dropped: Manifest,
    paths: Vec<PathState>,
    sparse_paths: Option<String>,
}

pub(crate) fn ensure_no_pending(oak_dir: &Path) -> Result<()> {
    let bytes = match fs::read(oak_dir.join(FILE)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let instruction = match serde_json::from_slice::<Journal>(&bytes) {
        Ok(journal) => {
            let target = if journal.target_name.is_empty() {
                journal.target_head.as_ref().map(Hash::as_str).unwrap_or("TARGET")
            } else { &journal.target_name };
            format!("Run `oak switch {target}` again to recover before modifying or recording this working tree.")
        }
        Err(_) => "The recovery journal could not be read; preserve it and the working tree for inspection.".into(),
    };
    Err(OakError::DirtyWorkingTree(format!("An interrupted switch is pending. {instruction} The recovery journal is .oak/switch-journal.json; do not delete it.")))
}

fn state(root: &Path, path: &str, repo: &dyn Repository, store: bool) -> Result<State> {
    // These internal directories can be ancestors of tracked workflow and
    // attribute files. They are observed only; never materialized as entries.
    if path != ".oak" && path != ".oak/workflows" {
        oak_core::validate_worktree_path(path)?;
    }
    // Never read through an ancestor symlink (including a target directory
    // that the interrupted switch has not replaced yet).
    let mut parent = root.to_path_buf();
    let components: Vec<_> = path.split('/').collect();
    for component in &components[..components.len() - 1] {
        parent.push(component);
        match fs::symlink_metadata(&parent) {
            Ok(md) if md.is_dir() => {}
            Ok(_) => return Ok(State::Missing),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::Missing),
            Err(e) => return Err(e.into()),
        }
    }
    let full = root.join(path);
    let md = match fs::symlink_metadata(&full) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::Missing),
        Err(e) => return Err(e.into()),
    };
    if md.is_dir() {
        return Ok(State::Directory);
    }
    let (bytes, mode) = if md.file_type().is_symlink() {
        let target = fs::read_link(&full)?;
        (
            target.as_os_str().as_encoded_bytes().to_vec(),
            FileMode::Symlink,
        )
    } else if md.is_file() {
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            if md.permissions().mode() & 0o111 != 0 {
                FileMode::Executable
            } else {
                FileMode::Regular
            }
        };
        #[cfg(not(unix))]
        let mode = FileMode::Regular;
        // O_NOFOLLOW also protects the final component from a symlink race.
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = options.open(&full)?;
        if !file.metadata()?.is_file() {
            return Err(OakError::DirtyWorkingTree(format!(
                "'{path}' changed while preparing switch recovery"
            )));
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)?;
        (bytes, mode)
    } else {
        return Err(OakError::DirtyWorkingTree(format!(
            "cannot journal special file '{path}'"
        )));
    };
    let hash = if store {
        repo.put_blob(bytes)?
    } else {
        repo.hash_blob_content(&bytes)
    };
    Ok(State::File { hash, mode })
}

// Flush inode data without issuing a whole-device cache flush per path on
// macOS. The journal's final sync_all below provides that durability barrier
// once, after every affected file and directory has reached the filesystem.
fn sync_inode(file: &fs::File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(unix))]
    file.sync_all()?;
    Ok(())
}

impl Journal {
    pub(super) fn begin(
        ctx: &crate::resolve::RepoContext,
        repo: &dyn Repository,
        plan: &super::WorkingDirMove,
        target_name: &str,
        target_head: Option<Hash>,
        create_branch: Option<Branch>,
    ) -> Result<Self> {
        ensure_no_pending(&ctx.oak_dir)?;
        plan.require_blobs(repo)?;
        // This journal covers the move's writes, so the planner's refusal
        // has to come before it exists.
        plan.preflight(&ctx.work_tree, repo)?;
        let sparse_paths = repo.get_metadata(MetadataKey::SparsePaths)?;
        let cone = oak_core::SparseCone::from_metadata(sparse_paths.as_deref());
        let in_cone = |path: &str| cone.as_ref().is_none_or(|c| c.covers(path));
        let mut target = plan.files().clone();
        target.entries.retain(|e| in_cone(&e.path));
        crate::materialize::validate_worktree_paths(&target, &|_| true)?;
        let mut dropped = match &plan.plan {
            super::MovePlan::Diff(diff) => diff.dropped().clone(),
            super::MovePlan::Full(_) => {
                let ignore = oak_core::IgnorePatterns::new(&ctx.work_tree)?;
                let entries = super::super::commit::scan_working_dir_for_capture(
                    &ctx.work_tree,
                    repo,
                    &ignore,
                    &in_cone,
                )?;
                Manifest {
                    hash: oak_core::Tree::empty_hash(),
                    entries,
                }
            }
        };
        let target_paths: BTreeSet<_> = target.entries.iter().map(|e| e.path.as_str()).collect();
        dropped
            .entries
            .retain(|e| in_cone(&e.path) && !target_paths.contains(e.path.as_str()));
        let mut target_directories = BTreeSet::new();
        for entry in &target.entries {
            let mut parent = Path::new(&entry.path).parent();
            while let Some(path) = parent.filter(|p| !p.as_os_str().is_empty()) {
                target_directories.insert(path.to_string_lossy().into_owned());
                parent = path.parent();
            }
        }
        let known_lost = super::super::known_loss::known_lost_paths_in_manifest(repo, &target);
        let mut after = BTreeMap::new();
        for entry in &dropped.entries {
            after.insert(entry.path.clone(), State::Missing);
        }
        for entry in &target.entries {
            let expected = if repo.has_blob(&entry.blob_hash)? {
                State::File {
                    hash: entry.blob_hash.clone(),
                    mode: if cfg!(unix) {
                        entry.mode
                    } else {
                        FileMode::Regular
                    },
                }
            } else if known_lost.contains(&entry.path) {
                State::Missing
            } else {
                state(&ctx.work_tree, &entry.path, repo, true)?
            };
            after.insert(entry.path.clone(), expected);
        }
        // Ancestors can themselves be replaced (file <-> directory). Record
        // them too, so a new post-crash symlink cannot redirect recovery.
        let leaves: Vec<_> = after.keys().cloned().collect();
        for path in leaves {
            let mut parent = Path::new(&path).parent();
            while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
                let name = p.to_string_lossy().into_owned();
                let expected = if target_directories.contains(&name) {
                    State::Directory
                } else {
                    state(&ctx.work_tree, &name, repo, true)?
                };
                if expected == State::Directory && target_directories.contains(&name) {
                    after.insert(name, expected);
                } else {
                    after.entry(name).or_insert(expected);
                }
                parent = p.parent();
            }
        }
        let paths = after
            .into_iter()
            .map(|(path, after)| {
                Ok(PathState {
                    before: state(&ctx.work_tree, &path, repo, true)?,
                    path,
                    after,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let journal = Self {
            version: 1,
            source_branch: repo.get_current_branch_name()?,
            source_head: repo.get_head()?,
            target_name: target_name.to_owned(),
            target_head,
            create_branch,
            target,
            dropped,
            paths,
            sparse_paths,
        };
        crate::atomic_file::write_atomic_private(
            &ctx.oak_dir.join(FILE),
            serde_json::to_vec(&journal)?,
        )?;
        Ok(journal)
    }

    pub(super) fn finish(
        &self,
        ctx: &crate::resolve::RepoContext,
        repo: &dyn Repository,
    ) -> Result<()> {
        // The intent cannot be retired durably before its filesystem writes.
        // Sync files and every affected directory before committing references.
        let mut directories = BTreeSet::new();
        directories.insert(ctx.work_tree.clone());
        for path in &self.paths {
            let full = ctx.work_tree.join(&path.path);
            match state(&ctx.work_tree, &path.path, repo, false)? {
                State::File {
                    mode: FileMode::Symlink,
                    ..
                }
                | State::Missing => {}
                State::Directory => {
                    directories.insert(full.clone());
                }
                State::File { .. } => {
                    let mut options = fs::OpenOptions::new();
                    options.read(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
                    }
                    sync_inode(&options.open(&full)?)?;
                }
            }
            if let Some(parent) = Path::new(&path.path)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
            {
                if state(&ctx.work_tree, &parent.to_string_lossy(), repo, false)?
                    == State::Directory
                {
                    directories.insert(ctx.work_tree.join(parent));
                }
            }
        }
        #[cfg(unix)]
        for directory in directories {
            if fs::symlink_metadata(&directory).is_ok_and(|md| md.is_dir()) {
                sync_inode(&fs::File::open(directory)?)?;
            }
        }
        fs::File::open(ctx.oak_dir.join(FILE))?.sync_all()?;
        if let Some(branch) = &self.create_branch {
            repo.store_branch(branch)?;
            if let Some(head) = &self.target_head {
                repo.set_branch_head(&branch.name, head)?;
            }
        }
        if let Some(head) = &self.target_head {
            repo.set_head(head)?;
        }
        repo.set_current_branch(&self.target_name)?;
        fs::remove_file(ctx.oak_dir.join(FILE))?;
        #[cfg(unix)]
        fs::File::open(&ctx.oak_dir)?.sync_all()?;
        Ok(())
    }

    fn verify(&self, ctx: &crate::resolve::RepoContext, repo: &dyn Repository) -> Result<()> {
        let branch = repo.get_current_branch_name()?;
        let head = repo.get_head()?;
        if self.version != 1
            || (branch != self.source_branch
                && branch.as_deref()
                    != if self.target_name.is_empty() {
                        None
                    } else {
                        Some(self.target_name.as_str())
                    })
            || (head != self.source_head && head != self.target_head)
            || repo.get_metadata(MetadataKey::SparsePaths)? != self.sparse_paths
        {
            return Err(OakError::DirtyWorkingTree("switch recovery metadata changed; preserve the journal and working tree for inspection".into()));
        }
        let mut seen = BTreeSet::new();
        for path in &self.paths {
            if !seen.insert(&path.path) {
                return Err(OakError::DirtyWorkingTree(
                    "duplicate switch recovery path".into(),
                ));
            }
            let current = state(&ctx.work_tree, &path.path, repo, false)?;
            // A case-only rename can already expose the new bytes under
            // both spellings. Require filesystem identity, not just folding.
            let is_renamed_alias = current != path.before
                && current != path.after
                && current != State::Missing
                && self.paths.iter().any(|other| {
                    if other.path == path.path
                        || !other.path.eq_ignore_ascii_case(&path.path)
                        || other.after != current
                    {
                        return false;
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        match (
                            fs::symlink_metadata(ctx.work_tree.join(&path.path)),
                            fs::symlink_metadata(ctx.work_tree.join(&other.path)),
                        ) {
                            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
                            _ => false,
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        false
                    }
                });
            if current != path.before
                && current != path.after
                && current != State::Missing
                && !is_renamed_alias
            {
                return Err(OakError::DirtyWorkingTree(format!("switch recovery stopped: '{}' was edited after the switch began. Move that edited path outside the checkout, then rerun `oak switch {}`. No recovery files were changed.", path.path, self.target_name)));
            }
        }
        Ok(())
    }
}

pub(super) fn resume(
    ctx: &crate::resolve::RepoContext,
    lock: &WorkdirLock,
    repo: &dyn Repository,
    name: Option<&str>,
) -> Result<bool> {
    let bytes = match fs::read(ctx.oak_dir.join(FILE)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let journal: Journal = serde_json::from_slice(&bytes)?;
    let requested = if journal.target_name.is_empty() {
        journal.target_head.as_ref().map(Hash::as_str)
    } else {
        Some(journal.target_name.as_str())
    };
    if name != requested {
        return Err(OakError::DirtyWorkingTree(format!("An interrupted switch is pending. Resume it with `oak switch {}` before switching elsewhere.", requested.unwrap_or("TARGET"))));
    }
    journal.verify(ctx, repo)?;
    super::apply_skipping_withheld(repo, &journal.target, |missing_blobs, known_lost| {
        apply_manifest(
            lock,
            &ctx.work_tree,
            repo,
            &journal.target,
            ApplyOpts {
                delete: DeleteScope::TrackedRemoved {
                    old: &journal.dropped,
                },
                missing_blobs,
                remove_missing_paths: Some(known_lost),
                ..ApplyOpts::default()
            },
        )
    })?;
    journal.finish(ctx, repo)?;
    crate::output::success(&format!(
        "Recovered interrupted switch to '{}'",
        requested.unwrap_or("detached HEAD")
    ));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{commit, init, switch};
    use tempfile::TempDir;

    fn fixture() -> (TempDir, String, Hash) {
        let dir = TempDir::new().unwrap();
        init::run(dir.path(), false).unwrap();
        fs::write(dir.path().join("a"), "original").unwrap();
        commit::run(dir.path()).unwrap();
        let ctx = crate::resolve::resolve(dir.path()).unwrap();
        let repo = ctx.open().unwrap();
        let original = repo.get_current_branch_name().unwrap().unwrap();
        switch::create(dir.path(), "target", switch::WorktreePolicy::Carry).unwrap();
        fs::write(dir.path().join("a"), "target").unwrap();
        fs::write(dir.path().join("b"), "added").unwrap();
        commit::run(dir.path()).unwrap();
        let target = repo.get_head().unwrap().unwrap();
        switch::run(dir.path(), Some(&original), false).unwrap();
        (dir, original, target)
    }

    fn interrupt(dir: &Path, head: &Hash, clean: bool) {
        let ctx = crate::resolve::resolve(dir).unwrap();
        let repo = ctx.open().unwrap();
        let _lock = WorkdirLock::acquire(&ctx.oak_dir).unwrap();
        let holds = if clean {
            switch::WorktreeHolds::Unknown
        } else {
            switch::WorktreeHolds::after_clean_guard(repo.as_ref()).unwrap()
        };
        let root = repo.get_commit(head).unwrap().unwrap().manifest_hash;
        let plan = switch::WorkingDirMove::plan(repo.as_ref(), &holds, &root).unwrap();
        Journal::begin(
            &ctx,
            repo.as_ref(),
            &plan,
            "target",
            Some(head.clone()),
            None,
        )
        .unwrap();
        // Crash after the first file replacement, before cache/ref writes.
        fs::write(dir.join("a"), "target").unwrap();
    }

    #[test]
    fn interrupted_switch_blocks_commit_and_status_then_resumes_without_sweeping_new_files() {
        let (dir, _, head) = fixture();
        interrupt(dir.path(), &head, false);
        assert!(commit::run(dir.path())
            .unwrap_err()
            .to_string()
            .contains("interrupted switch"));
        assert!(crate::commands::status::run_json(dir.path(), false, false).is_err());
        assert!(switch::run(dir.path(), Some("elsewhere"), false).is_err());
        fs::write(dir.path().join("new-untracked"), "keep me").unwrap();
        switch::run(dir.path(), Some("target"), false).unwrap();
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"target");
        assert_eq!(fs::read(dir.path().join("b")).unwrap(), b"added");
        assert_eq!(
            fs::read(dir.path().join("new-untracked")).unwrap(),
            b"keep me"
        );
        assert!(!dir.path().join(".oak").join(FILE).exists());
        let repo = crate::resolve::resolve(dir.path()).unwrap().open().unwrap();
        assert_eq!(
            repo.get_current_branch_name().unwrap().as_deref(),
            Some("target")
        );
        assert_eq!(repo.get_head().unwrap(), Some(head));
    }

    #[test]
    fn clean_switch_captures_dirty_bytes_and_refuses_intervening_edits_before_any_replay() {
        let (dir, original, head) = fixture();
        fs::write(dir.path().join("a"), "dirty original").unwrap();
        fs::write(dir.path().join("old-untracked"), "discard authorized").unwrap();
        interrupt(dir.path(), &head, true);
        let ctx = crate::resolve::resolve(dir.path()).unwrap();
        let repo = ctx.open_for_switch().unwrap();
        let j: Journal =
            serde_json::from_slice(&fs::read(ctx.oak_dir.join(FILE)).unwrap()).unwrap();
        let State::File { hash, .. } = &j.paths.iter().find(|p| p.path == "a").unwrap().before
        else {
            panic!()
        };
        assert_eq!(
            repo.get_blob(hash).unwrap().unwrap().content,
            b"dirty original"
        );
        fs::write(dir.path().join("a"), "edited after crash").unwrap();
        fs::write(dir.path().join("new-untracked"), "keep me").unwrap();
        let err = switch::run(dir.path(), Some("target"), false).unwrap_err();
        assert!(err.to_string().contains("edited after"));
        assert!(!dir.path().join("b").exists());
        assert_eq!(repo.get_current_branch_name().unwrap(), Some(original));
        assert_eq!(
            fs::read(dir.path().join("a")).unwrap(),
            b"edited after crash"
        );
        assert!(dir.path().join("old-untracked").exists());
        // Saving the edit away from the affected path permits an explicit retry.
        fs::rename(dir.path().join("a"), dir.path().join("saved-edit")).unwrap();
        switch::run(dir.path(), Some("target"), false).unwrap();
        assert_eq!(
            fs::read(dir.path().join("saved-edit")).unwrap(),
            b"edited after crash"
        );
        assert!(dir.path().join("new-untracked").exists());
        assert!(!dir.path().join("old-untracked").exists());
    }

    #[test]
    fn recovery_survives_partial_reference_update_and_repeated_replay() {
        let (dir, _, head) = fixture();
        interrupt(dir.path(), &head, false);
        let ctx = crate::resolve::resolve(dir.path()).unwrap();
        let repo = ctx.open_for_switch().unwrap();
        repo.set_head(&head).unwrap();
        let intent = fs::read(ctx.oak_dir.join(FILE)).unwrap();
        switch::run(dir.path(), Some("target"), false).unwrap();
        // Recreate the exact durable intent to model a crash after both refs
        // were committed but before retiring the journal.
        crate::atomic_file::write_atomic_private(&ctx.oak_dir.join(FILE), intent).unwrap();
        switch::run(dir.path(), Some("target"), false).unwrap();
        assert_eq!(repo.get_head().unwrap(), Some(head));
        assert_eq!(fs::read(dir.path().join("a")).unwrap(), b"target");
    }

    #[test]
    fn resume_accepts_a_materialized_case_only_rename() {
        let (dir, original, _) = fixture();
        switch::run(dir.path(), Some("target"), false).unwrap();
        fs::rename(dir.path().join("a"), dir.path().join("A")).unwrap();
        commit::run(dir.path()).unwrap();
        let ctx = crate::resolve::resolve(dir.path()).unwrap();
        let repo = ctx.open().unwrap();
        let head = repo.get_head().unwrap().unwrap();
        switch::run(dir.path(), Some(&original), false).unwrap();
        let lock = WorkdirLock::acquire(&ctx.oak_dir).unwrap();
        let root = repo.get_commit(&head).unwrap().unwrap().manifest_hash;
        let holds = switch::WorktreeHolds::after_clean_guard(repo.as_ref()).unwrap();
        let plan = switch::WorkingDirMove::plan(repo.as_ref(), &holds, &root).unwrap();
        Journal::begin(&ctx, repo.as_ref(), &plan, "target", Some(head), None).unwrap();
        plan.apply(&lock, dir.path(), repo.as_ref())
            .unwrap()
            .finish();
        drop(lock);
        switch::run(dir.path(), Some("target"), false).unwrap();
        assert_eq!(fs::read(dir.path().join("A")).unwrap(), b"target");
        let paths = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(paths.iter().any(|p| p == "A"));
        assert!(!paths.iter().any(|p| p == "a"));
    }

    #[test]
    fn recovery_completes_new_branch_and_detached_reference_intents() {
        for detached in [false, true] {
            let (dir, _, head) = fixture();
            interrupt(dir.path(), &head, false);
            let ctx = crate::resolve::resolve(dir.path()).unwrap();
            let mut j: Journal =
                serde_json::from_slice(&fs::read(ctx.oak_dir.join(FILE)).unwrap()).unwrap();
            j.target_name = if detached {
                String::new()
            } else {
                "created".into()
            };
            j.create_branch = if detached {
                None
            } else {
                Some(Branch::new("created".into(), None, Some("main".into())))
            };
            let bytes = serde_json::to_vec(&j).unwrap();
            crate::atomic_file::write_atomic_private(&ctx.oak_dir.join(FILE), &bytes).unwrap();
            let target = if detached { head.as_str() } else { "created" };
            switch::run(dir.path(), Some(target), detached).unwrap();
            let repo = ctx.open().unwrap();
            assert_eq!(repo.get_head().unwrap(), Some(head.clone()));
            assert_eq!(
                repo.get_current_branch_name().unwrap().as_deref(),
                if detached { None } else { Some("created") }
            );
            // Repeat recovery after refs already reached the target.
            crate::atomic_file::write_atomic_private(&ctx.oak_dir.join(FILE), &bytes).unwrap();
            switch::run(dir.path(), Some(target), detached).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn recovery_never_follows_symlink_ancestors_or_overwrites_new_symlinks() {
        use std::os::unix::fs::symlink;
        let (dir, original, _) = fixture();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("child"), "outside sentinel").unwrap();
        symlink(outside.path(), dir.path().join("link")).unwrap();
        commit::run(dir.path()).unwrap();
        switch::create(dir.path(), "tree-target", switch::WorktreePolicy::Carry).unwrap();
        fs::remove_file(dir.path().join("link")).unwrap();
        fs::create_dir(dir.path().join("link")).unwrap();
        fs::write(dir.path().join("link/child"), "inside target").unwrap();
        commit::run(dir.path()).unwrap();
        let ctx = crate::resolve::resolve(dir.path()).unwrap();
        let repo = ctx.open().unwrap();
        let head = repo.get_head().unwrap().unwrap();
        switch::run(dir.path(), Some(&original), false).unwrap();
        let lock = WorkdirLock::acquire(&ctx.oak_dir).unwrap();
        let root = repo.get_commit(&head).unwrap().unwrap().manifest_hash;
        let holds = switch::WorktreeHolds::after_clean_guard(repo.as_ref()).unwrap();
        let plan = switch::WorkingDirMove::plan(repo.as_ref(), &holds, &root).unwrap();
        Journal::begin(&ctx, repo.as_ref(), &plan, "tree-target", Some(head), None).unwrap();
        drop(lock);
        // A different symlink after the crash must refuse without touching its target.
        fs::remove_file(dir.path().join("link")).unwrap();
        symlink(outside.path().join("different"), dir.path().join("link")).unwrap();
        assert!(switch::run(dir.path(), Some("tree-target"), false).is_err());
        fs::remove_file(dir.path().join("link")).unwrap();
        // Model a kill after replacing the ancestor but before writing its child.
        fs::create_dir(dir.path().join("link")).unwrap();
        switch::run(dir.path(), Some("tree-target"), false).unwrap();
        assert_eq!(
            fs::read(dir.path().join("link/child")).unwrap(),
            b"inside target"
        );
        assert_eq!(
            fs::read(outside.path().join("child")).unwrap(),
            b"outside sentinel"
        );
    }

    #[test]
    fn journal_handles_tracked_workflows_without_treating_sidecar_directories_as_files() {
        let (dir, original, _) = fixture();
        fs::create_dir_all(dir.path().join(".oak/workflows")).unwrap();
        fs::write(
            dir.path().join(".oak/workflows/ci.yml"),
            "original workflow",
        )
        .unwrap();
        commit::run(dir.path()).unwrap();
        switch::create(dir.path(), "workflow-target", switch::WorktreePolicy::Carry).unwrap();
        fs::write(dir.path().join(".oak/workflows/ci.yml"), "target workflow").unwrap();
        commit::run(dir.path()).unwrap();
        let ctx = crate::resolve::resolve(dir.path()).unwrap();
        let repo = ctx.open().unwrap();
        let head = repo.get_head().unwrap().unwrap();
        switch::run(dir.path(), Some(&original), false).unwrap();
        let lock = WorkdirLock::acquire(&ctx.oak_dir).unwrap();
        let root = repo.get_commit(&head).unwrap().unwrap().manifest_hash;
        let holds = switch::WorktreeHolds::after_clean_guard(repo.as_ref()).unwrap();
        let plan = switch::WorkingDirMove::plan(repo.as_ref(), &holds, &root).unwrap();
        Journal::begin(
            &ctx,
            repo.as_ref(),
            &plan,
            "workflow-target",
            Some(head),
            None,
        )
        .unwrap();
        drop(lock);
        switch::run(dir.path(), Some("workflow-target"), false).unwrap();
        assert_eq!(
            fs::read(dir.path().join(".oak/workflows/ci.yml")).unwrap(),
            b"target workflow"
        );
    }

    #[test]
    fn recovery_preserves_untracked_work_added_inside_a_replaced_directory() {
        let (dir, original, _) = fixture();
        fs::create_dir(dir.path().join("node")).unwrap();
        fs::write(dir.path().join("node/child"), "tracked original").unwrap();
        commit::run(dir.path()).unwrap();
        switch::create(dir.path(), "file-target", switch::WorktreePolicy::Carry).unwrap();
        fs::remove_file(dir.path().join("node/child")).unwrap();
        fs::remove_dir(dir.path().join("node")).unwrap();
        fs::write(dir.path().join("node"), "target file").unwrap();
        commit::run(dir.path()).unwrap();
        let ctx = crate::resolve::resolve(dir.path()).unwrap();
        let repo = ctx.open().unwrap();
        let head = repo.get_head().unwrap().unwrap();
        switch::run(dir.path(), Some(&original), false).unwrap();
        let lock = WorkdirLock::acquire(&ctx.oak_dir).unwrap();
        let root = repo.get_commit(&head).unwrap().unwrap().manifest_hash;
        let holds = switch::WorktreeHolds::after_clean_guard(repo.as_ref()).unwrap();
        let plan = switch::WorkingDirMove::plan(repo.as_ref(), &holds, &root).unwrap();
        Journal::begin(&ctx, repo.as_ref(), &plan, "file-target", Some(head), None).unwrap();
        drop(lock);
        fs::write(dir.path().join("node/untracked"), "new work").unwrap();
        assert!(switch::run(dir.path(), Some("file-target"), false).is_err());
        assert_eq!(
            fs::read(dir.path().join("node/child")).unwrap(),
            b"tracked original"
        );
        assert_eq!(
            fs::read(dir.path().join("node/untracked")).unwrap(),
            b"new work"
        );
        assert_eq!(repo.get_current_branch_name().unwrap(), Some(original));
        fs::rename(
            dir.path().join("node/untracked"),
            dir.path().join("saved-untracked"),
        )
        .unwrap();
        switch::run(dir.path(), Some("file-target"), false).unwrap();
        assert_eq!(fs::read(dir.path().join("node")).unwrap(), b"target file");
        assert_eq!(
            fs::read(dir.path().join("saved-untracked")).unwrap(),
            b"new work"
        );
    }
}
