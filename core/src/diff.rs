use similar::{ChangeTag, TextDiff};

/// Default number of context lines to show around changes
pub const DEFAULT_CONTEXT_LINES: usize = 3;

/// Maximum byte length for either side of a text diff. Past this, content is
/// treated as non-text: the line differ's cost grows superlinearly and the
/// result is unreadable, so callers emit a notice instead of diffing. Mirrors
/// the spirit of git's `core.bigFileThreshold`.
pub const MAX_TEXT_DIFF_BYTES: usize = 16 * 1024 * 1024;

/// A file is treated as binary if it contains a NUL byte — the same heuristic
/// git uses to decide whether to print a textual diff or "Binary files differ".
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

/// Returns a stand-in notice when two blobs should not be rendered as a text
/// diff — either side is binary, or either exceeds [`MAX_TEXT_DIFF_BYTES`].
/// `None` means it's safe (and worthwhile) to run [`FileDiff::new`].
///
/// Every diff-rendering path should gate on this *before* reading blobs into
/// strings and calling the line differ: a NUL-laden or multi-megabyte blob (an
/// asset, a packfile, a binary in an import commit) makes `TextDiff` both
/// useless and pathologically slow, which is what otherwise freezes `oak log`
/// on large repos.
pub fn binary_or_large_notice(path: &str, old_bytes: &[u8], new_bytes: &[u8]) -> Option<String> {
    binary_or_oversize_notice(path, old_bytes, new_bytes, Some(MAX_TEXT_DIFF_BYTES))
}

/// Like [`binary_or_large_notice`], but callers may pass `None` to force a
/// text diff for oversized blobs while preserving the NUL-byte binary guard.
pub fn binary_or_oversize_notice(
    path: &str,
    old_bytes: &[u8],
    new_bytes: &[u8],
    max_text_bytes: Option<usize>,
) -> Option<String> {
    if is_binary(old_bytes) || is_binary(new_bytes) {
        return Some(format!("Binary files a/{path} and b/{path} differ"));
    }
    if max_text_bytes.is_some_and(|limit| old_bytes.len() > limit || new_bytes.len() > limit) {
        return Some(format!(
            "Binary files a/{path} and b/{path} differ ({} -> {} bytes; text diff omitted, use --text to force)",
            old_bytes.len(),
            new_bytes.len()
        ));
    }
    None
}

/// Byte ranges that differ between a paired removed/added line, computed
/// with a word-level diff: `(ranges_in_old, ranges_in_new)`.
///
/// Powers word-level emphasis — the diff browser's intra-line highlights
/// and `oak diff --word-diff`. Returns `None` when the pair is mostly
/// different (similarity below 0.4): highlighting nearly everything reads
/// worse than a plain replaced line.
/// Byte ranges of intra-line changes, one list per side of a paired line.
pub type InlineRanges = Vec<std::ops::Range<usize>>;

pub fn inline_diff_ranges(old: &str, new: &str) -> Option<(InlineRanges, InlineRanges)> {
    // Word tokens (whitespace runs are their own tokens, so values
    // concatenate back to the exact input and byte offsets stay honest).
    let diff = TextDiff::from_words(old, new);
    if diff.ratio() < 0.4 {
        return None;
    }
    let mut old_ranges: InlineRanges = Vec::new();
    let mut new_ranges: InlineRanges = Vec::new();
    let (mut old_pos, mut new_pos) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        let len = change.value().len();
        match change.tag() {
            ChangeTag::Equal => {
                old_pos += len;
                new_pos += len;
            }
            ChangeTag::Delete => {
                match old_ranges.last_mut() {
                    // Coalesce adjacent changed chars into one range.
                    Some(last) if last.end == old_pos => last.end = old_pos + len,
                    _ => old_ranges.push(old_pos..old_pos + len),
                }
                old_pos += len;
            }
            ChangeTag::Insert => {
                match new_ranges.last_mut() {
                    Some(last) if last.end == new_pos => last.end = new_pos + len,
                    _ => new_ranges.push(new_pos..new_pos + len),
                }
                new_pos += len;
            }
        }
    }
    Some((old_ranges, new_ranges))
}

