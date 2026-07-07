use similar::{ChangeTag, TextDiff};

/// Default number of context lines to show around changes
pub const DEFAULT_CONTEXT_LINES: usize = 3;

/// Maximum byte length for either side of a text diff. Past this, content is
/// treated as non-text: the line differ's cost grows superlinearly and the
/// result is unreadable, so callers emit a notice instead of diffing. Mirrors
/// the spirit of git's `core.bigFileThreshold`.
pub const MAX_TEXT_DIFF_BYTES: usize = 1024 * 1024;

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
    if is_binary(old_bytes) || is_binary(new_bytes) {
        return Some(format!("Binary files a/{path} and b/{path} differ"));
    }
    if old_bytes.len() > MAX_TEXT_DIFF_BYTES || new_bytes.len() > MAX_TEXT_DIFF_BYTES {
        return Some(format!(
            "Binary files a/{path} and b/{path} differ ({} -> {} bytes; text diff omitted)",
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

/// Result of diffing two files
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    pub lines: Vec<DiffLine>,
    pub has_changes: bool,
}

impl FileDiff {
    /// Create a diff between two text contents
    pub fn new(path: &str, old: &str, new: &str) -> Self {
        let diff = TextDiff::from_lines(old, new);
        let mut lines = Vec::new();
        let mut has_changes = false;

        for change in diff.iter_all_changes() {
            let line = change.value().trim_end_matches('\n').to_string();
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
            // Output hunk header
            output.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            ));

            // Output hunk lines
            for line in &hunk.lines {
                match line {
                    DiffLine::Context(s) => output.push_str(&format!(" {s}\n")),
                    DiffLine::Added(s) => output.push_str(&format!("+{s}\n")),
                    DiffLine::Removed(s) => output.push_str(&format!("-{s}\n")),
                }
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

        Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
            lines,
        }
    }
}

/// A hunk in a unified diff
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<DiffLine>,
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
