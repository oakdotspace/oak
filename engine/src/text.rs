use std::collections::BTreeMap;

use oak_core::diff::{DiffLine, FileDiff, DEFAULT_CONTEXT_LINES};
use oak_core::merge::{three_way_merge_text, TextMerge};
use serde_json::{json, Value};

use crate::args::Args;
use crate::error::EngineError;

/// `diff.text`: hunks between two texts, using the same diff as `oak diff`.
pub fn diff_text(args: &Args) -> Result<Value, EngineError> {
    let old = args.str("old")?;
    let new = args.str("new")?;
    let context = args
        .opt_u64("context")?
        .map_or(DEFAULT_CONTEXT_LINES, |n| n as usize);
    let diff = FileDiff::new(args.opt_str("path")?.unwrap_or("file"), old, new);
    let hunks: Vec<Value> = diff
        .hunks(context)
        .iter()
        .map(|hunk| {
            let (mut old_line, mut new_line) = (hunk.old_start, hunk.new_start);
            let lines: Vec<Value> = hunk
                .lines
                .iter()
                .map(|line| {
                    let (kind, text, old_no, new_no) = match line {
                        DiffLine::Context(text) => {
                            let at = (Some(old_line), Some(new_line));
                            old_line += 1;
                            new_line += 1;
                            ("context", text, at.0, at.1)
                        }
                        DiffLine::Added(text) => {
                            new_line += 1;
                            ("added", text, None, Some(new_line - 1))
                        }
                        DiffLine::Removed(text) => {
                            old_line += 1;
                            ("removed", text, Some(old_line - 1), None)
                        }
                    };
                    json!({
                        "kind": kind,
                        "text": text.trim_end_matches(['\n', '\r'].as_slice()),
                        "old_line": old_no,
                        "new_line": new_no,
                    })
                })
                .collect();
            json!({
                "old_start": hunk.old_start,
                "old_count": hunk.old_count,
                "new_start": hunk.new_start,
                "new_count": hunk.new_count,
                "header": hunk.header(),
                "lines": lines,
            })
        })
        .collect();
    Ok(json!({ "hunks": hunks }))
}

/// `merge.text3`: the same line merge `oak pull` uses. `text` is always the
/// whole file; when `clean` is false it carries diff3-style markers.
pub fn merge_text3(args: &Args) -> Result<Value, EngineError> {
    let merge = three_way_merge_text(args.str("base")?, args.str("ours")?, args.str("theirs")?);
    Ok(match merge {
        TextMerge::Clean(text) => json!({"clean": true, "text": text, "conflict_count": 0}),
        TextMerge::Conflicted(text) => {
            let conflicts = text.lines().filter(|l| l.starts_with("<<<<<<<")).count();
            json!({"clean": false, "text": text, "conflict_count": conflicts})
        }
    })
}

/// `narrative.append`: what `oak desc --append` would produce. The previous
/// text stays byte for byte; only the blank-line separator is inserted.
pub fn narrative_append(args: &Args) -> Result<Value, EngineError> {
    let message = args.str("message")?;
    if message.trim().is_empty() {
        return Err(EngineError::bad_request("`message` is empty"));
    }
    let text = match args.opt_str("existing")?.filter(|text| !text.is_empty()) {
        Some(existing) => format!("{existing}\n\n{message}"),
        None => message.to_string(),
    };
    Ok(json!({ "text": text }))
}

const STATUSES: [&str; 4] = ["added", "modified", "deleted", "renamed"];

#[derive(Default)]
struct Rollup {
    counts: [u64; 4],
    other: u64,
}

impl Rollup {
    fn add(&mut self, status: &str) {
        match STATUSES.iter().position(|known| *known == status) {
            Some(index) => self.counts[index] += 1,
            None => self.other += 1,
        }
    }
}

/// Count `path` against each directory above it, with `""` as the root.
fn add_path(rollups: &mut BTreeMap<String, Rollup>, path: &str, status: &str) {
    rollups.entry(String::new()).or_default().add(status);
    let mut end = 0;
    while let Some(slash) = path[end..].find('/') {
        end += slash;
        rollups
            .entry(path[..end].to_string())
            .or_default()
            .add(status);
        end += 1;
    }
}

