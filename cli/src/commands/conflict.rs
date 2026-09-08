use std::fs;
use std::path::Path;

use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};

use crate::output;

const SCHEMA_VERSION: u32 = crate::work_state::SCHEMA_VERSION;
const OURS_MARKER: &str = crate::commands::merge::OURS_MARKER;
const SEPARATOR_MARKER: &str = "=======";
const THEIRS_MARKER: &str = crate::commands::merge::THEIRS_MARKER;

#[derive(Debug, Clone, Copy)]
pub enum TakeSide {
    Ours,
    Theirs,
}

impl TakeSide {
    fn as_str(self) -> &'static str {
        match self {
            TakeSide::Ours => "ours",
            TakeSide::Theirs => "theirs",
        }
    }
}

#[derive(Debug, Serialize)]
struct ConflictStatusJson {
    schema_version: u32,
    context: String,
    in_progress: bool,
    kind: Option<String>,
    conflict_paths: Vec<String>,
    recommended_next_commands: Vec<String>,
    state: ConflictStateFactsJson,
}

#[derive(Debug, Serialize)]
struct ConflictShowJson {
    schema_version: u32,
    context: String,
    in_progress: bool,
    kind: Option<String>,
    conflict_paths: Vec<String>,
    conflicts: Vec<ConflictPathJson>,
    recommended_next_commands: Vec<String>,
    state: ConflictStateFactsJson,
}

#[derive(Debug, Serialize, Clone)]
struct ConflictPathJson {
    path: String,
    recorded: bool,
    exists: bool,
    has_conflict_markers: bool,
    resolution_state: String,
    can_take: bool,
}

#[derive(Debug, Serialize)]
struct ConflictTakeJson {
    schema_version: u32,
    context: String,
    path: String,
    side: String,
    remaining_conflict_count: usize,
    remaining_conflict_paths: Vec<String>,
    recommended_next_commands: Vec<String>,
}

#[derive(Debug, Default, Serialize, Clone)]
struct ConflictStateFactsJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_head: Option<MergeHeadJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_head: Option<SyncHeadJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_state: Option<SyncStateFactsJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mount: Option<MountConflictContextJson>,
}

#[derive(Debug, Serialize, Clone)]
struct MergeHeadJson {
    parent_branch: String,
    branch: String,
}

