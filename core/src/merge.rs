//! Pure 3-way merge of manifests.
//!
//! Used by both `oak merge` (CLI, sync, sqlite/git backends) and the server's
//! `POST /branches/:name/merge` (async, postgres). The path-by-path merge is
//! identical between them; only the conflict resolution policy differs (CLI
//! writes conflict markers and supports `--continue`; the server returns 409).
//! That policy lives at the call site.
//!
//! The LCA walk is *not* extracted — it requires storage access and the
//! sync/async traits don't share a generic shape worth the noise. Each
//! caller still loads the base, branch, and parent manifests itself.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::{FileMode, Hash, Manifest, ManifestEntry};

/// Result of a 3-way merge: the entries that resolved cleanly, plus the
/// per-path conflicts the caller must decide how to handle.
#[derive(Debug, Clone)]
pub struct MergeOutcome {
    /// Entries that merged without conflict.
    pub clean_entries: Vec<ManifestEntry>,
    /// One entry per conflicting path. The caller picks a resolution
    /// strategy (write conflict markers, prefer one side, reject the merge).
    pub conflicts: Vec<MergeConflict>,
}

/// A single conflicting path. `branch_entry` / `parent_entry` are `None` when
/// the file was deleted on that side, so callers can distinguish
/// modify/modify, modify/delete, and delete/modify.
#[derive(Debug, Clone)]
pub struct MergeConflict {
    pub path: String,
    pub branch_entry: Option<ManifestEntry>,
    pub parent_entry: Option<ManifestEntry>,
}

/// Three-way merge `branch` and `parent` against their common ancestor `base`.
///
/// For each path that appears in any of the three manifests:
/// - If neither side changed it relative to base, keep it.
/// - If only one side changed it, take that side's value (including deletion).
/// - If both sides changed it identically, keep that value.
/// - If both sides changed it differently, emit a `MergeConflict` and let the
///   caller decide what (if anything) ends up in the final manifest.
///
/// This function does no I/O and never reads blob contents — text vs. binary
/// classification, conflict-marker generation, and "merge-fail vs. continue"
/// policy all live above this layer.
pub fn three_way_merge_manifests(
    base: &Manifest,
    branch: &Manifest,
    parent: &Manifest,
) -> MergeOutcome {
    // A file's merge identity is `(content, mode)`, not content alone: a
    // `chmod +x` with no content edit is still a change to carry forward, and a
    // file whose two sides agree on content but disagree on mode is still a
    // conflict. Keying change-detection on `blob_hash` only silently dropped
    // mode flips at merge time (executable bits reset on every squash to main).
    fn sig(e: &ManifestEntry) -> (&Hash, FileMode) {
        (&e.blob_hash, e.mode)
    }

    let base_map: HashMap<&str, (&Hash, FileMode)> = base
        .entries
        .iter()
        .map(|e| (e.path.as_str(), sig(e)))
        .collect();
    let branch_map: HashMap<&str, &ManifestEntry> = branch
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();
    let parent_map: HashMap<&str, &ManifestEntry> = parent
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();

    let all_paths: HashSet<&str> = base_map
        .keys()
        .chain(branch_map.keys())
        .chain(parent_map.keys())
        .copied()
        .collect();

    let mut clean_entries: Vec<ManifestEntry> = Vec::new();
    let mut conflicts: Vec<MergeConflict> = Vec::new();

    for path in &all_paths {
        let base_sig = base_map.get(path).copied();
        let branch_entry = branch_map.get(path).copied();
        let parent_entry = parent_map.get(path).copied();

        let branch_sig = branch_entry.map(sig);
        let parent_sig = parent_entry.map(sig);

        let branch_changed = branch_sig != base_sig;
        let parent_changed = parent_sig != base_sig;

        match (branch_changed, parent_changed) {
            (false, false) => {
                if let Some(entry) = branch_entry.or(parent_entry) {
                    clean_entries.push(entry.clone());
                }
            }
            (true, false) => {
                if let Some(entry) = branch_entry {
                    clean_entries.push(entry.clone());
                }
            }
            (false, true) => {
                if let Some(entry) = parent_entry {
                    clean_entries.push(entry.clone());
                }
            }
            (true, true) => {
                if branch_sig == parent_sig {
                    if let Some(entry) = branch_entry.or(parent_entry) {
                        clean_entries.push(entry.clone());
                    }
                } else {
                    conflicts.push(MergeConflict {
                        path: path.to_string(),
                        branch_entry: branch_entry.cloned(),
                        parent_entry: parent_entry.cloned(),
                    });
                }
            }
        }
    }

    MergeOutcome {
        clean_entries,
        conflicts,
    }
}