/// Represents a line change in a diff
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    /// Context line (unchanged)
    Context(String),
    /// Added line
    Added(String),
    /// Removed line
    Removed(String),
}

/// The unified-diff marker that follows a line whose side of the file ended
/// without a newline terminator. Applicators (`git apply`, `patch`) need it
/// to reproduce the exact final bytes; without it they either reject the
/// hunk or silently add a newline the intended content never had.
pub const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file";

/// Split text into lines that keep their `\n` terminator, splitting on `\n`
/// only. `similar`'s own line tokenizer also ends a line at a lone `\r`,
/// which git does not: a file `a\rb` is one line to `git apply`, and a side
/// ending in `c\r` has no newline. Only the final line can lack a `\n`.
fn split_lines_keep_newline(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut rest = text;
    while let Some(idx) = rest.find('\n') {
        lines.push(&rest[..=idx]);
        rest = &rest[idx + 1..];
    }
    if !rest.is_empty() {
        lines.push(rest);
    }
    lines
}

/// Result of diffing two files
///
/// Construct with [`FileDiff::new`]; the struct is `#[non_exhaustive]` so
/// fields can be added without breaking downstream crates again.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FileDiff {
    pub path: String,
    pub lines: Vec<DiffLine>,
    pub has_changes: bool,
    /// Indices into `lines` whose content was the final line of its side and
    /// carried no newline terminator (at most one per side, so at most two).
    /// The line strings themselves are always terminator-free, so this is the
    /// only record of that information; a faithful renderer emits
    /// [`NO_NEWLINE_MARKER`] right after each of these lines.
    pub missing_newline_at: Vec<usize>,
}

impl FileDiff {
    /// Create a diff between two text contents
    pub fn new(path: &str, old: &str, new: &str) -> Self {
        let old_lines = split_lines_keep_newline(old);
        let new_lines = split_lines_keep_newline(new);
        let diff = TextDiff::configure().diff_slices(&old_lines, &new_lines);
        let mut lines = Vec::new();
        let mut has_changes = false;
        let mut missing_newline_at = Vec::new();

        for change in diff.iter_all_changes() {
            // Line tokens keep their terminator, so a final `c` and a final
            // `c\n` are different tokens and never pair up as one Equal.
            // Record the missing terminator before stripping it.
            let value: &str = change.value();
            if !value.ends_with('\n') {
                missing_newline_at.push(lines.len());
            }
            let line = value.strip_suffix('\n').unwrap_or(value).to_string();
            match change.tag() {
                ChangeTag::Equal => {
                    lines.push(DiffLine::Context(line));
                }
                ChangeTag::Delete => {
                    lines.push(DiffLine::Removed(line));
                    has_changes = true;
                }
                ChangeTag::Insert => {
                    lines.push(DiffLine::Added(line));
                    has_changes = true;
                }
            }
        }

        FileDiff {
            path: path.to_string(),
            lines,
            has_changes,
            missing_newline_at,
        }
    }

    /// Format the diff as a unified diff string with context lines (like git diff)
    pub fn to_unified(&self) -> String {
        self.to_unified_with_context(DEFAULT_CONTEXT_LINES)
    }

    /// Format the diff as a unified diff string with a custom number of context lines
    pub fn to_unified_with_context(&self, context: usize) -> String {
        let mut output = String::new();

        if !self.has_changes {
            return output;
        }

        output.push_str(&format!("diff --oak a/{} b/{}\n", self.path, self.path));
        output.push_str(&format!("--- a/{}\n", self.path));
        output.push_str(&format!("+++ b/{}\n", self.path));

        // Find hunks: groups of changes with their surrounding context
        let hunks = self.compute_hunks(context);

        for hunk in hunks {
            for line in hunk.unified_lines() {
                output.push_str(&line);
                output.push('\n');
            }
        }

        output
    }

