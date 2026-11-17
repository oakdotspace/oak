//! Structured, line-numbered hunks (fb-334).
//!
//! Every JSON `patch` and every printed diff block is produced by one
//! renderer (`Hunk::unified_lines` behind a `diff --oak` header), so line
//! numbers are derived from that rendered text rather than by re-diffing:
//! the numbers can never disagree with the patch they annotate, and they are
//! bounded by exactly the same `--max-bytes` budget (a file whose patch was
//! omitted gets no hunks either).

use serde::Serialize;

use super::FileSummaryJson;

/// One `@@ -a,b +c,d @@` hunk with every body line numbered.
///
/// `old_start`/`new_start` are the header's values (an empty side is
/// numbered by the line *after which* it sits, as `diff -u` does, so an added
/// file's old side is `0,0`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HunkJson {
    pub(crate) old_start: usize,
    pub(crate) old_len: usize,
    pub(crate) new_start: usize,
    pub(crate) new_len: usize,
    pub(crate) lines: Vec<HunkLineJson>,
}

/// One hunk body line. `old_line` is null for added lines and `new_line` is
/// null for removed lines; context lines carry both. `text` is the line
/// without its ` `/`+`/`-` prefix (a CRLF line keeps its `\r`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HunkLineJson {
    pub(crate) kind: &'static str,
    pub(crate) old_line: Option<usize>,
    pub(crate) new_line: Option<usize>,
    pub(crate) text: String,
    /// The `\ No newline at end of file` marker followed this line.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) no_newline_at_eof: bool,
}

/// Parse `@@ -a[,b] +c[,d] @@[ section]` into `(a, b, c, d)`.
fn parse_header(line: &str) -> Option<(usize, usize, usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |text: &str| -> Option<(usize, usize)> {
        match text.split_once(',') {
            Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
            None => Some((text.parse().ok()?, 1)),
        }
    };
    let (old_start, old_len) = range(old)?;
    let (new_start, new_len) = range(new)?;
    Some((old_start, old_len, new_start, new_len))
}

/// Line-number cursor for one hunk: the next old/new line to assign.
struct Cursor {
    old: usize,
    new: usize,
    old_left: usize,
    new_left: usize,
}

impl Cursor {
    fn new((old_start, old_len, new_start, new_len): (usize, usize, usize, usize)) -> Self {
        Self {
            old: old_start,
            new: new_start,
            old_left: old_len,
            new_left: new_len,
        }
    }

    fn open(&self) -> bool {
        self.old_left > 0 || self.new_left > 0
    }

    /// Number one body line, or `None` when it is not a body line of this
    /// hunk (the hunk is exhausted, or the prefix is not ` `/`+`/`-`).
    fn step(&mut self, line: &str) -> Option<(&'static str, Option<usize>, Option<usize>)> {
        let numbered = match line.as_bytes().first()? {
            b' ' if self.old_left > 0 && self.new_left > 0 => {
                self.old_left -= 1;
                self.new_left -= 1;
                ("context", Some(self.old), Some(self.new))
            }
            b'-' if self.old_left > 0 => {
                self.old_left -= 1;
                ("removed", Some(self.old), None)
            }
            b'+' if self.new_left > 0 => {
                self.new_left -= 1;
                ("added", None, Some(self.new))
            }
            _ => return None,
        };
        if numbered.1.is_some() {
            self.old += 1;
        }
        if numbered.2.is_some() {
            self.new += 1;
        }
        Some(numbered)
    }
}

/// Structured hunks for one rendered patch. A patch without `@@` hunks
/// (binary notice, pure rename, mode-only) yields an empty list.
pub(crate) fn structured_hunks(patch: &str) -> Vec<HunkJson> {
    let mut hunks: Vec<HunkJson> = Vec::new();
    let mut cursor: Option<Cursor> = None;
    for line in patch.split_terminator('\n') {
        if let Some(active) = cursor.as_mut() {
            if line == oak_core::diff::NO_NEWLINE_MARKER {
                if let Some(last) = hunks.last_mut().and_then(|hunk| hunk.lines.last_mut()) {
                    last.no_newline_at_eof = true;
                }
                continue;
            }
            if active.open() {
                if let Some((kind, old_line, new_line)) = active.step(line) {
                    let hunk = hunks.last_mut().expect("cursor implies an open hunk");
                    hunk.lines.push(HunkLineJson {
                        kind,
                        old_line,
                        new_line,
                        text: line[1..].to_string(),
                        no_newline_at_eof: false,
                    });
                    continue;
                }
            }
            cursor = None;
        }
        if let Some(header) = parse_header(line) {
            hunks.push(HunkJson {
                old_start: header.0,
                old_len: header.1,
                new_start: header.2,
                new_len: header.3,
                lines: Vec::new(),
            });
            cursor = Some(Cursor::new(header));
        }
    }
    hunks
}

/// `--line-numbers` for JSON: give every file that carries a `patch` its
/// structured `hunks`. Files without a patch (not requested, omitted by the
/// byte budget, missing content) stay without hunks — the same bound.
pub(super) fn attach_line_numbers(files: &mut [FileSummaryJson]) {
    for file in files {
        file.hunks = file.patch.as_deref().map(structured_hunks);
    }
}

/// Width of each number column in the text gutter.
const GUTTER_WIDTH: usize = 6;