/// Outcome of a line-level 3-way text merge.
///
/// `Clean` means diffy reconciled both sides with no overlapping hunks — the
/// string is the fully merged file, safe to store as-is. `Conflicted` means at
/// least one region diverged; the string still contains the *whole* file but
/// with diff3-style `<<<<<<< / ||||||| original / ======= / >>>>>>>` markers
/// around the diverging hunks (the `|||||||` section is the base), so a human
/// (or the web resolver) can pick the right content.
#[derive(Debug, Clone)]
pub enum TextMerge {
    Clean(String),
    Conflicted(String),
}

/// Line-level 3-way merge of two text revisions against their common ancestor.
///
/// This is the content-level counterpart to [`three_way_merge_manifests`]:
/// once the manifest merge has decided a path was edited on *both* sides (so
/// the blob hashes diverge), this decides whether those edits actually
/// overlap. Non-overlapping edits to the same file come back `Clean`; only
/// genuinely conflicting regions produce `Conflicted` with markers.
///
/// `base` is the ancestor content (empty string when the file was added on
/// both sides with no common base — diffy then treats the whole file as the
/// conflict, which is the correct behavior).
///
/// Conflict markers always sit on lines of their own, even when a side's
/// conflicting content ends in an unterminated last line; that line gains a
/// newline inside the conflict block. A clean merge keeps the winning side's
/// missing trailing newline exactly.
pub fn three_way_merge_text(base: &str, branch: &str, parent: &str) -> TextMerge {
    // diffy writes each marker straight after the preceding section, so an
    // unterminated last line inside a conflict comes out as `…css=======` /
    // `…css>>>>>>> theirs`: the markers land mid-line, where editors, people
    // and column-0 marker scans don't see them. Tag every unterminated last
    // line with `sentinel + '\n'` first. diffy compares whole lines including
    // their terminator, and the sentinel occurs in no input, so line identity
    // — and therefore the merge — is exactly what it would have been. Then
    // drop the tags: the file's final line goes back to unterminated, and any
    // tagged line inside a conflict block keeps a plain newline so the marker
    // after it starts a fresh line.
    let sentinel = unterminated_line_sentinel(&[base, branch, parent]);
    let tagged_eol = format!("{sentinel}\n");
    fn tag<'a>(text: &'a str, tagged_eol: &str) -> Cow<'a, str> {
        if text.is_empty() || text.ends_with('\n') {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(format!("{text}{tagged_eol}"))
        }
    }
    let untag = |merged: String| match merged.strip_suffix(&tagged_eol) {
        Some(body) => body.replace(&tagged_eol, "\n"),
        None => merged.replace(&tagged_eol, "\n"),
    };
    let (base, branch, parent) = (
        tag(base, &tagged_eol),
        tag(branch, &tagged_eol),
        tag(parent, &tagged_eol),
    );
    // diff3 is diffy's default; pin it anyway. Conflict files already on disk
    // use it, and `oak conflict take` / the `--continue` gates parse it.
    let mut options = diffy::MergeOptions::new();
    options.set_conflict_style(diffy::ConflictStyle::Diff3);
    match options.merge(&base, &branch, &parent) {
        Ok(clean) => TextMerge::Clean(untag(clean)),
        Err(conflicted) => TextMerge::Conflicted(untag(conflicted)),
    }
}

