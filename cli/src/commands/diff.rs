use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use oak_core::Repository;
use oak_core::{ChangeType, FileChange, FileDiff, FileMode, Hash, ManifestEntry, Result};

use crate::output;

/// Show diff between working directory and HEAD.
///
/// The set of paths shown here is computed by [`super::commit::compute_changes`]
/// — the exact same function `oak status` uses and the same scan/diff logic
/// `oak commit` records with. So `oak status`, `oak diff`, and `oak commit`
/// always agree on *which* files changed; `oak diff` only adds the rendering of
/// the contents. In particular, diff must never silently omit a path in that
/// set just because it can't render a textual hunk for it (binary files, a
/// trailing-newline-only change, a pure mode change): it emits a "differ"
/// notice instead. (It used to gate every file on `FileDiff::has_changes`,
/// which dropped binary edits that status/commit still counted.)
pub fn run(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let work_tree = ctx.work_tree.clone();
    let repo = ctx.open()?;

    let (changes, formatted) = render(repo.as_ref(), &work_tree)?;
    if changes.is_empty() {
        output::info("No differences");
        return Ok(());
    }

    print_formatted(&formatted);
    Ok(())
}

/// Emit an already-formatted (colorized) diff string: through `$PAGER` when
/// stdout is a terminal, otherwise straight to stdout line by line.
///
/// Shared by the regular `oak diff` and the mount-aware `oak diff` so both
/// paginate and print identically.
pub(crate) fn print_formatted(formatted: &str) {
    // If stdout is a terminal, pipe through a pager
    if io::stdout().is_terminal() {
        let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".to_string());
        let mut args: Vec<&str> = pager.split_whitespace().collect();
        let cmd = args.remove(0);

        // Add -R for less to support ANSI colors, -F to quit if output fits on screen
        if cmd == "less" && args.is_empty() {
            args.push("-RF");
        }

        if let Ok(mut child) = Command::new(cmd).args(&args).stdin(Stdio::piped()).spawn() {
            if let Some(ref mut stdin) = child.stdin {
                // Ignore broken pipe (user quit pager early)
                let _ = stdin.write_all(formatted.as_bytes());
            }
            let _ = child.wait();
            return;
        }
        // Fall through to direct output if pager fails to spawn
    }

    // Non-TTY or pager failed: print directly
    for line in formatted.lines() {
        output::print_line(line);
    }
}

/// Compute the change set and render it to a formatted (colorized) diff string.
///
/// Shared by [`run`] and exercised directly by tests. The returned `changes`
/// are exactly what [`super::commit::compute_changes`] (and therefore
/// `oak status`) report, and `formatted` is guaranteed to contain a
/// `diff --oak a/<path> b/<path>` block for *every* one of them — diff never
/// drops a path in the set, so `oak status`, `oak diff`, and `oak commit` agree.
pub fn render(repo: &dyn Repository, work_tree: &Path) -> Result<(Vec<FileChange>, String)> {
    // Authoritative change set — identical to `oak status` and to what
    // `oak commit` will record (honors the active project/team scope, the
    // parent-branch head walk, and counts by blob hash rather than by whether a
    // textual diff happens to render).
    let (changes, head, _branch) = super::commit::compute_changes(repo, work_tree)?;

    // Full head manifest, for looking up the pre-image blob of each change.
    // (The change set already restricts which paths we render.)
    let head_manifest = if let Some(ref head_hash) = head {
        repo.get_commit(head_hash)?
            .and_then(|c| repo.get_manifest(&c.manifest_hash).ok().flatten())
    } else {
        None
    };
    let head_files: HashMap<&str, &ManifestEntry> = head_manifest
        .as_ref()
        .map(|m| m.entries.iter().map(|e| (e.path.as_str(), e)).collect())
        .unwrap_or_default();

    let mut formatted = String::new();
    for change in &changes {
        formatted.push_str(&render_change(repo, work_tree, &head_files, change)?);
        formatted.push('\n');
    }
    Ok((changes, formatted))
}