    /// Get hunks for rendering diffs with the given number of context lines
    pub fn hunks(&self, context: usize) -> Vec<Hunk> {
        self.compute_hunks(context)
    }

    /// Compute hunks for unified diff output
    fn compute_hunks(&self, context: usize) -> Vec<Hunk> {
        if self.lines.is_empty() {
            return vec![];
        }

        // First, find all indices where there are changes
        let change_indices: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| !matches!(line, DiffLine::Context(_)))
            .map(|(i, _)| i)
            .collect();

        if change_indices.is_empty() {
            return vec![];
        }

        // Group changes into hunks - changes are in the same hunk if they're
        // within 2*context lines of each other
        let mut hunks = Vec::new();
        let mut hunk_start = change_indices[0];
        let mut hunk_end = change_indices[0];

        for &idx in &change_indices[1..] {
            // If this change is close enough to the previous one, extend the hunk
            if idx <= hunk_end + 2 * context + 1 {
                hunk_end = idx;
            } else {
                // Start a new hunk
                hunks.push(self.create_hunk(hunk_start, hunk_end, context));
                hunk_start = idx;
                hunk_end = idx;
            }
        }
        // Don't forget the last hunk
        hunks.push(self.create_hunk(hunk_start, hunk_end, context));

        hunks
    }

    /// Create a hunk from a range of change indices
    fn create_hunk(&self, first_change: usize, last_change: usize, context: usize) -> Hunk {
        // Calculate the actual range including context
        let start = first_change.saturating_sub(context);
        let end = (last_change + context + 1).min(self.lines.len());

        // Collect the lines for this hunk
        let lines: Vec<DiffLine> = self.lines[start..end].to_vec();
        // Re-base the missing-terminator record onto the hunk's own indices;
        // a final line that falls outside this hunk's context needs no marker
        // here (the hunk never mentions it), exactly as git renders it.
        let missing_newline_at: Vec<usize> = self
            .missing_newline_at
            .iter()
            .filter(|&&idx| idx >= start && idx < end)
            .map(|&idx| idx - start)
            .collect();

        // Calculate line numbers for the hunk header
        // We need to track old line number and new line number
        let mut old_line = 1;
        let mut new_line = 1;

        // Count lines up to the start of the hunk
        for line in &self.lines[..start] {
            match line {
                DiffLine::Context(_) => {
                    old_line += 1;
                    new_line += 1;
                }
                DiffLine::Removed(_) => {
                    old_line += 1;
                }
                DiffLine::Added(_) => {
                    new_line += 1;
                }
            }
        }

        let old_start = old_line;
        let new_start = new_line;

        // Count lines in the hunk
        let mut old_count = 0;
        let mut new_count = 0;
        for line in &lines {
            match line {
                DiffLine::Context(_) => {
                    old_count += 1;
                    new_count += 1;
                }
                DiffLine::Removed(_) => {
                    old_count += 1;
                }
                DiffLine::Added(_) => {
                    new_count += 1;
                }
            }
        }

        Hunk::new(
            old_start,
            old_count,
            new_start,
            new_count,
            lines,
            missing_newline_at,
        )
    }
}

/// A hunk in a unified diff
///
/// Produced by [`FileDiff::hunks`]; build one by hand with [`Hunk::new`].
/// `#[non_exhaustive]` so fields can be added without breaking downstream
/// crates again.
#[non_exhaustive]
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<DiffLine>,
    /// Indices into `lines` that must be followed by [`NO_NEWLINE_MARKER`];
    /// see [`FileDiff::missing_newline_at`].
    pub missing_newline_at: Vec<usize>,
}