/// `status.rollup`: per-directory counts for folder badges, from the `changes`
/// of a parsed `status`. A rename counts in the old and the new directory, so
/// a folder that lost a file still shows a badge.
pub fn status_rollup(args: &Args) -> Result<Value, EngineError> {
    let mut rollups: BTreeMap<String, Rollup> = BTreeMap::new();
    for change in args.array("changes")? {
        let change = Args::new(change)?;
        let path = change.str("path")?.replace('\\', "/");
        let status = change.opt_str("status")?.unwrap_or("modified");
        add_path(&mut rollups, &path, status);
        if let Some(old_path) = change.opt_str("old_path")? {
            let old_path = old_path.replace('\\', "/");
            if old_path.rsplit_once('/').map(|p| p.0) != path.rsplit_once('/').map(|p| p.0) {
                add_path(&mut rollups, &old_path, status);
            }
        }
    }
    let directories: Vec<Value> = rollups
        .iter()
        .map(|(path, rollup)| {
            let [added, modified, deleted, renamed] = rollup.counts;
            let kinds: Vec<&str> = STATUSES
                .iter()
                .zip(rollup.counts)
                .filter(|(_, count)| *count > 0)
                .map(|(status, _)| *status)
                .collect();
            let status = match kinds.as_slice() {
                [only] if rollup.other == 0 => only,
                _ => "mixed",
            };
            json!({
                "path": path,
                "added": added,
                "modified": modified,
                "deleted": deleted,
                "renamed": renamed,
                "total": rollup.counts.iter().sum::<u64>() + rollup.other,
                "status": status,
            })
        })
        .collect();
    Ok(json!({ "directories": directories }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with<T>(args: Value, run: impl Fn(&Args) -> T) -> T {
        run(&Args::new(&args).unwrap())
    }

    #[test]
    fn diff_numbers_every_line() {
        let result = with(
            json!({"old": "a\nb\nc\n", "new": "a\nB\nc\nd\n", "context": 1}),
            diff_text,
        )
        .unwrap();
        let hunks = result["hunks"].as_array().unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0]["header"], "@@ -1,3 +1,4 @@");
        let lines = hunks[0]["lines"].as_array().unwrap();
        assert_eq!(
            lines[1],
            json!({"kind": "removed", "text": "b", "old_line": 2, "new_line": null})
        );
        assert_eq!(
            lines[2],
            json!({"kind": "added", "text": "B", "old_line": null, "new_line": 2})
        );
        assert_eq!(
            lines[3],
            json!({"kind": "context", "text": "c", "old_line": 3, "new_line": 3})
        );
        assert_eq!(lines[4]["new_line"], 4);

        let same = with(json!({"old": "x", "new": "x"}), diff_text).unwrap();
        assert_eq!(same["hunks"], json!([]));
    }

    #[test]
    fn merge_clean_and_conflicted() {
        let clean = with(
            json!({"base": "a\nb\nc\n", "ours": "A\nb\nc\n", "theirs": "a\nb\nC\n"}),
            merge_text3,
        )
        .unwrap();
        assert_eq!(
            clean,
            json!({"clean": true, "text": "A\nb\nC\n", "conflict_count": 0})
        );

        let conflict = with(
            json!({"base": "a\n", "ours": "b\n", "theirs": "c\n"}),
            merge_text3,
        )
        .unwrap();
        assert_eq!(conflict["clean"], false);
        assert_eq!(conflict["conflict_count"], 1);
        assert!(conflict["text"].as_str().unwrap().contains("<<<<<<<"));
    }

    #[test]
    fn narrative_matches_desc_append() {
        let text = |args| with(args, narrative_append).unwrap()["text"].clone();
        assert_eq!(text(json!({"existing": "a", "message": "b"})), "a\n\nb");
        assert_eq!(text(json!({"existing": "", "message": "b"})), "b");
        assert_eq!(text(json!({"existing": null, "message": "b\n"})), "b\n");
        assert_eq!(text(json!({"existing": "a\n", "message": "b"})), "a\n\n\nb");
        assert!(with(json!({"message": "  "}), narrative_append).is_err());
    }

    #[test]
    fn rollup_counts_every_ancestor() {
        let result = with(
            json!({"changes": [
                {"path": "Assets/Art/a.png", "status": "added"},
                {"path": "Assets/Art/b.png", "status": "added"},
                {"path": "Assets/c.cs", "status": "modified"},
                {"path": "New/d.png", "status": "renamed", "old_path": "Old/d.png"},
                {"path": "Same/e.png", "status": "renamed", "old_path": "Same/f.png"},
                {"path": "g.txt", "status": "typechange"},
                {"path": "h\\i.txt"},
            ]}),
            status_rollup,
        )
        .unwrap();
        let directories = result["directories"].as_array().unwrap();
        let find = |path: &str| directories.iter().find(|d| d["path"] == path).unwrap();
        assert_eq!(find("Assets/Art")["status"], "added");
        assert_eq!(find("Assets/Art")["total"], 2);
        assert_eq!(find("Assets")["status"], "mixed");
        assert_eq!(find("Assets")["total"], 3);
        assert_eq!(find("Old")["renamed"], 1);
        assert_eq!(find("New")["renamed"], 1);
        assert_eq!(find("Same")["renamed"], 1);
        assert_eq!(find("h")["modified"], 1);
        assert_eq!(find("")["status"], "mixed");
        // `Old/d.png` adds a second count at the root.
        assert_eq!(find("")["total"], 8);
    }

    #[test]
    fn rollup_needs_a_path() {
        assert!(with(json!({"changes": [{}]}), status_rollup).is_err());
        assert!(with(json!({}), status_rollup).is_err());
    }
}