/// A marker string that occurs in none of `texts`, used by
/// [`three_way_merge_text`] to tag unterminated last lines.
fn unterminated_line_sentinel(texts: &[&str]) -> String {
    let mut sentinel = String::from("\u{1}oak:no-newline-at-eof\u{1}");
    while texts.iter().any(|text| text.contains(&sentinel)) {
        sentinel.push('\u{1}');
    }
    sentinel
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hash_string, FileMode};

    // --- line-level text merge ---

    #[test]
    fn text_merge_non_overlapping_edits_are_clean() {
        // Base has two distant regions; branch edits the top, parent edits the
        // bottom. They don't touch the same lines, so the merge is clean — this
        // is the case that used to be reported as a conflict at the file level.
        let base = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n";
        let branch = "CHANGED1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n";
        let parent = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nCHANGED8\n";
        match three_way_merge_text(base, branch, parent) {
            TextMerge::Clean(merged) => {
                assert!(merged.contains("CHANGED1"));
                assert!(merged.contains("CHANGED8"));
                assert!(!merged.contains("<<<<<<<"));
            }
            TextMerge::Conflicted(c) => panic!("expected clean merge, got:\n{c}"),
        }
    }

    #[test]
    fn text_merge_overlapping_edits_conflict() {
        let base = "line1\nline2\nline3\n";
        let branch = "line1\nBRANCH\nline3\n";
        let parent = "line1\nPARENT\nline3\n";
        match three_way_merge_text(base, branch, parent) {
            TextMerge::Conflicted(c) => {
                assert!(c.contains("<<<<<<<"));
                assert!(c.contains("BRANCH"));
                assert!(c.contains("PARENT"));
            }
            TextMerge::Clean(s) => panic!("expected conflict, got clean:\n{s}"),
        }
    }

    /// Conflicts are diff3-style, and the base section is written even when
    /// it is empty (both sides inserted at the same spot) — `oak conflict
    /// take` relies on that shape.
    #[test]
    fn text_merge_conflict_is_diff3_with_empty_base_section() {
        let base = "a\nz\n";
        let branch = "a\nBRANCH\nz\n";
        let parent = "a\nPARENT\nz\n";
        match three_way_merge_text(base, branch, parent) {
            TextMerge::Conflicted(c) => assert_eq!(
                c,
                "a\n<<<<<<< ours\nBRANCH\n||||||| original\n=======\nPARENT\n>>>>>>> theirs\nz\n"
            ),
            TextMerge::Clean(s) => panic!("expected conflict, got clean:\n{s}"),
        }
    }

    /// Asserts `merged` holds exactly one (diff3-style) conflict block whose
    /// four markers each sit on a line of their own, wrapping `ours`, the
    /// `base` and `theirs`, and that no other line carries a marker.
    fn assert_one_well_formed_conflict(merged: &str, ours: &str, base: &str, theirs: &str) {
        let lines: Vec<&str> = merged.split('\n').collect();
        let find = |marker: &str| lines.iter().position(|l| *l == marker);
        let (Some(open), Some(orig), Some(mid), Some(close)) = (
            find("<<<<<<< ours"),
            find("||||||| original"),
            find("======="),
            find(">>>>>>> theirs"),
        ) else {
            panic!("markers must each be on their own line, got:\n{merged:?}");
        };
        assert!(
            open < orig && orig < mid && mid < close,
            "marker order: {merged:?}"
        );
        let section = |from: usize, to: usize| lines[from + 1..to].join("\n");
        assert_eq!(section(open, orig), ours.trim_end_matches('\n'));
        assert_eq!(section(orig, mid), base.trim_end_matches('\n'));
        assert_eq!(section(mid, close), theirs.trim_end_matches('\n'));
        for (i, line) in lines.iter().enumerate() {
            if [open, orig, mid, close].contains(&i) {
                continue;
            }
            assert!(
                !["<<<<<<<", "|||||||", "=======", ">>>>>>>"]
                    .iter()
                    .any(|m| line.contains(m)),
                "line {i} has a marker glued into content: {:?}",
                &line[line.len().saturating_sub(40)..]
            );
        }
    }

    /// Minified-asset shape (a tailwind build): a short header line, then a
    /// single huge line with no trailing newline. diffy writes `=======` and
    /// `>>>>>>>` straight after each side's content, so without a terminator
    /// they used to land mid-line — invisible to column-0 conflict scanners,
    /// which then committed the markered file as a clean merge.
    #[test]
    fn text_merge_single_long_line_without_trailing_newline_conflicts_on_own_lines() {
        let css = |tag: &str| format!(".a{{color:red}}{}.z{{margin:0}}", tag.repeat(20_000));
        let header = "/*! tailwindcss v4 | MIT License | https://tailwindcss.com */\n";
        let base = format!("{header}{}", css("b"));
        let ours = format!("{header}{}", css("o"));
        let theirs = format!("{header}{}", css("t"));
        match three_way_merge_text(&base, &ours, &theirs) {
            TextMerge::Conflicted(merged) => {
                assert!(merged.starts_with(header), "header merges clean");
                assert_one_well_formed_conflict(&merged, &css("o"), &css("b"), &css("t"));
            }
            TextMerge::Clean(_) => panic!("same line edited differently must conflict"),
        }
    }

    /// Every trailing-newline combination of (base, ours, theirs) on a
    /// single-line file whose one line both sides changed: always a
    /// conflict, always with markers on their own lines.
    #[test]
    fn text_merge_single_line_conflict_for_every_trailing_newline_variant() {
        let long = |c: &str| c.repeat(80_000);
        for mask in 0..8u8 {
            let eol = |bit: u8| if mask & bit != 0 { "\n" } else { "" };
            let base = format!("{}{}", long("b"), eol(1));
            let ours = format!("{}{}", long("o"), eol(2));
            let theirs = format!("{}{}", long("t"), eol(4));
            match three_way_merge_text(&base, &ours, &theirs) {
                TextMerge::Conflicted(merged) => {
                    assert_one_well_formed_conflict(&merged, &ours, &base, &theirs);
                    assert!(merged.ends_with(">>>>>>> theirs\n"), "mask {mask}");
                }
                TextMerge::Clean(_) => panic!("mask {mask}: expected a conflict"),
            }
        }
    }

    /// A conflict in the middle of a file whose (unchanged) last line has no
    /// terminator: markers on their own lines, and the merged file still
    /// ends without a newline.
    #[test]
    fn text_merge_conflict_keeps_unterminated_unchanged_tail() {
        let base = "head\nmid\ntail";
        let ours = "head\nOURS\ntail";
        let theirs = "head\nTHEIRS\ntail";
        match three_way_merge_text(base, ours, theirs) {
            TextMerge::Conflicted(merged) => {
                assert_eq!(
                    merged,
                    "head\n<<<<<<< ours\nOURS\n||||||| original\nmid\n=======\nTHEIRS\n>>>>>>> theirs\ntail"
                );
            }
            TextMerge::Clean(s) => panic!("expected conflict, got clean:\n{s}"),
        }
    }

    /// Clean merges must keep a missing trailing newline exactly as the
    /// winning side had it — the newline handling must not add or drop one.
    #[test]
    fn text_merge_clean_preserves_missing_trailing_newline() {
        let cases = [
            // ours edits the top, the unterminated tail is untouched.
            ("a\nb", "A\nb", "a\nb", "A\nb"),
            // theirs edits the unterminated last line, ours the first.
            ("a\nb\nc", "A\nb\nc", "a\nb\nC", "A\nb\nC"),
            // theirs adds the trailing newline; ours edits elsewhere.
            ("a\nb\nc", "A\nb\nc", "a\nb\nc\n", "A\nb\nc\n"),
            // ours drops the trailing newline; theirs edits elsewhere.
            ("a\nb\nc\n", "a\nb\nc", "A\nb\nc\n", "A\nb\nc"),
            // both sides make the same single-line edit.
            ("x", "y", "y", "y"),
        ];
        for (base, ours, theirs, want) in cases {
            match three_way_merge_text(base, ours, theirs) {
                TextMerge::Clean(merged) => assert_eq!(merged, want, "{base:?}"),
                TextMerge::Conflicted(c) => panic!("{base:?}: expected clean, got:\n{c}"),
            }
        }
    }

    fn entry(path: &str, content: &str) -> ManifestEntry {
        entry_mode(path, content, FileMode::Regular)
    }

    fn entry_mode(path: &str, content: &str, mode: FileMode) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            blob_hash: hash_string(content),
            mode,
        }
    }

    fn manifest(entries: Vec<ManifestEntry>) -> Manifest {
        Manifest::new(entries)
    }

    #[test]
    fn unchanged_file_kept() {
        let m = manifest(vec![entry("a.txt", "v1")]);
        let out = three_way_merge_manifests(&m, &m, &m);
        assert!(out.conflicts.is_empty());
        assert_eq!(out.clean_entries.len(), 1);
        assert_eq!(out.clean_entries[0].path, "a.txt");
    }

    #[test]
    fn one_side_modify_other_unchanged() {
        let base = manifest(vec![entry("a.txt", "v1")]);
        let branch = manifest(vec![entry("a.txt", "v2")]);
        let parent = base.clone();
        let out = three_way_merge_manifests(&base, &branch, &parent);
        assert!(out.conflicts.is_empty());
        assert_eq!(out.clean_entries[0].blob_hash, hash_string("v2"));
    }

    #[test]
    fn both_sides_identical_change_is_clean() {
        let base = manifest(vec![entry("a.txt", "v1")]);
        let branch = manifest(vec![entry("a.txt", "v2")]);
        let parent = manifest(vec![entry("a.txt", "v2")]);
        let out = three_way_merge_manifests(&base, &branch, &parent);
        assert!(out.conflicts.is_empty());
    }

    #[test]
    fn both_sides_different_change_conflicts() {
        let base = manifest(vec![entry("a.txt", "v1")]);
        let branch = manifest(vec![entry("a.txt", "v2-branch")]);
        let parent = manifest(vec![entry("a.txt", "v2-parent")]);
        let out = three_way_merge_manifests(&base, &branch, &parent);
        assert_eq!(out.conflicts.len(), 1);
        let c = &out.conflicts[0];
        assert_eq!(c.path, "a.txt");
        assert_eq!(
            c.branch_entry.as_ref().map(|e| &e.blob_hash),
            Some(&hash_string("v2-branch"))
        );
        assert_eq!(
            c.parent_entry.as_ref().map(|e| &e.blob_hash),
            Some(&hash_string("v2-parent"))
        );
    }

    #[test]
    fn modify_delete_is_conflict() {
        let base = manifest(vec![entry("a.txt", "v1")]);
        let branch = manifest(vec![entry("a.txt", "v2")]);
        let parent = manifest(vec![]);
        let out = three_way_merge_manifests(&base, &branch, &parent);
        assert_eq!(out.conflicts.len(), 1);
        assert!(out.conflicts[0].branch_entry.is_some());
        assert!(out.conflicts[0].parent_entry.is_none());
    }

    #[test]
    fn both_delete_is_clean_with_no_entry() {
        let base = manifest(vec![entry("a.txt", "v1")]);
        let branch = manifest(vec![]);
        let parent = manifest(vec![]);
        let out = three_way_merge_manifests(&base, &branch, &parent);
        assert!(out.conflicts.is_empty());
        assert!(out.clean_entries.is_empty());
    }

    #[test]
    fn add_on_one_side_only() {
        let base = manifest(vec![]);
        let branch = manifest(vec![entry("new.txt", "x")]);
        let parent = manifest(vec![]);
        let out = three_way_merge_manifests(&base, &branch, &parent);
        assert!(out.conflicts.is_empty());
        assert_eq!(out.clean_entries.len(), 1);
        assert_eq!(out.clean_entries[0].path, "new.txt");
    }

    #[test]
    fn add_same_file_both_sides_identical_is_clean() {
        let base = manifest(vec![]);
        let branch = manifest(vec![entry("new.txt", "x")]);
        let parent = manifest(vec![entry("new.txt", "x")]);
        let out = three_way_merge_manifests(&base, &branch, &parent);
        assert!(out.conflicts.is_empty());
        assert_eq!(out.clean_entries.len(), 1);
    }

    #[test]
    fn add_same_file_both_sides_different_is_conflict() {
        let base = manifest(vec![]);
        let branch = manifest(vec![entry("new.txt", "from-branch")]);
        let parent = manifest(vec![entry("new.txt", "from-parent")]);
        let out = three_way_merge_manifests(&base, &branch, &parent);
        assert_eq!(out.conflicts.len(), 1);
    }

    // --- mode (executable bit) is part of a file's merge identity ---

    /// A `chmod +x` with no content edit, while the other side leaves the file
    /// alone, must carry the executable bit through cleanly. Regression for the
    /// bug where merge keyed on `blob_hash` only and the bit was kept by
    /// accident (or lost as soon as the other side touched the file).
    #[test]
    fn chmod_only_one_side_keeps_executable() {
        let base = manifest(vec![entry_mode("s.sh", "x", FileMode::Regular)]);
        let branch = manifest(vec![entry_mode("s.sh", "x", FileMode::Executable)]);
        let parent = base.clone();
        let out = three_way_merge_manifests(&base, &branch, &parent);
        assert!(out.conflicts.is_empty());
        assert_eq!(out.clean_entries.len(), 1);
        assert_eq!(out.clean_entries[0].mode, FileMode::Executable);
    }

    /// Same as above but with the sides swapped: only the parent (main) flips
    /// the bit. The mode change must survive even though the branch is the
    /// "unchanged" side.
    #[test]
    fn chmod_only_other_side_keeps_executable() {
        let base = manifest(vec![entry_mode("s.sh", "x", FileMode::Regular)]);
        let branch = base.clone();
        let parent = manifest(vec![entry_mode("s.sh", "x", FileMode::Executable)]);
        let out = three_way_merge_manifests(&base, &branch, &parent);
        assert!(out.conflicts.is_empty());
        assert_eq!(out.clean_entries.len(), 1);
        assert_eq!(out.clean_entries[0].mode, FileMode::Executable);
    }

    /// The reset that motivated the fix: the branch flips the bit (no content
    /// change) while main edits the file's content. Content hash matched base
    /// on the branch side, so the old hash-only logic took main's entry and
    /// silently dropped the `+x`. Divergent identities must now conflict so the
    /// user resolves it rather than losing the bit.
    #[test]
    fn chmod_vs_concurrent_content_edit_conflicts() {
        let base = manifest(vec![entry_mode("s.sh", "v1", FileMode::Regular)]);
        let branch = manifest(vec![entry_mode("s.sh", "v1", FileMode::Executable)]);
        let parent = manifest(vec![entry_mode("s.sh", "v2", FileMode::Regular)]);
        let out = three_way_merge_manifests(&base, &branch, &parent);
        assert_eq!(out.conflicts.len(), 1);
        assert_eq!(out.conflicts[0].path, "s.sh");
    }

    /// Both sides flip to the *same* mode with identical content: that's an
    /// agreed-upon change, not a conflict.
    #[test]
    fn both_sides_same_chmod_is_clean() {
        let base = manifest(vec![entry_mode("s.sh", "x", FileMode::Regular)]);
        let branch = manifest(vec![entry_mode("s.sh", "x", FileMode::Executable)]);
        let parent = manifest(vec![entry_mode("s.sh", "x", FileMode::Executable)]);
        let out = three_way_merge_manifests(&base, &branch, &parent);
        assert!(out.conflicts.is_empty());
        assert_eq!(out.clean_entries[0].mode, FileMode::Executable);
    }

    /// Same content, but the two sides pick *different* modes: a genuine
    /// mode/mode conflict that hash-only detection couldn't see.
    #[test]
    fn both_sides_different_chmod_conflicts() {
        let base = manifest(vec![entry_mode("s.sh", "x", FileMode::Regular)]);
        let branch = manifest(vec![entry_mode("s.sh", "x", FileMode::Executable)]);
        let parent = manifest(vec![entry_mode("s.sh", "x", FileMode::Symlink)]);
        let out = three_way_merge_manifests(&base, &branch, &parent);
        assert_eq!(out.conflicts.len(), 1);
    }
}