impl Hunk {
    /// Assemble a hunk. `missing_newline_at` indexes `lines`.
    pub fn new(
        old_start: usize,
        old_count: usize,
        new_start: usize,
        new_count: usize,
        lines: Vec<DiffLine>,
        missing_newline_at: Vec<usize>,
    ) -> Self {
        Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
            lines,
            missing_newline_at,
        }
    }

    /// The `@@ -a,b +c,d @@` header line (no trailing newline).
    pub fn header(&self) -> String {
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_count, self.new_start, self.new_count
        )
    }

    /// The hunk as unified-diff lines without trailing newlines: the header,
    /// then each ` `/`+`/`-` prefixed line, with [`NO_NEWLINE_MARKER`] after
    /// any line whose side of the file ended there without a terminator.
    ///
    /// Every renderer that prints hunk bodies should go through this so the
    /// applicator-facing details (prefixes, markers) cannot drift apart.
    pub fn unified_lines(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.lines.len() + 1 + self.missing_newline_at.len());
        out.push(self.header());
        for (idx, line) in self.lines.iter().enumerate() {
            out.push(match line {
                DiffLine::Context(s) => format!(" {s}"),
                DiffLine::Added(s) => format!("+{s}"),
                DiffLine::Removed(s) => format!("-{s}"),
            });
            if self.missing_newline_at.contains(&idx) {
                out.push(NO_NEWLINE_MARKER.to_string());
            }
        }
        out
    }
}

/// Represents overall status of a file
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// File is new (not in previous commit)
    Added,
    /// File was modified
    Modified,
    /// File was deleted
    Deleted,
    /// File is unchanged
    Unchanged,
}