/// `--print --line-numbers`: the gutter to prepend to each rendered line —
/// `"{old} {new} "` for hunk body lines (blank on the side a line does not
/// exist on), `None` for headers, markers and notices, which print as-is.
pub(crate) fn gutters<S: AsRef<str>>(lines: &[S]) -> Vec<Option<String>> {
    let mut out = Vec::with_capacity(lines.len());
    let mut cursor: Option<Cursor> = None;
    let column = |value: Option<usize>| match value {
        Some(n) => format!("{n:>GUTTER_WIDTH$}"),
        None => " ".repeat(GUTTER_WIDTH),
    };
    for line in lines {
        let line = line.as_ref();
        if let Some(active) = cursor.as_mut() {
            if line == oak_core::diff::NO_NEWLINE_MARKER {
                out.push(None);
                continue;
            }
            if active.open() {
                if let Some((_, old, new)) = active.step(line) {
                    out.push(Some(format!("{} {} ", column(old), column(new))));
                    continue;
                }
            }
            cursor = None;
        }
        if let Some(header) = parse_header(line) {
            cursor = Some(Cursor::new(header));
        }
        out.push(None);
    }
    out
}

/// Prefix each hunk body line of an uncoloured patch with its gutter.
pub(crate) fn number_patch_text(patch: &str) -> String {
    let lines: Vec<&str> = patch.split_terminator('\n').collect();
    let mut out = String::with_capacity(patch.len() + lines.len() * (2 * GUTTER_WIDTH + 2));
    for (line, gutter) in lines.iter().zip(gutters(&lines)) {
        if let Some(gutter) = gutter {
            out.push_str(&gutter);
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(old: &str, new: &str, context: usize) -> String {
        let diff = oak_core::FileDiff::new("f", old, new);
        let mut lines = vec![
            "diff --oak a/f b/f".to_string(),
            "--- a/f".to_string(),
            "+++ b/f".to_string(),
        ];
        for hunk in diff.hunks(context) {
            lines.extend(hunk.unified_lines());
        }
        let mut patch = lines.join("\n");
        patch.push('\n');
        patch
    }

    /// Independent oracle: every numbered line's text equals that line of
    /// the pre-/post-image, as `sed -n '<n>p'` would print it.
    fn assert_matches_images(patch: &str, old: &str, new: &str) {
        let old_lines: Vec<&str> = old.split_terminator('\n').collect();
        let new_lines: Vec<&str> = new.split_terminator('\n').collect();
        let hunks = structured_hunks(patch);
        assert!(!hunks.is_empty());
        for hunk in &hunks {
            let olds = hunk.lines.iter().filter(|l| l.old_line.is_some()).count();
            let news = hunk.lines.iter().filter(|l| l.new_line.is_some()).count();
            assert_eq!((olds, news), (hunk.old_len, hunk.new_len), "{hunk:?}");
            for line in &hunk.lines {
                if let Some(n) = line.old_line {
                    assert_eq!(old_lines[n - 1], line.text, "old {n}");
                }
                if let Some(n) = line.new_line {
                    assert_eq!(new_lines[n - 1], line.text, "new {n}");
                }
            }
        }
    }

    #[test]
    fn numbers_match_pre_and_post_images_across_hunks_and_context_widths() {
        let old: String = (1..=40).map(|n| format!("line {n}\n")).collect();
        let new = old
            .replace("line 3\n", "LINE three\n")
            .replace("line 20\n", "")
            .replace("line 35\n", "line 35\ninserted a\ninserted b\n");
        for context in [0, 1, 3, 10] {
            let patch = render(&old, &new, context);
            assert_matches_images(&patch, &old, &new);
        }
        let hunks = structured_hunks(&render(&old, &new, 3));
        assert_eq!(hunks.len(), 3);
        let added: Vec<_> = hunks[2]
            .lines
            .iter()
            .filter(|l| l.kind == "added")
            .map(|l| (l.new_line, l.text.as_str()))
            .collect();
        assert_eq!(
            added,
            vec![(Some(35), "inserted a"), (Some(36), "inserted b")]
        );
    }

    #[test]
    fn added_deleted_and_content_that_looks_like_headers() {
        // A removed `--flag` renders as `---flag`; it is body, not a header.
        let old = "--flag\n++x\n@@ not a hunk\nkeep\n";
        let new = "keep\n+++y\n";
        assert_matches_images(&render(old, new, 3), old, new);

        let added = structured_hunks(&render("", "a\nb\n", 3));
        assert_eq!(
            (added[0].old_start, added[0].old_len, added[0].new_start),
            (0, 0, 1)
        );
        assert!(added[0].lines.iter().all(|l| l.old_line.is_none()));
        let deleted = structured_hunks(&render("a\nb\n", "", 3));
        assert_eq!((deleted[0].new_start, deleted[0].new_len), (0, 0));
        assert!(deleted[0].lines.iter().all(|l| l.new_line.is_none()));
    }

    #[test]
    fn no_newline_marker_flags_the_line_it_follows() {
        let hunks = structured_hunks(&render("a\nb", "a\nB", 3));
        let flagged: Vec<_> = hunks[0]
            .lines
            .iter()
            .filter(|l| l.no_newline_at_eof)
            .map(|l| (l.kind, l.text.as_str()))
            .collect();
        assert_eq!(flagged, vec![("removed", "b"), ("added", "B")]);
    }

    #[test]
    fn binary_notice_has_no_hunks_and_text_gutter_skips_headers() {
        assert!(structured_hunks("diff --oak a/x b/x\nBinary files differ\n").is_empty());
        let numbered = number_patch_text(&render("a\nb\nc\n", "a\nB\nc\n", 3));
        let lines: Vec<&str> = numbered.lines().collect();
        assert_eq!(lines[0], "diff --oak a/f b/f");
        assert_eq!(lines[3], "@@ -1,3 +1,3 @@");
        assert_eq!(lines[4], "     1      1  a");
        assert_eq!(lines[5], "     2        -b");
        assert_eq!(lines[6], "            2 +B");
        assert_eq!(lines[7], "     3      3  c");
    }
}