#[derive(Debug, Serialize, Clone)]
struct SyncHeadJson {
    parent_branch: String,
    branch: String,
    parent_head: Option<String>,
    reseed_old_tip: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CheckoutSyncState {
    merged_manifest_hash: String,
    conflict_paths: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
struct SyncStateFactsJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    merged_manifest_hash: Option<String>,
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[derive(Debug, Serialize, Clone)]
struct MountConflictContextJson {
    id: String,
    mount_point: String,
    repo: String,
    remote_url: String,
    virtual_branch: String,
    base_branch: String,
    base_commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    merged_manifest_hash: Option<String>,
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[derive(Debug, Serialize, Clone)]
struct MountConflictContextJson {}

pub fn status_checkout(path: &Path) -> Result<()> {
    let snapshot = checkout_snapshot(path)?;
    print_status_json(&snapshot, "checkout")
}

pub fn status_checkout_human(path: &Path) -> Result<()> {
    let snapshot = checkout_snapshot(path)?;
    print_status_human(&snapshot, "checkout");
    Ok(())
}

pub fn show_checkout(path: &Path) -> Result<()> {
    let snapshot = checkout_snapshot(path)?;
    print_show_json(&snapshot, "checkout")
}

pub fn show_checkout_human(path: &Path) -> Result<()> {
    let snapshot = checkout_snapshot(path)?;
    print_show_human(&snapshot, "checkout");
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub fn status_mount(dest: &Path) -> Result<()> {
    let snapshot = mount_snapshot(dest)?;
    print_status_json(&snapshot, "mount")
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub fn status_mount_human(dest: &Path) -> Result<()> {
    let snapshot = mount_snapshot(dest)?;
    print_status_human(&snapshot, "mount");
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub fn show_mount(dest: &Path) -> Result<()> {
    let snapshot = mount_snapshot(dest)?;
    print_show_json(&snapshot, "mount")
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub fn show_mount_human(dest: &Path) -> Result<()> {
    let snapshot = mount_snapshot(dest)?;
    print_show_human(&snapshot, "mount");
    Ok(())
}

pub fn take_checkout(cwd: &Path, input_path: &Path, side: TakeSide) -> Result<()> {
    let result = take_checkout_inner(cwd, input_path, side)?;
    output::success(&format!(
        "Resolved '{}' by taking {}.",
        result.path,
        side.as_str()
    ));
    if result.remaining_conflict_count > 0 {
        output::info("Run `oak conflict show --json` to inspect remaining conflicts.");
    } else if let Some(next) = result.recommended_next_commands.first() {
        output::info(&format!("Run `{next}` to continue."));
    }
    Ok(())
}

pub fn take_checkout_json(cwd: &Path, input_path: &Path, side: TakeSide) -> Result<()> {
    output::print_json(&take_checkout_inner(cwd, input_path, side)?)
}

fn take_checkout_inner(cwd: &Path, input_path: &Path, side: TakeSide) -> Result<ConflictTakeJson> {
    let snapshot = checkout_snapshot(cwd)?;
    if !snapshot.in_progress {
        return Err(OakError::MergeFailed(
            "no checkout conflict is in progress; run `oak conflict status --json` to inspect state"
                .to_string(),
        ));
    }

    let ctx = crate::resolve::resolve(cwd)?;
    let rel = crate::pathutil::repo_relative(cwd, &ctx.work_tree, input_path)
        .to_string_lossy()
        .replace('\\', "/");
    if rel.starts_with("../") || rel == ".." || Path::new(&rel).is_absolute() {
        return Err(OakError::InvalidPath(format!(
            "path '{}' is not inside this checkout",
            input_path.display()
        )));
    }
    if !snapshot.conflict_paths.iter().any(|p| p == &rel) {
        return Err(OakError::MergeFailed(format!(
            "'{rel}' is not recorded as part of the current conflict; run `oak conflict show --json`"
        )));
    }

    let file_path = ctx.work_tree.join(&rel);
    let content = fs::read_to_string(&file_path).map_err(|e| {
        OakError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot read conflicted file '{}': {e}", file_path.display()),
        ))
    })?;
    if !crate::commands::merge::content_has_conflict_markers(&content) {
        return Err(OakError::MergeFailed(format!(
            "'{rel}' no longer has conflict markers; run `oak pull --continue` or `oak merge --continue`"
        )));
    }

    let Some(resolved_content) = take_side_from_markers(&content, side) else {
        return Err(OakError::MergeFailed(format!(
            "'{rel}' has malformed conflict markers; edit it manually before continuing"
        )));
    };

    fs::write(&file_path, &resolved_content)?;
    warn_if_unbalanced_delimiters(&rel, &resolved_content);

    let remaining_snapshot = checkout_snapshot(cwd)?;
    let remaining_conflict_paths: Vec<String> = remaining_snapshot
        .conflicts
        .iter()
        .filter(|conflict| conflict.has_conflict_markers)
        .map(|conflict| conflict.path.clone())
        .collect();
    let recommended_next_commands = if remaining_conflict_paths.is_empty() {
        remaining_snapshot.next_commands
    } else {
        vec!["oak conflict show --json".to_string()]
    };
    Ok(ConflictTakeJson {
        schema_version: SCHEMA_VERSION,
        context: "checkout".to_string(),
        path: rel,
        side: side.as_str().to_string(),
        remaining_conflict_count: remaining_conflict_paths.len(),
        remaining_conflict_paths,
        recommended_next_commands,
    })
}

/// Sidecar state files live under `.oak/` in the work tree on mount backends;
/// fall back to the repo's oak_dir for plain checkouts.
fn state_file_path(ctx: &crate::resolve::RepoContext, name: &str) -> std::path::PathBuf {
    let in_tree = ctx.work_tree.join(".oak").join(name);
    if in_tree.exists() {
        in_tree
    } else {
        ctx.oak_dir.join(name)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub fn take_mount(_dest: &Path, path: &Path, _side: TakeSide) -> Result<()> {
    Err(OakError::MergeFailed(format!(
        "mount conflict take is not supported yet for '{}'; edit the mounted file manually, then run `oak pull --continue`",
        path.display()
    )))
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
pub fn take_mount_json(dest: &Path, path: &Path, side: TakeSide) -> Result<()> {
    take_mount(dest, path, side)
}

struct ConflictSnapshot {
    in_progress: bool,
    kind: Option<String>,
    conflict_paths: Vec<String>,
    conflicts: Vec<ConflictPathJson>,
    next_commands: Vec<String>,
    state: ConflictStateFactsJson,
}

fn checkout_snapshot(path: &Path) -> Result<ConflictSnapshot> {
    let ctx = crate::resolve::resolve(path)?;
    let merge_head_path = ctx.oak_dir.join("MERGE_HEAD");
    let sync_head_path = state_file_path(&ctx, "SYNC_HEAD");
    let sync_state_path = state_file_path(&ctx, "SYNC_STATE");

    let mut state = ConflictStateFactsJson::default();
    let mut kind = None;
    let mut conflict_paths = Vec::new();
    let mut next_commands = Vec::new();
    let mut recorded_conflict_paths = false;

    if merge_head_path.exists() {
        let merge_head = read_merge_head(&merge_head_path)?;
        state.merge_head = Some(merge_head);
        kind = Some("merge".to_string());
        let ignore = oak_core::IgnorePatterns::new(&ctx.work_tree)?;
        conflict_paths =
            crate::commands::merge::find_conflict_markers(&ctx.work_tree, &ctx.work_tree, &ignore)?;
        next_commands = vec![
            "oak merge --continue".to_string(),
            "oak merge --abort".to_string(),
        ];
    } else if sync_head_path.exists() {
        let sync_head = read_sync_head(&sync_head_path)?;
        state.sync_head = Some(sync_head);
        let sync_state = read_checkout_sync_state(&sync_state_path)?;
        if let Some(sync_state) = sync_state {
            conflict_paths = sync_state.conflict_paths.clone();
            recorded_conflict_paths = true;
            state.sync_state = Some(SyncStateFactsJson {
                merged_manifest_hash: Some(sync_state.merged_manifest_hash),
            });
        } else {
            let ignore = oak_core::IgnorePatterns::new(&ctx.work_tree)?;
            conflict_paths = crate::commands::merge::find_conflict_markers(
                &ctx.work_tree,
                &ctx.work_tree,
                &ignore,
            )?;
        }
        kind = Some("sync".to_string());
        next_commands = vec![
            "oak pull --continue".to_string(),
            "oak pull --abort".to_string(),
        ];
    }

    conflict_paths.sort();
    conflict_paths.dedup();
    let conflicts = conflict_facts(&ctx.work_tree, &conflict_paths, recorded_conflict_paths);

    Ok(ConflictSnapshot {
        in_progress: kind.is_some(),
        kind,
        conflict_paths,
        conflicts,
        next_commands,
        state,
    })
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn mount_snapshot(dest: &Path) -> Result<ConflictSnapshot> {
    use crate::commands::mount::state;

    let id = state::lookup_id_for(dest)?.ok_or_else(|| {
        OakError::Server(format!(
            "no mount registered for '{}'. Run `oak mount <organization>/<repo>` first.",
            dest.display()
        ))
    })?;
    let state_dir = state::state_dir_for(&id)?;
    let cfg = state::load_config(&state_dir)?;
    let sync = state::load_sync_state(&state_dir)?;
    let Some(sync) = sync else {
        return Ok(ConflictSnapshot {
            in_progress: false,
            kind: None,
            conflict_paths: Vec::new(),
            conflicts: Vec::new(),
            next_commands: Vec::new(),
            state: ConflictStateFactsJson {
                mount: Some(MountConflictContextJson {
                    id: cfg.id,
                    mount_point: cfg.mount_point.display().to_string(),
                    repo: format!("{}/{}", cfg.owner, cfg.repo),
                    remote_url: cfg.remote_url,
                    virtual_branch: cfg.virtual_branch,
                    base_branch: cfg.base_branch,
                    base_commit: cfg.base_commit,
                    parent_branch: None,
                    parent_head: None,
                    merged_manifest_hash: None,
                }),
                ..ConflictStateFactsJson::default()
            },
        });
    };

    let mut conflict_paths = sync.conflicts.clone();
    conflict_paths.sort();
    conflict_paths.dedup();
    let conflicts = conflict_facts(&cfg.mount_point, &conflict_paths, true);

    Ok(ConflictSnapshot {
        in_progress: true,
        kind: Some("mount_pull".to_string()),
        conflict_paths,
        conflicts,
        next_commands: vec![
            "oak pull --continue".to_string(),
            "oak pull --abort".to_string(),
        ],
        state: ConflictStateFactsJson {
            mount: Some(MountConflictContextJson {
                id: cfg.id,
                mount_point: cfg.mount_point.display().to_string(),
                repo: format!("{}/{}", cfg.owner, cfg.repo),
                remote_url: cfg.remote_url,
                virtual_branch: cfg.virtual_branch,
                base_branch: cfg.base_branch,
                base_commit: cfg.base_commit,
                parent_branch: Some(sync.parent_name),
                parent_head: Some(sync.parent_head),
                merged_manifest_hash: Some(sync.merged_manifest),
            }),
            ..ConflictStateFactsJson::default()
        },
    })
}

fn read_merge_head(path: &Path) -> Result<MergeHeadJson> {
    let raw = fs::read_to_string(path)?;
    let mut lines = raw.lines();
    let parent_branch = lines
        .next()
        .ok_or_else(|| OakError::MergeFailed("corrupt MERGE_HEAD".to_string()))?
        .to_string();
    let branch = lines
        .next()
        .ok_or_else(|| OakError::MergeFailed("corrupt MERGE_HEAD".to_string()))?
        .to_string();
    Ok(MergeHeadJson {
        parent_branch,
        branch,
    })
}

fn read_sync_head(path: &Path) -> Result<SyncHeadJson> {
    let raw = fs::read_to_string(path)?;
    let mut lines = raw.lines();
    let parent_branch = lines
        .next()
        .ok_or_else(|| OakError::MergeFailed("corrupt SYNC_HEAD".to_string()))?
        .to_string();
    let branch = lines
        .next()
        .ok_or_else(|| OakError::MergeFailed("corrupt SYNC_HEAD".to_string()))?
        .to_string();
    let parent_head = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    let reseed_old_tip = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    Ok(SyncHeadJson {
        parent_branch,
        branch,
        parent_head,
        reseed_old_tip,
    })
}

fn read_checkout_sync_state(path: &Path) -> Result<Option<CheckoutSyncState>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)?;
    serde_json::from_str(&raw).map(Some).map_err(|e| {
        OakError::Io(std::io::Error::other(format!(
            "invalid checkout sync state: {e}"
        )))
    })
}

fn conflict_facts(root: &Path, paths: &[String], recorded: bool) -> Vec<ConflictPathJson> {
    paths
        .iter()
        .map(|path| {
            let file_path = root.join(path);
            let content = fs::read_to_string(&file_path).ok();
            let exists = file_path.exists();
            let has_conflict_markers = content
                .as_deref()
                .is_some_and(crate::commands::merge::content_has_conflict_markers);
            ConflictPathJson {
                path: path.clone(),
                recorded,
                exists,
                has_conflict_markers,
                resolution_state: if has_conflict_markers {
                    "unresolved".to_string()
                } else if exists {
                    "resolved_or_binary".to_string()
                } else {
                    "missing".to_string()
                },
                can_take: has_conflict_markers,
            }
        })
        .collect()
}

fn take_side_from_markers(content: &str, side: TakeSide) -> Option<String> {
    take_side_from_markers_with_markers(content, side, OURS_MARKER, SEPARATOR_MARKER, THEIRS_MARKER)
}

fn take_side_from_markers_with_markers(
    content: &str,
    side: TakeSide,
    ours_marker: &str,
    separator_marker: &str,
    theirs_marker: &str,
) -> Option<String> {
    enum State {
        Normal,
        Ours,
        SkipOurs,
        Theirs,
        SkipTheirs,
    }

    let mut state = State::Normal;
    let mut out = String::with_capacity(content.len());
    let mut saw_conflict = false;
    let mut closed_block = false;

    for line in split_lines_keep_endings(content) {
        match (&state, side) {
            (State::Normal, _) if line.starts_with(ours_marker) => {
                saw_conflict = true;
                closed_block = false;
                state = match side {
                    TakeSide::Ours => State::Ours,
                    TakeSide::Theirs => State::SkipOurs,
                };
            }
            (State::Normal, _)
                if closed_block
                    && (line.starts_with(theirs_marker)
                        || is_separator_line(line, separator_marker)) =>
            {
                return None;
            }
            (State::Normal, _) => out.push_str(line),
            (State::Ours, TakeSide::Ours) if is_separator_line(line, separator_marker) => {
                state = State::SkipTheirs;
            }
            (State::Ours, TakeSide::Ours) => out.push_str(line),
            (State::SkipOurs, TakeSide::Theirs) if is_separator_line(line, separator_marker) => {
                state = State::Theirs;
            }
            (State::SkipOurs, TakeSide::Theirs) => {}
            (State::Theirs, TakeSide::Theirs) if line.starts_with(theirs_marker) => {
                closed_block = true;
                state = State::Normal;
            }
            (State::Theirs, TakeSide::Theirs) if is_separator_line(line, separator_marker) => {
                return None;
            }
            (State::Theirs, TakeSide::Theirs) => out.push_str(line),
            (State::SkipTheirs, TakeSide::Ours) if line.starts_with(theirs_marker) => {
                closed_block = true;
                state = State::Normal;
            }
            (State::SkipTheirs, TakeSide::Ours) if is_separator_line(line, separator_marker) => {
                return None;
            }
            (State::SkipTheirs, TakeSide::Ours) => {}
            _ => return None,
        }
    }

    match state {
        State::Normal if saw_conflict => Some(out),
        _ => None,
    }
}

fn is_separator_line(line: &str, separator_marker: &str) -> bool {
    line.trim_end_matches(['\r', '\n']) == separator_marker
}

fn split_lines_keep_endings(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = s.split_inclusive('\n').collect();
    let consumed: usize = lines.iter().map(|l| l.len()).sum();
    if consumed < s.len() {
        lines.push(&s[consumed..]);
    }
    lines
}

fn print_status_json(snapshot: &ConflictSnapshot, context: &str) -> Result<()> {
    output::print_json(&ConflictStatusJson {
        schema_version: SCHEMA_VERSION,
        context: context.to_string(),
        in_progress: snapshot.in_progress,
        kind: snapshot.kind.clone(),
        conflict_paths: snapshot.conflict_paths.clone(),
        recommended_next_commands: snapshot.next_commands.clone(),
        state: snapshot.state.clone(),
    })
}

fn print_show_json(snapshot: &ConflictSnapshot, context: &str) -> Result<()> {
    output::print_json(&ConflictShowJson {
        schema_version: SCHEMA_VERSION,
        context: context.to_string(),
        in_progress: snapshot.in_progress,
        kind: snapshot.kind.clone(),
        conflict_paths: snapshot.conflict_paths.clone(),
        conflicts: snapshot.conflicts.clone(),
        recommended_next_commands: snapshot.next_commands.clone(),
        state: snapshot.state.clone(),
    })
}

fn print_status_human(snapshot: &ConflictSnapshot, context: &str) {
    output::print_line(&format!("Context: {context}"));
    output::print_line(&format!(
        "In progress: {}",
        if snapshot.in_progress { "yes" } else { "no" }
    ));
    if let Some(kind) = snapshot.kind.as_deref() {
        output::print_line(&format!("Kind: {kind}"));
    }
    if snapshot.conflict_paths.is_empty() {
        output::print_line("Conflict paths: none");
    } else {
        output::print_line(&format!(
            "Conflict paths ({}):",
            snapshot.conflict_paths.len()
        ));
        for path in &snapshot.conflict_paths {
            output::print_line(&format!("  {path}"));
        }
    }
    if !snapshot.next_commands.is_empty() {
        output::print_line("Recommended:");
        for cmd in &snapshot.next_commands {
            output::print_line(&format!("  {cmd}"));
        }
    }
}

fn print_show_human(snapshot: &ConflictSnapshot, context: &str) {
    output::print_line(&format!("Context: {context}"));
    output::print_line(&format!(
        "In progress: {}",
        if snapshot.in_progress { "yes" } else { "no" }
    ));
    if let Some(kind) = snapshot.kind.as_deref() {
        output::print_line(&format!("Kind: {kind}"));
    }
    if snapshot.conflicts.is_empty() {
        output::print_line("Conflicts: none");
    } else {
        output::print_line(&format!("Conflicts ({}):", snapshot.conflicts.len()));
        for conflict in &snapshot.conflicts {
            output::print_line(&format!("  {}", conflict.path));
            output::print_line(&format!("    resolution: {}", conflict.resolution_state));
            output::print_line(&format!(
                "    conflict markers: {}",
                yes_no(conflict.has_conflict_markers)
            ));
            output::print_line(&format!("    can take: {}", yes_no(conflict.can_take)));
        }
    }
    if !snapshot.next_commands.is_empty() {
        output::print_line("Recommended:");
        for cmd in &snapshot.next_commands {
            output::print_line(&format!("  {cmd}"));
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn warn_if_unbalanced_delimiters(path: &str, content: &str) {
    let imbalances = unbalanced_delimiter_pairs(content);
    if imbalances.is_empty() {
        return;
    }
    let details: Vec<String> = imbalances
        .into_iter()
        .map(|(open, close, opens, closes)| {
            format!("'{open}{close}' ({opens} open, {closes} close)")
        })
        .collect();
    output::warning(&format!(
        "'{path}' may have unbalanced delimiters after take: {}",
        details.join(", ")
    ));
}

fn unbalanced_delimiter_pairs(content: &str) -> Vec<(char, char, usize, usize)> {
    const PAIRS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}')];
    PAIRS
        .iter()
        .filter_map(|&(open, close)| {
            let opens = content.chars().filter(|c| *c == open).count();
            let closes = content.chars().filter(|c| *c == close).count();
            (opens != closes).then_some((open, close, opens, closes))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{take_side_from_markers, unbalanced_delimiter_pairs, TakeSide};

    #[test]
    fn takes_ours_from_multiple_marker_blocks() {
        let input = "a\n<<<<<<< ours\nours1\n=======\ntheirs1\n>>>>>>> theirs\nb\n<<<<<<< ours\nours2\n=======\ntheirs2\n>>>>>>> theirs\n";
        assert_eq!(
            take_side_from_markers(input, TakeSide::Ours).unwrap(),
            "a\nours1\nb\nours2\n"
        );
    }

    #[test]
    fn takes_theirs_from_multiple_marker_blocks() {
        let input = "a\n<<<<<<< ours\nours1\n=======\ntheirs1\n>>>>>>> theirs\nb\n";
        assert_eq!(
            take_side_from_markers(input, TakeSide::Theirs).unwrap(),
            "a\ntheirs1\nb\n"
        );
    }

    #[test]
    fn rejects_ambiguous_theirs_marker_inside_theirs_content() {
        let input =
            "<<<<<<< ours\nours\n=======\n>>>>>>> weird content\nmore theirs\n>>>>>>> main\n";
        assert!(take_side_from_markers(input, TakeSide::Ours).is_none());
        assert!(take_side_from_markers(input, TakeSide::Theirs).is_none());
    }

    #[test]
    fn detects_unbalanced_delimiter_pairs() {
        assert_eq!(
            unbalanced_delimiter_pairs("fn open() {"),
            vec![('{', '}', 1, 0)]
        );
        assert_eq!(unbalanced_delimiter_pairs("(open"), vec![('(', ')', 1, 0)]);
        assert!(unbalanced_delimiter_pairs("balanced () and [] and {}").is_empty());
    }
}