impl std::fmt::Display for FileStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileStatus::Added => write!(f, "A"),
            FileStatus::Modified => write!(f, "M"),
            FileStatus::Deleted => write!(f, "D"),
            FileStatus::Unchanged => write!(f, " "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diff_no_changes() {
        let content = "line1\nline2\nline3\n";
        let diff = FileDiff::new("test.txt", content, content);
        assert!(!diff.has_changes);
    }

    #[test]
    fn test_diff_added_lines() {
        let old = "line1\nline2\n";
        let new = "line1\nline2\nline3\n";
        let diff = FileDiff::new("test.txt", old, new);
        assert!(diff.has_changes);
        assert!(diff.lines.iter().any(|l| matches!(l, DiffLine::Added(_))));
    }

    #[test]
    fn test_diff_removed_lines() {
        let old = "line1\nline2\nline3\n";
        let new = "line1\nline2\n";
        let diff = FileDiff::new("test.txt", old, new);
        assert!(diff.has_changes);
        assert!(diff.lines.iter().any(|l| matches!(l, DiffLine::Removed(_))));
    }

    #[test]
    fn test_diff_modified_lines() {
        let old = "line1\nline2\nline3\n";
        let new = "line1\nmodified\nline3\n";
        let diff = FileDiff::new("test.txt", old, new);
        assert!(diff.has_changes);
    }

    #[test]
    fn test_unified_output() {
        let old = "line1\nline2\n";
        let new = "line1\nline2\nline3\n";
        let diff = FileDiff::new("test.txt", old, new);
        let output = diff.to_unified();
        assert!(output.contains("diff --oak"));
        assert!(output.contains("@@"));
        assert!(output.contains("+line3"));
    }

    #[test]
    fn test_unified_shows_limited_context() {
        // Create a file with many lines, change one in the middle
        let old = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let new = (1..=20)
            .map(|i| {
                if i == 10 {
                    "modified".to_string()
                } else {
                    format!("line{i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";

        let diff = FileDiff::new("test.txt", &old, &new);
        let output = diff.to_unified();

        // Should have hunk header
        assert!(output.contains("@@"));

        // Should NOT contain line1 (too far from change at line 10)
        assert!(
            !output.contains(" line1\n"),
            "Should not show line1 (too far from change)"
        );
        assert!(
            !output.contains(" line2\n"),
            "Should not show line2 (too far from change)"
        );
        assert!(
            !output.contains(" line3\n"),
            "Should not show line3 (too far from change)"
        );
        assert!(
            !output.contains(" line4\n"),
            "Should not show line4 (too far from change)"
        );
        assert!(
            !output.contains(" line5\n"),
            "Should not show line5 (too far from change)"
        );
        assert!(
            !output.contains(" line6\n"),
            "Should not show line6 (too far from change)"
        );

        // Should contain lines near the change (line 7, 8, 9 as context before)
        assert!(
            output.contains(" line7\n"),
            "Should show line7 (context before)"
        );
        assert!(
            output.contains(" line8\n"),
            "Should show line8 (context before)"
        );
        assert!(
            output.contains(" line9\n"),
            "Should show line9 (context before)"
        );

        // Should contain the change
        assert!(output.contains("-line10\n"), "Should show removed line10");
        assert!(output.contains("+modified\n"), "Should show added modified");

        // Should contain lines after (line 11, 12, 13 as context after)
        assert!(
            output.contains(" line11\n"),
            "Should show line11 (context after)"
        );
        assert!(
            output.contains(" line12\n"),
            "Should show line12 (context after)"
        );
        assert!(
            output.contains(" line13\n"),
            "Should show line13 (context after)"
        );

        // Should NOT contain line20 (too far from change)
        assert!(
            !output.contains(" line20\n"),
            "Should not show line20 (too far from change)"
        );
    }

    #[test]
    fn test_multiple_hunks() {
        // Changes at line 2 and line 15 should create two separate hunks
        let old = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let new = (1..=20)
            .map(|i| {
                if i == 2 {
                    "change1".to_string()
                } else if i == 15 {
                    "change2".to_string()
                } else {
                    format!("line{i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";

        let diff = FileDiff::new("test.txt", &old, &new);
        let output = diff.to_unified();

        // Should have two hunk headers (changes are far enough apart)
        let hunk_count = output.matches("@@").count() / 2; // Each @@ appears twice per hunk header
        assert!(
            hunk_count >= 2,
            "Should have at least 2 hunks, got {hunk_count}"
        );
    }

    #[test]
    fn test_diff_empty_files() {
        let diff = FileDiff::new("empty.txt", "", "");
        assert!(!diff.has_changes);
        let output = diff.to_unified();
        assert!(output.is_empty());
    }

    #[test]
    fn test_diff_new_file_from_empty() {
        let diff = FileDiff::new("new.txt", "", "hello\nworld\n");
        assert!(diff.has_changes);
        let output = diff.to_unified();
        assert!(output.contains("+hello"));
        assert!(output.contains("+world"));
    }

    #[test]
    fn test_diff_delete_entire_file() {
        let diff = FileDiff::new("gone.txt", "hello\nworld\n", "");
        assert!(diff.has_changes);
        let output = diff.to_unified();
        assert!(output.contains("-hello"));
        assert!(output.contains("-world"));
    }

    #[test]
    fn test_diff_single_line_change() {
        let diff = FileDiff::new("f.txt", "one line\n", "different line\n");
        assert!(diff.has_changes);
        let output = diff.to_unified();
        assert!(output.contains("-one line"));
        assert!(output.contains("+different line"));
    }

    #[test]
    fn test_diff_no_trailing_newline() {
        let diff = FileDiff::new("f.txt", "no newline", "no newline");
        assert!(!diff.has_changes);
    }

    #[test]
    fn test_diff_crlf_vs_lf() {
        // CRLF and LF should be treated as different content
        let diff = FileDiff::new("f.txt", "line\r\n", "line\n");
        assert!(diff.has_changes);
    }

    // --- terminal-newline fidelity (fb521) -------------------------------
    //
    // Unified output must carry `\ No newline at end of file` exactly where
    // git puts it, so `git apply`/`patch` reproduce the intended bytes.
    // These compare whole strings: substring checks cannot catch a marker
    // that is missing, duplicated, or attached to the wrong line.

    fn hunk_body(path: &str, old: &str, new: &str, context: usize) -> String {
        let unified = FileDiff::new(path, old, new).to_unified_with_context(context);
        let header = format!("diff --oak a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n");
        unified
            .strip_prefix(&header)
            .unwrap_or_else(|| panic!("unexpected file header in {unified:?}"))
            .to_string()
    }

    #[test]
    fn missing_newline_on_both_sides_marks_removed_and_added_final_lines() {
        // The retained fb521 reproduction: `a\nb\nc` -> `a\nB\nc`.
        let diff = FileDiff::new("f.txt", "a\nb\nc", "a\nB\nc");
        // `c` is one Context line shared by both sides, so one record.
        assert_eq!(diff.missing_newline_at, vec![3]);
        assert_eq!(
            hunk_body("f.txt", "a\nb\nc", "a\nB\nc", 3),
            "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n\\ No newline at end of file\n"
        );
    }

    #[test]
    fn newline_only_transitions_render_as_a_marked_replace() {
        // Old side lacks the terminator, new side gains it.
        assert_eq!(
            hunk_body("f.txt", "a\nb\nc", "a\nb\nc\n", 3),
            "@@ -1,3 +1,3 @@\n a\n b\n-c\n\\ No newline at end of file\n+c\n"
        );
        // And the reverse: the marker moves to the added line.
        assert_eq!(
            hunk_body("f.txt", "a\nb\nc\n", "a\nb\nc", 3),
            "@@ -1,3 +1,3 @@\n a\n b\n-c\n+c\n\\ No newline at end of file\n"
        );
    }

    #[test]
    fn unterminated_final_line_that_changes_gets_a_marker_on_each_side() {
        let diff = FileDiff::new("f.txt", "a\nb\nc", "a\nb\nC");
        assert_eq!(diff.missing_newline_at, vec![2, 3]);
        assert_eq!(
            hunk_body("f.txt", "a\nb\nc", "a\nb\nC", 3),
            "@@ -1,3 +1,3 @@\n a\n b\n-c\n\\ No newline at end of file\n+C\n\\ No newline at end of file\n"
        );
        // Growing past an unterminated line: the old `c` is replaced by a
        // terminated `c` and a new unterminated `d`.
        assert_eq!(
            hunk_body("f.txt", "a\nb\nc", "a\nb\nc\nd", 3),
            "@@ -1,3 +1,4 @@\n a\n b\n-c\n\\ No newline at end of file\n+c\n+d\n\\ No newline at end of file\n"
        );
    }

    #[test]
    fn marker_follows_the_final_line_only_when_the_hunk_reaches_it() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh";
        let new = "a\nX\nc\nd\ne\nf\ng\nh";
        // Default context stops at `e`; the unterminated `h` is not in the
        // hunk, so no marker (git does the same).
        assert_eq!(
            hunk_body("f.txt", old, new, 3),
            "@@ -1,5 +1,5 @@\n a\n-b\n+X\n c\n d\n e\n"
        );
        // Wide context reaches `h`: marker after that context line, once.
        assert_eq!(
            hunk_body("f.txt", old, new, 10),
            "@@ -1,8 +1,8 @@\n a\n-b\n+X\n c\n d\n e\n f\n g\n h\n\\ No newline at end of file\n"
        );
        // Hunk indices are re-based: the marker index refers to the hunk's
        // own `lines`, not the whole diff.
        let diff = FileDiff::new("f.txt", old, new);
        let hunks = diff.hunks(10);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].missing_newline_at, vec![hunks[0].lines.len() - 1]);
        assert!(diff.hunks(3)[0].missing_newline_at.is_empty());
    }

    #[test]
    fn multiple_hunks_keep_their_count_and_only_the_last_can_carry_a_marker() {
        let old = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = old
            .replace("line2\n", "change1\n")
            .replace("line20", "change2");
        let diff = FileDiff::new("f.txt", &old, &new);
        let hunks = diff.hunks(3);
        assert_eq!(hunks.len(), 2, "two far-apart edits stay two hunks");
        assert!(hunks[0].missing_newline_at.is_empty());
        assert_eq!(hunks[1].missing_newline_at.len(), 2);
        let rendered = diff.to_unified();
        assert_eq!(rendered.matches("\n@@ ").count(), 2);
        assert!(rendered.ends_with(
            "-line20\n\\ No newline at end of file\n+change2\n\\ No newline at end of file\n"
        ));
    }

    #[test]
    fn added_and_deleted_unterminated_files_are_marked() {
        // (Empty ranges print `-1,0`/`+1,0` where git prints `0,0`; the
        // header numbering is a separate, applicator-tolerated concern and
        // is pinned here only so the marker placement is compared exactly.)
        assert_eq!(
            hunk_body("new.txt", "", "hello\nworld", 3),
            "@@ -1,0 +1,2 @@\n+hello\n+world\n\\ No newline at end of file\n"
        );
        assert_eq!(
            hunk_body("gone.txt", "hello\nworld", "", 3),
            "@@ -1,2 +1,0 @@\n-hello\n-world\n\\ No newline at end of file\n"
        );
        // Terminated content stays marker-free.
        assert_eq!(
            hunk_body("new.txt", "", "hello\nworld\n", 3),
            "@@ -1,0 +1,2 @@\n+hello\n+world\n"
        );
    }

    #[test]
    fn crlf_content_keeps_its_carriage_returns_and_marks_the_missing_lf() {
        assert_eq!(
            hunk_body("f.txt", "a\r\nb\r\nc", "a\r\nB\r\nc", 3),
            "@@ -1,3 +1,3 @@\n a\r\n-b\r\n+B\r\n c\n\\ No newline at end of file\n"
        );
        assert_eq!(
            hunk_body("f.txt", "a\r\nb", "a\r\nb\r\n", 3),
            "@@ -1,2 +1,2 @@\n a\r\n-b\n\\ No newline at end of file\n+b\r\n"
        );
    }

    #[test]
    fn lone_carriage_return_is_content_not_a_line_terminator() {
        // git sees `a\rb` as one line; so must the patch, or it cannot apply.
        let diff = FileDiff::new("f.txt", "a\rb", "a\rB");
        assert_eq!(
            diff.lines,
            vec![
                DiffLine::Removed("a\rb".to_string()),
                DiffLine::Added("a\rB".to_string())
            ]
        );
        assert_eq!(
            hunk_body("f.txt", "a\rb", "a\rB", 3),
            "@@ -1,1 +1,1 @@\n-a\rb\n\\ No newline at end of file\n+a\rB\n\\ No newline at end of file\n"
        );
        // A side ending in `c\r` has no LF: it must be marked, not silently
        // given one.
        assert_eq!(
            hunk_body("f.txt", "a\nc\r", "a\nc\r\n", 3),
            "@@ -1,2 +1,2 @@\n a\n-c\r\n\\ No newline at end of file\n+c\r\n"
        );
        // Mid-file lone `\r` stays inside its line on both sides.
        assert!(!FileDiff::new("f.txt", "x\ry\nz\n", "x\ry\nz\n").has_changes);
    }

    #[test]
    fn split_lines_keep_newline_splits_on_lf_only() {
        assert_eq!(split_lines_keep_newline(""), Vec::<&str>::new());
        assert_eq!(split_lines_keep_newline("\n"), vec!["\n"]);
        assert_eq!(split_lines_keep_newline("a"), vec!["a"]);
        assert_eq!(split_lines_keep_newline("a\nb"), vec!["a\n", "b"]);
        assert_eq!(
            split_lines_keep_newline("a\r\nb\r\n"),
            vec!["a\r\n", "b\r\n"]
        );
        assert_eq!(split_lines_keep_newline("a\rb\n"), vec!["a\rb\n"]);
        assert_eq!(split_lines_keep_newline("a\n\n"), vec!["a\n", "\n"]);
    }

    #[test]
    fn terminated_content_never_carries_a_marker() {
        for (old, new) in [
            ("a\nb\nc\n", "a\nB\nc\n"),
            ("", "\n"),
            ("\n", ""),
            ("a\n\n", "a\n"),
        ] {
            let diff = FileDiff::new("f.txt", old, new);
            assert!(diff.missing_newline_at.is_empty(), "{old:?} -> {new:?}");
            assert!(!diff.to_unified().contains(NO_NEWLINE_MARKER));
        }
    }

    #[test]
    fn hunk_unified_lines_match_the_unified_string() {
        let diff = FileDiff::new("f.txt", "a\nb\nc", "a\nB\nc\nd");
        let mut from_hunks = String::from("diff --oak a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n");
        for hunk in diff.hunks(3) {
            for line in hunk.unified_lines() {
                from_hunks.push_str(&line);
                from_hunks.push('\n');
            }
        }
        assert_eq!(from_hunks, diff.to_unified());
    }

    #[test]
    fn test_is_binary_detects_nul() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary(b"plain text\n"));
        assert!(!is_binary(b""));
    }

    #[test]
    fn test_binary_notice_for_nul_content() {
        let notice = binary_or_large_notice("a.bin", b"\0\0", b"\0\x01");
        assert_eq!(
            notice.as_deref(),
            Some("Binary files a/a.bin and b/a.bin differ")
        );
    }

    #[test]
    fn test_large_text_gets_notice() {
        let big = vec![b'a'; MAX_TEXT_DIFF_BYTES + 1];
        let notice = binary_or_large_notice("big.txt", b"small", &big);
        assert!(notice.is_some());
        assert!(notice.unwrap().contains("text diff omitted"));
    }

    #[test]
    fn test_force_text_keeps_binary_guard_but_overrides_size_guard() {
        let big = vec![b'a'; MAX_TEXT_DIFF_BYTES + 1];
        assert!(binary_or_oversize_notice("big.txt", b"small", &big, None).is_none());
        assert!(binary_or_oversize_notice("bin.dat", b"\0", &big, None).is_some());
    }

    #[test]
    fn test_normal_text_no_notice() {
        assert!(binary_or_large_notice("f.txt", b"old\n", b"new\n").is_none());
    }

    #[test]
    fn test_diff_with_custom_context() {
        let old = "line1\nline2\nline3\nline4\nline5\n";
        let new = "line1\nline2\nchanged\nline4\nline5\n";
        let diff = FileDiff::new("f.txt", old, new);
        let output = diff.to_unified_with_context(1);
        // With context of 1, should show line2 and line4 as context
        assert!(output.contains(" line2"));
        assert!(output.contains("-line3"));
        assert!(output.contains("+changed"));
        assert!(output.contains(" line4"));
        // Should NOT show line1 with context=1
        assert!(!output.contains(" line1"));
    }

    #[test]
    fn inline_diff_ranges_marks_changed_words_with_exact_byte_offsets() {
        let old = "the quick brown fox jumps";
        let new = "the quick red fox leaps";
        let (old_ranges, new_ranges) = inline_diff_ranges(old, new).expect("similar enough");
        let old_words: Vec<&str> = old_ranges.iter().map(|r| &old[r.clone()]).collect();
        let new_words: Vec<&str> = new_ranges.iter().map(|r| &new[r.clone()]).collect();
        assert_eq!(old_words, vec!["brown", "jumps"]);
        assert_eq!(new_words, vec!["red", "leaps"]);
    }

    #[test]
    fn inline_diff_ranges_declines_mostly_different_lines() {
        assert!(inline_diff_ranges("AAA", "BBB").is_none());
    }

    #[test]
    fn inline_diff_ranges_handles_multibyte_content() {
        let old = "prefix héllo suffix";
        let new = "prefix wörld suffix";
        let (old_ranges, new_ranges) = inline_diff_ranges(old, new).expect("similar enough");
        // Ranges must be valid char boundaries — slicing panics otherwise.
        for r in &old_ranges {
            let _ = &old[r.clone()];
        }
        for r in &new_ranges {
            let _ = &new[r.clone()];
        }
    }
}