/// Render one change into a formatted (colorized) diff block. Always produces
/// output — a file in the change set is never dropped, even when there's no
/// textual hunk to show.
pub(crate) fn render_change(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    change: &FileChange,
) -> Result<String> {
    let read_blob = |hash: &Hash| -> Result<Vec<u8>> {
        Ok(repo.get_blob(hash)?.map(|b| b.content).unwrap_or_default())
    };
    let read_worktree = |rel: &str| -> Result<Vec<u8>> {
        let p = work_tree.join(rel);
        if p.exists() {
            Ok(fs::read(&p)?)
        } else {
            Ok(Vec::new())
        }
    };
    // Pre-image blob for a path: prefer the hash recorded on the change, fall
    // back to the head manifest entry.
    let old_for = |path: &str, recorded: &Option<Hash>| -> Result<Vec<u8>> {
        if let Some(h) = recorded {
            read_blob(h)
        } else if let Some(e) = head_files.get(path) {
            read_blob(&e.blob_hash)
        } else {
            Ok(Vec::new())
        }
    };

    let path = &change.path;
    let (old_bytes, new_bytes, rename_header) = match change.change_type {
        ChangeType::Added => (Vec::new(), read_worktree(path)?, None),
        ChangeType::Deleted => (old_for(path, &change.old_blob_hash)?, Vec::new(), None),
        ChangeType::Modified => (
            old_for(path, &change.old_blob_hash)?,
            read_worktree(path)?,
            None,
        ),
        ChangeType::Renamed => {
            let old_path = change.old_path.as_deref().unwrap_or(path);
            let header = Some(format!("rename from {old_path}\nrename to {path}"));
            (
                old_for(old_path, &change.old_blob_hash)?,
                read_worktree(path)?,
                header,
            )
        }
    };

    // Mode change (e.g. chmod +x), read straight off the change.
    let mode_header = match (change.old_mode, change.new_mode) {
        (Some(o), Some(n)) if o != n => Some(format!(
            "old mode {}\nnew mode {}",
            mode_octal(o),
            mode_octal(n)
        )),
        _ => None,
    };

    // Binary content can't be shown as a line diff. Emit a git-style notice so
    // the path still appears — status and commit count it, so diff must too.
    if is_binary(&old_bytes) || is_binary(&new_bytes) {
        let mut block = format!("diff --oak a/{path} b/{path}\n");
        if let Some(h) = &rename_header {
            block.push_str(h);
            block.push('\n');
        }
        if let Some(h) = &mode_header {
            block.push_str(h);
            block.push('\n');
        }
        block.push_str(&format!("Binary files a/{path} and b/{path} differ\n"));
        return Ok(block);
    }

    let old_str = String::from_utf8_lossy(&old_bytes);
    let new_str = String::from_utf8_lossy(&new_bytes);
    let diff = FileDiff::new(path, &old_str, &new_str);
    let unified = diff.to_unified();

    let mut block = String::new();
    if !unified.is_empty() {
        // Insert the rename/mode header right after the `diff --oak` line.
        let header = rename_header.as_deref().or(mode_header.as_deref());
        for line in unified.lines() {
            block.push_str(&output::format_diff_line(line));
            block.push('\n');
            if line.starts_with("diff --oak") {
                if let Some(h) = header {
                    block.push_str(h);
                    block.push('\n');
                }
            }
        }
    } else {
        // No textual hunk: a pure rename, a mode-only change, or a byte-level
        // change the line differ can't represent (e.g. a trailing-newline
        // difference). The blob hash still differs, so emit the header — never
        // drop the path.
        block.push_str(&format!("diff --oak a/{path} b/{path}\n"));
        if let Some(h) = &rename_header {
            block.push_str(h);
            block.push('\n');
        } else if let Some(h) = &mode_header {
            block.push_str(h);
            block.push('\n');
        } else {
            block.push_str(&format!("Files a/{path} and b/{path} differ\n"));
        }
    }
    Ok(block)
}

/// A file is treated as binary if it contains a NUL byte — the same heuristic
/// git uses to decide whether to print a textual diff or "Binary files differ".
fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

fn mode_octal(mode: FileMode) -> &'static str {
    match mode {
        FileMode::Regular => "100644",
        FileMode::Executable => "100755",
        FileMode::Symlink => "120000",
    }
}
