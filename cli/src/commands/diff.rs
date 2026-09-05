use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use oak_core::Repository;
use oak_core::{
    ChangeType, DiffLine, FileChange, FileDiff, FileMode, Hash, ManifestEntry, OakError, Result,
};

use crate::output;

/// Show diff between working directory and HEAD.
///
/// The set of paths shown here is computed by the same scan/diff pipeline that
/// `oak status` uses and `oak commit` records with. `oak diff` takes the
/// non-persisting path because hunk rendering reads new bytes directly from the
/// worktree; it only needs committed pre-image blobs from the object store.
/// Status, diff, and commit must still agree on *which* files changed. In
/// particular, diff must never silently omit a path just because it can't render
/// a textual hunk for it (binary files, a trailing-newline-only change, a pure
/// mode change): it emits a "differ" notice instead. (It used to gate every file
/// on `FileDiff::has_changes`, which dropped binary edits that status/commit
/// still counted.)
///
/// With `branch: true`, the pre-image is the current branch's fork point on
/// its parent instead of HEAD, so the diff covers the branch's whole
/// contribution — its commits plus the dirty working tree.
/// Diff the working tree (or, with `branch`, the whole branch contribution)
/// against its base and render with the requested output style. Library
/// surface used by integration tests; the CLI goes through [`dispatch`].
pub fn run(
    path: &Path,
    paths: &[std::path::PathBuf],
    stat: bool,
    name_only: bool,
    branch: bool,
) -> Result<bool> {
    let ctx = crate::resolve::resolve(path)?;
    let work_tree = ctx.work_tree.clone();
    let repo = ctx.open()?;

    let filters = resolve_filters(path, &work_tree, paths)?;
    let source = collect_source(repo.as_ref(), &work_tree, branch)?;
    present(
        repo.as_ref(),
        &work_tree,
        source,
        &filters,
        stat,
        name_only,
        !stat && !name_only,
        RenderOptions::default(),
    )
}

/// Render an already-collected [`DiffSource`] through one of the output
/// styles. The single presentation seam for every diff form — working tree,
/// `--branch`, and revision endpoints — so they can never drift apart:
///
/// - `name_only` → bare paths;
/// - `stat` → per-file counts;
/// - `print` → colorized hunks (paged on a TTY);
/// - otherwise → the interactive browser on a TTY, or the stat summary
///   plus a self-describing footer when piped (signal first for scripts
///   and agents; the footer names the exact commands that fetch more).
#[allow(clippy::too_many_arguments)] // internal seam; every call site is in this file
fn present(
    repo: &dyn Repository,
    work_tree: &Path,
    source: DiffSource,
    filters: &[String],
    stat: bool,
    name_only: bool,
    print: bool,
    options: RenderOptions,
) -> Result<bool> {
    let changes = filter_changes(source.changes, filters);
    // Predicted conflicts obey the user's path filters like any change row,
    // and every output style must surface them — a conflict that only shows
    // in one view is a conflict that gets missed.
    let conflicts: Vec<&String> = source
        .conflict_paths
        .iter()
        .filter(|path| filters.is_empty() || path_matches(path, filters))
        .collect();
    if changes.is_empty() && conflicts.is_empty() {
        output::info(no_differences_message(filters));
        return Ok(false);
    }

    if name_only {
        for line in render_name_only(&changes) {
            output::print_line(&line);
        }
        for path in &conflicts {
            output::print_line(path);
        }
        return Ok(true);
    }

    let base_files = manifest_file_map(&source.base_manifest);

    // `OAK_DIFF_TOOL` replaces the built-in browser with an external tool
    // over two materialized trees (old, new), like `git difftool -d`. It
    // outranks the non-TTY stat fallback: setting it is an explicit ask.
    if !print && !stat {
        if let Ok(tool) = std::env::var("OAK_DIFF_TOOL") {
            if !tool.trim().is_empty() {
                run_external_difftool(
                    &tool,
                    repo,
                    work_tree,
                    &base_files,
                    &changes,
                    &source.worktree_blobs,
                )?;
                // Predicted-conflict paths are excluded from the change set
                // the tool saw; surface them here so the external-tool style
                // honors the every-output-shows-conflicts invariant above.
                for path in &conflicts {
                    output::print_line(&predicted_conflict_line(path));
                }
                return Ok(true);
            }
        }
    }

    if stat || (!print && !io::stdout().is_terminal()) {
        let activity = output::activity("Computing diff statistics...");
        let rows = render_stat_with_worktree_blobs(
            repo,
            work_tree,
            &base_files,
            &changes,
            &source.worktree_blobs,
        )?;
        activity.finish_and_clear();
        for line in rows {
            output::print_line(&line);
        }
        for path in &conflicts {
            output::print_line(&format!("! {path}  conflict predicted"));
        }
        if !stat {
            // Non-TTY fallback is a stat summary by design; tell the caller
            // exactly how to get more instead of making it guess flags.
            output::print_line(&format!(
                "# hunks: {stub} --print · structured: {stub} --json",
                stub = source.command_stub
            ));
        }
        return Ok(true);
    }

    if print {
        print_changes(
            repo,
            work_tree,
            &base_files,
            &changes,
            &source.worktree_blobs,
            options,
        )?;
        for path in &conflicts {
            output::print_line(&predicted_conflict_line(path));
        }
        return Ok(true);
    }

    let mut entries = build_entries_with_worktree_blobs(
        repo,
        work_tree,
        &base_files,
        &changes,
        &source.worktree_blobs,
        options,
    )?;
    // Predicted-conflict paths are excluded from a net-merge change set;
    // give them tree entries anyway so the browser can mark them instead of
    // hiding them.
    for path in &conflicts {
        let path = (*path).clone();
        entries.push(FileEntry {
            path: path.clone(),
            display_path: path.clone(),
            change_type: ChangeType::Modified,
            lines: vec![
                format!("⚠ {path}: the merge prediction says this file would conflict."),
                "It is excluded from the net-merge diff; run `oak merge` to resolve it for real."
                    .to_string(),
            ],
            emphasis: vec![Vec::new(), Vec::new()],
            category: None,
            conflict: true,
        });
    }
    run_tui_entries(entries, &source.scope_label)?;
    Ok(true)
}

/// The one-line predicted-conflict marker shared by the `--print` and
/// external-tool output styles, so no style words the invariant differently.
fn predicted_conflict_line(path: &str) -> String {
    format!(
        "⚠ predicted conflict: {path} (excluded from the net-merge diff; run `oak merge` to resolve)"
    )
}

/// Materialize the old and new sides of a change set into two temp trees
/// and hand them to `$OAK_DIFF_TOOL <old> <new>`. The tool line is split on
/// whitespace.
///
/// The temp trees are deleted when the tool **exits**, so the tool must
/// block until the user is done — like `git difftool`. GUI launchers that
/// hand off to a running instance and return immediately need their wait
/// flag: `OAK_DIFF_TOOL="code --wait --diff"`, not `code --diff`.
fn run_external_difftool(
    tool: &str,
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    changes: &[FileChange],
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let old_root = temp.path().join("old");
    let new_root = temp.path().join("new");
    fs::create_dir_all(&old_root)?;
    fs::create_dir_all(&new_root)?;

    let write_side = |root: &Path, rel: &str, bytes: &[u8]| -> Result<()> {
        let dest = root.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(dest, bytes)?;
        Ok(())
    };

    for change in changes {
        let (old_bytes, new_bytes) =
            change_bytes(repo, work_tree, head_files, change, worktree_blobs)?;
        let old_path = change.old_path.as_deref().unwrap_or(&change.path);
        match change.change_type {
            ChangeType::Added => write_side(&new_root, &change.path, &new_bytes)?,
            ChangeType::Deleted => write_side(&old_root, old_path, &old_bytes)?,
            ChangeType::Modified | ChangeType::Renamed => {
                write_side(&old_root, old_path, &old_bytes)?;
                write_side(&new_root, &change.path, &new_bytes)?;
            }
        }
    }

    let mut parts = tool.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| OakError::InvalidArgument("OAK_DIFF_TOOL is empty".to_string()))?;
    let status = Command::new(program)
        .args(parts)
        .arg(&old_root)
        .arg(&new_root)
        .status()
        .map_err(|err| {
            OakError::InvalidArgument(format!("OAK_DIFF_TOOL '{tool}' failed to launch: {err}"))
        })?;
    // Exit code 1 conventionally means "differences found" for diff-like
    // tools; only complain about real failures.
    if !status.success() && status.code() != Some(1) {
        output::warning(&format!("OAK_DIFF_TOOL exited with {status}"));
    }
    Ok(())
}

/// Everything `oak diff` (non-mount) can be asked to do, bundled so the
/// dispatcher has one signature. Built in `main.rs` straight from the CLI
/// flags.
pub struct DiffRequest {
    pub args: Vec<std::path::PathBuf>,
    /// `Some(n)` when a literal `--` appeared, with `n` positionals after
    /// it (always paths). The separator's presence also forces the leading
    /// arguments to be read as revisions.
    pub forced_path_count: Option<usize>,
    pub json: bool,
    pub json_options: super::review::DiffJsonOptions,
    pub print: bool,
    pub stat: bool,
    pub name_only: bool,
    pub branch: bool,
    pub against: Option<String>,
    pub mode: Option<String>,
    /// `-U/--unified` context lines (None = default 3).
    pub unified: Option<usize>,
    /// `--word-diff`: mark intra-line changes in printed hunks.
    pub word_diff: bool,
    /// `-a/--text`: render oversized text blobs instead of emitting the
    /// compact large-file notice. NUL-containing binary data still stays binary.
    pub force_text: bool,
}

/// Single entry point for `oak diff` outside a mount: resolves revision
/// endpoints, validates flag combinations, and routes to the JSON or human
/// renderers. Returns whether differences were found (for `--exit-code`).
pub fn dispatch(path: &Path, request: DiffRequest) -> Result<bool> {
    let ctx = crate::resolve::resolve(path)?;
    let work_tree = ctx.work_tree.clone();
    let repo = ctx.open()?;

    let (endpoints, paths) = split_endpoint_args(
        repo.as_ref(),
        path,
        &request.args,
        request.forced_path_count,
    )?;
    let mode = request
        .mode
        .as_deref()
        .map(super::review::DiffMode::parse)
        .transpose()?;

    if endpoints.is_empty() {
        if request.against.is_some() || mode.is_some() {
            return Err(OakError::InvalidArgument(
                "--against/--mode require a revision endpoint, e.g. `oak diff <branch> --mode tree`"
                    .to_string(),
            ));
        }
        if request.json {
            drop(repo);
            return super::review::worktree_diff_json(path, &paths, request.json_options);
        }
        let filters = resolve_filters(path, &work_tree, &paths)?;
        let source = collect_source(repo.as_ref(), &work_tree, request.branch)?;
        return present(
            repo.as_ref(),
            &work_tree,
            source,
            &filters,
            request.stat,
            request.name_only,
            request.print,
            render_options(&request),
        );
    }

    if request.branch {
        return Err(OakError::InvalidArgument(
            "--branch conflicts with revision endpoints; `oak diff <branch>` already diffs that branch".to_string(),
        ));
    }

    if request.json {
        drop(repo);
        return super::review::endpoint_diff_json(
            path,
            &endpoints,
            &paths,
            request.against.as_deref(),
            mode,
            request.json_options,
        );
    }

    let filters = resolve_filters(path, &work_tree, &paths)?;
    let source = endpoint_source(
        repo.as_ref(),
        &work_tree,
        &endpoints,
        request.against.as_deref(),
        mode,
    )?;
    present(
        repo.as_ref(),
        &work_tree,
        source,
        &filters,
        request.stat,
        request.name_only,
        request.print,
        render_options(&request),
    )
}

fn render_options(request: &DiffRequest) -> RenderOptions {
    RenderOptions {
        context: request.unified.unwrap_or(oak_core::DEFAULT_CONTEXT_LINES),
        word_diff: request.word_diff,
        force_text: request.force_text,
    }
}

/// Human-readable `oak branch diff <name>`: the endpoint presentation over
/// the branch-mode manifest pair — stat table when piped, interactive
/// browser on a TTY. The JSON twin is `review::branch_diff_json`; both
/// resolve through `review::branch_mode_manifests`, so they can never
/// disagree about what changed.
pub fn branch_diff_human(
    path: &Path,
    branch: &str,
    against: &str,
    mode: super::review::DiffMode,
    paths: &[std::path::PathBuf],
) -> Result<bool> {
    let ctx = crate::resolve::resolve(path)?;
    let work_tree = ctx.work_tree.clone();
    let repo = ctx.open()?;
    let filters = resolve_filters(path, &work_tree, paths)?;

    let data = super::review::branch_mode_manifests(repo.as_ref(), branch, against, mode)?;
    for caveat in &data.caveats {
        output::warning(caveat);
    }
    let changes = data.changes(repo.as_ref())?;
    let source = DiffSource {
        changes,
        base_manifest: Some(data.base),
        worktree_blobs: HashMap::new(),
        scope_label: format!("{branch} vs {against} ({})", mode.as_str()),
        // `oak branch diff` itself has no --print/--hunks surface; point the
        // footer at the `oak diff` endpoint spelling, where both exist.
        command_stub: format!(
            "oak diff {branch} --against {against} --mode {}",
            mode.as_str()
        ),
        conflict_paths: data.conflict_paths,
    };
    present(
        repo.as_ref(),
        &work_tree,
        source,
        &filters,
        false,
        false,
        false,
        RenderOptions::default(),
    )
}

/// How hunks are rendered: context width (`-U`) and word-level change
/// markers (`--word-diff`). The TUI ignores `word_diff` — it always shows
/// intra-line emphasis natively.
#[derive(Clone, Copy)]
pub(crate) struct RenderOptions {
    pub context: usize,
    pub word_diff: bool,
    pub force_text: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            context: oak_core::DEFAULT_CONTEXT_LINES,
            word_diff: false,
            force_text: false,
        }
    }
}

/// The inputs one diff invocation renders from: the change set, the pre-image
/// manifest those changes were computed against (HEAD or the branch fork
/// point), the ephemeral blobs hashed off the working tree, and a scope label
/// for the browser title.
struct DiffSource {
    changes: Vec<FileChange>,
    base_manifest: Option<oak_core::Manifest>,
    worktree_blobs: HashMap<Hash, Vec<u8>>,
    scope_label: String,
    /// The exact `oak diff …` invocation (minus output flags) that
    /// reproduces this diff — used in self-describing footers.
    command_stub: String,
    /// Net-merge endpoints: paths predicted to conflict (excluded from the
    /// change set; shown as ⚠ entries so they never silently disappear).
    conflict_paths: Vec<String>,
}

fn collect_source(repo: &dyn Repository, work_tree: &Path, branch: bool) -> Result<DiffSource> {
    if branch {
        return branch_source(repo, work_tree);
    }
    let (changes, head, _branch, worktree_blobs) =
        super::commit::compute_changes_for_status_with_ephemeral(repo, work_tree)?;
    // Before filtering, so a path-scoped diff still surfaces a mass drop of
    // tracked-but-now-ignored files elsewhere in the tree.
    super::commit::warn_tracked_now_ignored(
        &changes,
        work_tree,
        "will be removed from the branch by the next commit",
    );
    Ok(DiffSource {
        changes,
        base_manifest: head_manifest_for(repo, &head)?,
        worktree_blobs,
        scope_label: "working tree".to_string(),
        command_stub: "oak diff".to_string(),
        conflict_paths: Vec::new(),
    })
}

/// `--branch`: diff the branch's whole contribution — committed work plus the
/// dirty working tree — against its fork point on the parent branch.
fn branch_source(repo: &dyn Repository, work_tree: &Path) -> Result<DiffSource> {
    let branch = repo.get_current_branch_name()?.ok_or_else(|| {
        OakError::InvalidArgument("--branch requires an active branch".to_string())
    })?;
    let row = repo
        .get_branch(&branch)?
        .ok_or_else(|| OakError::BranchNotFound(branch.clone()))?;
    let Some(parent) = row.parent_branch else {
        return Err(OakError::InvalidArgument(format!(
            "branch '{branch}' has no parent branch to diff against; --branch shows a feature branch's changes since it forked"
        )));
    };
    let (base_manifest, caveats) =
        super::review::branch_fork_base_manifest(repo, &branch, &parent)?;
    for caveat in &caveats {
        output::warning(caveat);
    }
    let (changes, worktree_blobs) =
        super::commit::compute_worktree_changes_vs_manifest(repo, work_tree, &base_manifest)?;
    Ok(DiffSource {
        changes,
        base_manifest: Some(base_manifest),
        worktree_blobs,
        scope_label: format!("branch {branch} vs {parent}"),
        command_stub: "oak diff --branch".to_string(),
        conflict_paths: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Revision endpoints
//
// `oak diff` accepts up to two leading revision arguments — branch names or
// commit hashes (unique prefixes ≥ 4 hex chars) — before its path filters:
//
//   oak diff                     working tree vs HEAD
//   oak diff <branch>            branch contribution vs its fork point
//   oak diff <commit>            commit vs working tree
//   oak diff <rev> <rev>         tree diff between two revisions
//
// Everything after `--` is always a path. All of this is checkout-free: no
// endpoint form ever touches the working tree.
// ---------------------------------------------------------------------------

/// One side of an endpoint diff, as resolved from a CLI argument.
#[derive(Debug, Clone)]
pub(crate) enum Endpoint {
    Branch(String),
    Commit(Hash),
}

impl Endpoint {
    pub(crate) fn display(&self) -> String {
        match self {
            Endpoint::Branch(name) => name.clone(),
            Endpoint::Commit(hash) => hash.as_str().chars().take(12).collect(),
        }
    }

    /// The manifest this endpoint denotes (branch effective head, or the
    /// commit itself).
    fn manifest(&self, repo: &dyn Repository) -> Result<oak_core::Manifest> {
        match self {
            Endpoint::Branch(name) => {
                let head = super::commit::resolve_effective_head(repo, name)?;
                super::review::manifest_for_head(repo, head.as_ref())
            }
            Endpoint::Commit(hash) => super::review::manifest_for_head(repo, Some(hash)),
        }
    }
}

/// Try to resolve one CLI argument as a revision. `None` means "not a
/// revision" — the caller treats it as a path filter.
fn resolve_revision(repo: &dyn Repository, arg: &str) -> Result<Option<Endpoint>> {
    if repo.get_branch(arg)?.is_some() {
        return Ok(Some(Endpoint::Branch(arg.to_string())));
    }
    let hex_like = arg.len() >= 4 && arg.chars().all(|c| c.is_ascii_hexdigit());
    if hex_like {
        if let Ok(hash) = super::checkout::resolve_commit_ref(repo, arg) {
            return Ok(Some(Endpoint::Commit(hash)));
        }
    }
    // The default branch usually has no local branch row (Oak treats main
    // as server-owned), and a name recorded as some branch's parent is a
    // real branch even when it isn't local. Both are branch endpoints; the
    // comparison layer applies its existing head fallbacks and caveats.
    if super::review::is_rowless_branch_name(repo, arg)? {
        return Ok(Some(Endpoint::Branch(arg.to_string())));
    }
    Ok(None)
}

/// Split `oak diff`'s positional args into leading revision endpoints (at
/// most two) and trailing path filters.
///
/// `forced_path_count` is how many positionals appeared after a literal
/// `--` on the command line: those are always paths, never revisions. An
/// argument that is both a resolvable revision *and* an existing filesystem
/// path is ambiguous, and the error spells out both exact invocations —
/// the user should never have to guess the disambiguation rule.
pub(crate) fn split_endpoint_args(
    repo: &dyn Repository,
    cwd: &Path,
    args: &[std::path::PathBuf],
    forced_path_count: Option<usize>,
) -> Result<(Vec<Endpoint>, Vec<std::path::PathBuf>)> {
    // A typed `--` is itself the disambiguation: everything before it is a
    // revision, so the both-revision-and-path check must not fire — that
    // would make the error's own suggested escape hatch a dead end.
    let double_dash = forced_path_count.is_some();
    let candidate_count = args.len().saturating_sub(forced_path_count.unwrap_or(0));
    let mut endpoints = Vec::new();
    let mut paths = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        let text = arg.to_string_lossy();
        let still_leading = index < candidate_count && index == endpoints.len();
        let revision = if still_leading && endpoints.len() < 2 {
            resolve_revision(repo, &text)?
        } else {
            None
        };
        match revision {
            Some(endpoint) => {
                if !double_dash && cwd.join(arg).exists() {
                    let kind = match &endpoint {
                        Endpoint::Branch(_) => "a branch",
                        Endpoint::Commit(_) => "a commit",
                    };
                    return Err(OakError::InvalidArgument(format!(
                        "'{text}' is both {kind} and an existing path\n  treat it as a revision:  oak diff {text} --\n  diff just the path:      oak diff -- {text}"
                    )));
                }
                endpoints.push(endpoint);
            }
            None => {
                // An argument before `--` that resolves to nothing is far
                // more often a typo'd revision than a filter for a deleted
                // file; fail loudly (like git) and name the escape hatch.
                if index < candidate_count && !cwd.join(arg).exists() {
                    // Only leading positions are read as revisions. When the
                    // arg *would* resolve as one, "not a branch" would be a
                    // lie — name the real constraint instead.
                    if resolve_revision(repo, &text)?.is_some() {
                        if endpoints.len() >= 2 {
                            return Err(OakError::InvalidArgument(format!(
                                "oak diff accepts at most two revisions; '{}' and '{}' are already the two, so '{text}' cannot be a third",
                                endpoints[0].display(),
                                endpoints[1].display(),
                            )));
                        }
                        let first_path = paths
                            .first()
                            .map(|p: &std::path::PathBuf| p.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        return Err(OakError::InvalidArgument(format!(
                            "revisions must come before paths: '{text}' appears after path '{first_path}'\n  put the revision first: oak diff {text} -- {first_path}"
                        )));
                    }
                    return Err(OakError::InvalidArgument(format!(
                        "'{text}' is not a branch, commit, or existing path\n  for a path that no longer exists: oak diff -- {text}"
                    )));
                }
                paths.push(arg.clone());
            }
        }
    }
    Ok((endpoints, paths))
}

/// Build the [`DiffSource`] for resolved endpoints. This is where the
/// endpoint forms get their semantics:
///
/// - one branch: checkout-free branch diff, `--mode` picks the perspective
///   (default **contribution** — "what did this branch do"), `--against`
///   picks the base (default: the branch's parent).
/// - one commit: that commit vs the working tree (like `git diff <commit>`).
/// - two revisions: tree diff base → target; when both are branches,
///   `--mode` applies with the first revision as the base.
fn endpoint_source(
    repo: &dyn Repository,
    work_tree: &Path,
    endpoints: &[Endpoint],
    against: Option<&str>,
    mode: Option<super::review::DiffMode>,
) -> Result<DiffSource> {
    use super::review::DiffMode;
    match endpoints {
        [Endpoint::Branch(branch)] => {
            let against_name = match against {
                Some(name) => name.to_string(),
                None => repo
                    .get_branch(branch)?
                    .and_then(|row| row.parent_branch)
                    .unwrap_or_else(|| oak_core::DEFAULT_BRANCH.to_string()),
            };
            let mode = mode.unwrap_or(DiffMode::Contribution);
            let data = super::review::branch_mode_manifests(repo, branch, &against_name, mode)?;
            for caveat in &data.caveats {
                output::warning(caveat);
            }
            let changes = data.changes(repo)?;
            // Spell the canonical invocation with defaults resolved
            // (explicit --against and --mode), matching the JSON payload's
            // command stubs, so both surfaces name the same command.
            let command_stub =
                super::review::endpoint_command_stub(endpoints, Some(&against_name), Some(mode));
            Ok(DiffSource {
                changes,
                base_manifest: Some(data.base),
                worktree_blobs: HashMap::new(),
                scope_label: format!("{branch} vs {against_name} ({})", mode.as_str()),
                command_stub,
                conflict_paths: data.conflict_paths,
            })
        }
        [Endpoint::Commit(hash)] => {
            if against.is_some() || mode.is_some() {
                return Err(OakError::InvalidArgument(
                    "--against/--mode apply to branch endpoints; `oak diff <commit>` always compares the commit to the working tree".to_string(),
                ));
            }
            let base_manifest = super::review::manifest_for_head(repo, Some(hash))?;
            let (changes, worktree_blobs) = super::commit::compute_worktree_changes_vs_manifest(
                repo,
                work_tree,
                &base_manifest,
            )?;
            Ok(DiffSource {
                changes,
                base_manifest: Some(base_manifest),
                worktree_blobs,
                scope_label: format!(
                    "{} vs working tree",
                    Endpoint::Commit(hash.clone()).display()
                ),
                command_stub: super::review::endpoint_command_stub(endpoints, None, None),
                conflict_paths: Vec::new(),
            })
        }
        [base, target] => {
            if against.is_some() {
                return Err(OakError::InvalidArgument(
                    "--against conflicts with two explicit revisions; the first revision is the base".to_string(),
                ));
            }
            if let (Endpoint::Branch(base_branch), Endpoint::Branch(target_branch)) = (base, target)
            {
                let mode = mode.unwrap_or(DiffMode::Tree);
                let data =
                    super::review::branch_mode_manifests(repo, target_branch, base_branch, mode)?;
                for caveat in &data.caveats {
                    output::warning(caveat);
                }
                let changes = data.changes(repo)?;
                return Ok(DiffSource {
                    changes,
                    base_manifest: Some(data.base),
                    worktree_blobs: HashMap::new(),
                    scope_label: format!("{base_branch} vs {target_branch} ({})", mode.as_str()),
                    command_stub: super::review::endpoint_command_stub(endpoints, None, Some(mode)),
                    conflict_paths: data.conflict_paths,
                });
            }
            if mode.is_some_and(|m| m != DiffMode::Tree) {
                return Err(OakError::InvalidArgument(
                    "--mode contribution/net-merge needs two branch endpoints; commit pairs always use a tree diff".to_string(),
                ));
            }
            let base_manifest = base.manifest(repo)?;
            let target_manifest = target.manifest(repo)?;
            let changes = super::review::diff_manifests_with_content_renames(
                repo,
                &base_manifest,
                &target_manifest,
            )?;
            Ok(DiffSource {
                changes,
                base_manifest: Some(base_manifest),
                worktree_blobs: HashMap::new(),
                scope_label: format!("{} vs {}", base.display(), target.display()),
                command_stub: super::review::endpoint_command_stub(endpoints, None, None),
                conflict_paths: Vec::new(),
            })
        }
        _ => Err(OakError::InvalidArgument(
            "oak diff accepts at most two revisions".to_string(),
        )),
    }
}

pub(crate) fn render_name_only(changes: &[FileChange]) -> Vec<String> {
    changes.iter().map(|change| change.path.clone()).collect()
}

/// The "nothing to show" line, qualified when the user scoped the diff so an
/// empty result is distinguishable from a clean tree.
fn no_differences_message(filters: &[String]) -> &'static str {
    if filters.is_empty() {
        "No differences"
    } else {
        "No differences in the given paths"
    }
}

/// Translate user-supplied CLI paths (absolute or cwd-relative) into
/// repo-root-relative forward-slash filter strings. Errors if a path lies
/// outside the repository.
pub(crate) fn resolve_filters(
    cwd: &Path,
    work_tree: &Path,
    paths: &[std::path::PathBuf],
) -> Result<Vec<String>> {
    paths
        .iter()
        .map(|p| crate::pathutil::repo_relative_str(cwd, work_tree, &p.to_string_lossy()))
        .collect()
}

/// Does `path` (repo-relative, forward slashes) match any filter — either
/// exactly, or as a file under a filter directory? An empty filter (the repo
/// root itself, e.g. `oak diff .`) matches everything.
pub(crate) fn path_matches(path: &str, filters: &[String]) -> bool {
    filters.iter().any(|f| {
        let f = f.trim_end_matches('/');
        f.is_empty() || path == f || (path.starts_with(f) && path.as_bytes()[f.len()] == b'/')
    })
}

/// Restrict a change set to the user's paths. With no filters the set passes
/// through untouched; renames match on either their old or new path so a
/// scoped diff never hides where a file went.
pub(crate) fn filter_changes(changes: Vec<FileChange>, filters: &[String]) -> Vec<FileChange> {
    if filters.is_empty() {
        return changes;
    }
    changes
        .into_iter()
        .filter(|c| {
            path_matches(&c.path, filters)
                || c.old_path
                    .as_deref()
                    .is_some_and(|p| path_matches(p, filters))
        })
        .collect()
}

/// Look up the full manifest at `head`, if any.
fn head_manifest_for(
    repo: &dyn Repository,
    head: &Option<Hash>,
) -> Result<Option<oak_core::Manifest>> {
    Ok(if let Some(head_hash) = head {
        repo.get_commit(head_hash)?
            .and_then(|c| repo.get_manifest(&c.manifest_hash).ok().flatten())
    } else {
        None
    })
}

/// Index a manifest's entries by path for pre-image lookups.
fn manifest_file_map(manifest: &Option<oak_core::Manifest>) -> HashMap<&str, &ManifestEntry> {
    manifest
        .as_ref()
        .map(|m| m.entries.iter().map(|e| (e.path.as_str(), e)).collect())
        .unwrap_or_default()
}

/// Render the `--stat` summary: one `<kind> <path>  +added -removed` row per
/// change and a totals line. Counts are derived from the same old/new bytes and
/// text-diff rules as the full diff, without building unified diff text first
/// (binary files show `bin` instead of counts).
pub(crate) fn render_stat(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    changes: &[FileChange],
) -> Result<Vec<String>> {
    render_stat_with_worktree_blobs(repo, work_tree, head_files, changes, &HashMap::new())
}

fn render_stat_with_worktree_blobs(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    changes: &[FileChange],
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
) -> Result<Vec<String>> {
    render_stat_with_limit(repo, work_tree, head_files, changes, None, worktree_blobs)
}

pub(crate) fn render_stat_with_limit(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    changes: &[FileChange],
    max_rows: Option<usize>,
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
) -> Result<Vec<String>> {
    let mut rows = Vec::with_capacity(changes.len() + 1);
    let (mut total_added, mut total_removed) = (0usize, 0usize);
    for (index, change) in changes.iter().enumerate() {
        let stat = stat_counts_for_change(repo, work_tree, head_files, change, worktree_blobs)?;
        let kind = match change.change_type {
            ChangeType::Added => "A",
            ChangeType::Modified => "M",
            ChangeType::Deleted => "D",
            ChangeType::Renamed => "R",
        };
        let label = match (change.change_type, &change.old_path) {
            (ChangeType::Renamed, Some(old)) => format!("{old} -> {}", change.path),
            _ => change.path.clone(),
        };
        let show_row = max_rows.is_none_or(|limit| index < limit);
        if stat.binary {
            if show_row {
                rows.push(format!("{kind} {label}  bin"));
            }
        } else {
            if show_row {
                rows.push(format!("{kind} {label}  +{} -{}", stat.added, stat.removed));
            }
            total_added += stat.added;
            total_removed += stat.removed;
        }
    }
    if let Some(limit) = max_rows {
        if changes.len() > limit {
            rows.push(format!("... {} more", changes.len() - limit));
        }
    }
    rows.push(format!(
        "{} file{} changed, +{total_added} -{total_removed}",
        changes.len(),
        if changes.len() == 1 { "" } else { "s" },
    ));
    Ok(rows)
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

fn print_changes(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    changes: &[FileChange],
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
    options: RenderOptions,
) -> Result<()> {
    if io::stdout().is_terminal() {
        let activity = output::activity("Rendering text diffs...");
        let mut formatted = String::new();
        for change in changes {
            formatted.push_str(&render_change_with_options(
                repo,
                work_tree,
                head_files,
                change,
                worktree_blobs,
                options,
            )?);
            formatted.push('\n');
        }
        activity.finish_and_clear();
        print_formatted(&formatted);
        return Ok(());
    }

    for change in changes {
        let block = render_change_with_options(
            repo,
            work_tree,
            head_files,
            change,
            worktree_blobs,
            options,
        )?;
        for line in block.lines() {
            output::print_line(line);
        }
        output::print_line("");
    }
    Ok(())
}

/// Compute the change set and render it to a formatted (colorized) diff string.
///
/// Shared by [`run`] and exercised directly by tests. The returned `changes`
/// are exactly what [`super::commit::compute_changes_for_status`] (and therefore
/// `oak status`) report, and `formatted` is guaranteed to contain a
/// `diff --oak a/<path> b/<path>` block for *every* one of them — diff never
/// drops a path in the set, so `oak status`, `oak diff`, and `oak commit` agree.
pub fn render(repo: &dyn Repository, work_tree: &Path) -> Result<(Vec<FileChange>, String)> {
    // Authoritative change set — identical to `oak status` and to what
    // `oak commit` will record (honors the active project/team scope, the
    // parent-branch head walk, and counts by blob hash rather than by whether a
    // textual diff happens to render).
    let (changes, head, _branch, worktree_blobs) =
        super::commit::compute_changes_for_status_with_ephemeral(repo, work_tree)?;

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
        formatted.push_str(&render_change_with_options(
            repo,
            work_tree,
            &head_files,
            change,
            &worktree_blobs,
            RenderOptions::default(),
        )?);
        formatted.push('\n');
    }
    Ok((changes, formatted))
}

/// Render one change into a formatted (colorized) diff block. Always produces
/// output — a file in the change set is never dropped, even when there's no
/// textual hunk to show.
///
/// This is the colorized wrapper over [`render_change_lines_with_options`]: it takes the raw
/// per-line block that function produces and runs each line through
/// [`output::format_diff_line`]. So `oak diff --print` and the interactive
/// browser render exactly the same lines, just one with ANSI codes and one with
/// ratatui styles.
pub(crate) fn render_change(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    change: &FileChange,
) -> Result<String> {
    render_change_with_options(
        repo,
        work_tree,
        head_files,
        change,
        &HashMap::new(),
        RenderOptions::default(),
    )
}

fn render_change_with_options(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    change: &FileChange,
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
    options: RenderOptions,
) -> Result<String> {
    let mut block = String::new();
    for line in render_change_lines_with_options(
        repo,
        work_tree,
        head_files,
        change,
        worktree_blobs,
        options,
    )? {
        block.push_str(&output::format_diff_line(&line));
        block.push('\n');
    }
    Ok(block)
}

struct StatCounts {
    added: usize,
    removed: usize,
    binary: bool,
}

fn stat_counts_for_change(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    change: &FileChange,
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
) -> Result<StatCounts> {
    let (old_bytes, new_bytes) = change_bytes(repo, work_tree, head_files, change, worktree_blobs)?;
    if oak_core::binary_or_oversize_notice(
        &change.path,
        &old_bytes,
        &new_bytes,
        Some(oak_core::MAX_TEXT_DIFF_BYTES),
    )
    .is_some()
    {
        return Ok(StatCounts {
            added: 0,
            removed: 0,
            binary: true,
        });
    }

    let old_str = String::from_utf8_lossy(&old_bytes);
    let new_str = String::from_utf8_lossy(&new_bytes);
    let diff = FileDiff::new(&change.path, &old_str, &new_str);
    let mut added = 0;
    let mut removed = 0;
    for line in &diff.lines {
        match line {
            DiffLine::Added(_) => added += 1,
            DiffLine::Removed(_) => removed += 1,
            DiffLine::Context(_) => {}
        }
    }

    Ok(StatCounts {
        added,
        removed,
        binary: false,
    })
}

/// `(old_bytes, new_bytes)` for a changed file — borrowed from the worktree
/// blob cache where possible, owned otherwise.
type DiffBytes<'a> = (Cow<'a, [u8]>, Cow<'a, [u8]>);

fn change_bytes<'a>(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    change: &FileChange,
    worktree_blobs: &'a HashMap<Hash, Vec<u8>>,
) -> Result<DiffBytes<'a>> {
    let path = &change.path;
    let current_bytes = || -> Result<Cow<'a, [u8]>> {
        if let Some(hash) = &change.new_blob_hash {
            if let Some(bytes) = worktree_blobs.get(hash) {
                return Ok(Cow::Borrowed(bytes.as_slice()));
            }
            // Committed post-image (endpoint diffs like `oak diff A B`):
            // the new side lives in the object store, not the worktree.
            // Worktree diffs never hit this — their ephemeral hashes are
            // not stored — so the fall-through below still serves them.
            if let Some(blob) = repo.get_blob(hash)? {
                return Ok(Cow::Owned(blob.content));
            }
        }
        Ok(Cow::Owned(read_worktree_bytes(work_tree, path)?))
    };
    match change.change_type {
        ChangeType::Added => Ok((Cow::Borrowed(&[]), current_bytes()?)),
        ChangeType::Deleted => Ok((
            Cow::Owned(old_bytes_for(
                repo,
                head_files,
                path,
                &change.old_blob_hash,
            )?),
            Cow::Borrowed(&[]),
        )),
        ChangeType::Modified => Ok((
            Cow::Owned(old_bytes_for(
                repo,
                head_files,
                path,
                &change.old_blob_hash,
            )?),
            current_bytes()?,
        )),
        ChangeType::Renamed => {
            let old_path = change.old_path.as_deref().unwrap_or(path);
            Ok((
                Cow::Owned(old_bytes_for(
                    repo,
                    head_files,
                    old_path,
                    &change.old_blob_hash,
                )?),
                current_bytes()?,
            ))
        }
    }
}

fn old_bytes_for(
    repo: &dyn Repository,
    head_files: &HashMap<&str, &ManifestEntry>,
    path: &str,
    recorded: &Option<Hash>,
) -> Result<Vec<u8>> {
    if let Some(hash) = recorded {
        read_blob_bytes(repo, hash)
    } else if let Some(entry) = head_files.get(path) {
        read_blob_bytes(repo, &entry.blob_hash)
    } else {
        Ok(Vec::new())
    }
}

fn read_blob_bytes(repo: &dyn Repository, hash: &Hash) -> Result<Vec<u8>> {
    Ok(repo.get_blob(hash)?.map(|b| b.content).unwrap_or_default())
}

fn read_worktree_bytes(work_tree: &Path, rel: &str) -> Result<Vec<u8>> {
    let path = work_tree.join(rel);
    if path.exists() {
        Ok(fs::read(path)?)
    } else {
        Ok(Vec::new())
    }
}

pub(crate) fn render_change_lines_with_context_and_text(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    change: &FileChange,
    context: usize,
    force_text: bool,
) -> Result<Vec<String>> {
    render_change_lines_with_options(
        repo,
        work_tree,
        head_files,
        change,
        &HashMap::new(),
        RenderOptions {
            context,
            word_diff: false,
            force_text,
        },
    )
}

/// Compute the raw (uncolored) diff block lines for one change — the single
/// source of truth for *what* a change's diff looks like. The first line is
/// always the `diff --oak a/<path> b/<path>` header so the path is never
/// dropped, even when there's no textual hunk (binary, pure rename, mode-only,
/// trailing-newline-only). [`render_change`] colorizes these for `--print`; the
/// TUI in [`build_entries`] styles them per-line for the browser.
fn render_change_lines_with_options(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    change: &FileChange,
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
    options: RenderOptions,
) -> Result<Vec<String>> {
    let path = &change.path;
    let (old_bytes, new_bytes) = change_bytes(repo, work_tree, head_files, change, worktree_blobs)?;
    let rename_header = match change.change_type {
        ChangeType::Renamed => {
            let old_path = change.old_path.as_deref().unwrap_or(path);
            Some(format!("rename from {old_path}\nrename to {path}"))
        }
        _ => None,
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

    let mut lines: Vec<String> = Vec::new();

    // Binary or very large content can't be shown usefully as an inline text
    // diff. Emit a notice so the path still appears — status and commit count
    // it, so diff must too.
    let max_text_bytes = (!options.force_text).then_some(oak_core::MAX_TEXT_DIFF_BYTES);
    if let Some(notice) =
        oak_core::binary_or_oversize_notice(path, &old_bytes, &new_bytes, max_text_bytes)
    {
        lines.push(format!("diff --oak a/{path} b/{path}"));
        if let Some(h) = &rename_header {
            lines.extend(h.lines().map(str::to_string));
        }
        if let Some(h) = &mode_header {
            lines.extend(h.lines().map(str::to_string));
        }
        lines.push(notice);
        return Ok(lines);
    }

    let old_str = String::from_utf8_lossy(&old_bytes);
    let new_str = String::from_utf8_lossy(&new_bytes);
    let diff = FileDiff::new(path, &old_str, &new_str);
    let unified = diff.to_unified_with_context(options.context);

    if !unified.is_empty() {
        // Insert the rename/mode header right after the `diff --oak` line.
        let header = rename_header.as_deref().or(mode_header.as_deref());
        for line in unified.lines() {
            lines.push(line.to_string());
            if line.starts_with("diff --oak") {
                if let Some(h) = header {
                    lines.extend(h.lines().map(str::to_string));
                }
            }
        }
    } else {
        // No textual hunk: a pure rename, a mode-only change, or a byte-level
        // change the line differ can't represent (e.g. a trailing-newline
        // difference). The blob hash still differs, so emit the header — never
        // drop the path.
        lines.push(format!("diff --oak a/{path} b/{path}"));
        if let Some(h) = &rename_header {
            lines.extend(h.lines().map(str::to_string));
        } else if let Some(h) = &mode_header {
            lines.extend(h.lines().map(str::to_string));
        } else {
            lines.push(format!("Files a/{path} and b/{path} differ"));
        }
    }
    if options.word_diff {
        apply_word_diff_markers(&mut lines);
    }
    Ok(lines)
}

/// Cap on how many lines of a removed run are paired with the following
/// added run for intra-line diffs; beyond this the pairing is mostly noise
/// and the char-diff cost adds up.
const MAX_INLINE_PAIRS: usize = 32;

/// Pair each removed run with the added run that follows it (index-wise)
/// and compute the intra-line changed ranges for both sides. Returns one
/// entry per input line: byte ranges into that exact line string (prefix
/// included), empty when there is nothing to emphasize.
fn compute_line_emphasis(lines: &[String]) -> Vec<Vec<std::ops::Range<usize>>> {
    let mut emphasis: Vec<Vec<std::ops::Range<usize>>> = vec![Vec::new(); lines.len()];
    // Only lines inside a hunk (after an `@@` header) are diff content.
    // Prefix heuristics alone would misread content that itself starts
    // with `--`/`++` (a removed `--flag` renders as `---flag`) as a file
    // header, splitting runs and pairing lines against the wrong partner.
    let mut hunk_starts = vec![false; lines.len()];
    let mut in_hunk = false;
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("diff --oak") {
            in_hunk = false;
        } else if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        hunk_starts[index] = in_hunk;
    }
    let is_removed = |i: usize| hunk_starts[i] && lines[i].starts_with('-');
    let is_added = |i: usize| hunk_starts[i] && lines[i].starts_with('+');
    let mut i = 0;
    while i < lines.len() {
        if !is_removed(i) {
            i += 1;
            continue;
        }
        let removed_start = i;
        while i < lines.len() && is_removed(i) {
            i += 1;
        }
        let added_start = i;
        while i < lines.len() && is_added(i) {
            i += 1;
        }
        let pairs = (added_start - removed_start)
            .min(i - added_start)
            .min(MAX_INLINE_PAIRS);
        for k in 0..pairs {
            let old_line = &lines[removed_start + k];
            let new_line = &lines[added_start + k];
            if let Some((old_ranges, new_ranges)) =
                oak_core::inline_diff_ranges(&old_line[1..], &new_line[1..])
            {
                // Shift past the +/- prefix byte.
                emphasis[removed_start + k] = old_ranges
                    .into_iter()
                    .map(|r| r.start + 1..r.end + 1)
                    .collect();
                emphasis[added_start + k] = new_ranges
                    .into_iter()
                    .map(|r| r.start + 1..r.end + 1)
                    .collect();
            }
        }
    }
    emphasis
}

/// `--word-diff`: wrap the intra-line changed segments of paired lines in
/// `[-…-]` / `{+…+}` markers (plain text, works without color).
fn apply_word_diff_markers(lines: &mut [String]) {
    let emphasis = compute_line_emphasis(lines);
    for (line, ranges) in lines.iter_mut().zip(emphasis) {
        if ranges.is_empty() {
            continue;
        }
        let (open, close) = if line.starts_with('-') {
            ("[-", "-]")
        } else {
            ("{+", "+}")
        };
        let mut marked = String::with_capacity(line.len() + ranges.len() * 4);
        let mut cursor = 0;
        for range in &ranges {
            marked.push_str(&line[cursor..range.start]);
            marked.push_str(open);
            marked.push_str(&line[range.clone()]);
            marked.push_str(close);
            cursor = range.end;
        }
        marked.push_str(&line[cursor..]);
        *line = marked;
    }
}

fn mode_octal(mode: FileMode) -> &'static str {
    match mode {
        FileMode::Regular => "100644",
        FileMode::Executable => "100755",
        FileMode::Symlink => "120000",
    }
}

// ---------------------------------------------------------------------------
// Interactive file-tree browser
//
// `oak diff` (no `--print`, attached to a terminal) opens a two-pane TUI: a
// collapsible tree of the changed files on the left, the selected file's diff
// on the right. The diff lines come from the very same [`render_change_lines_with_options`]
// that `--print` colorizes, so the two views never disagree about content.
// ---------------------------------------------------------------------------

use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use crate::commands::keys::{self, Nav, SpaceKey};

/// One changed file, ready for the browser: its path, how to label it (renames
/// show `old -> new`), the kind of change, and the raw diff block lines.
pub(crate) struct FileEntry {
    path: String,
    display_path: String,
    change_type: ChangeType,
    lines: Vec<String>,
    /// Per-line intra-line changed byte ranges (into the exact line string),
    /// for word-level emphasis; empty when there is nothing to highlight.
    emphasis: Vec<Vec<std::ops::Range<usize>>>,
    /// Noise classification (lockfile/vendored/generated) — dimmed and
    /// auto-collapsed in the tree. Path-based only here; byte-based checks
    /// stay in the JSON summaries.
    category: Option<&'static str>,
    /// Net-merge: the merge prediction says this file would conflict.
    conflict: bool,
}

/// Build the per-file entries the browser navigates from an already-computed
/// change set. Shared by the working-tree diff ([`run_tui`]) and the mount diff
/// so both browse identically.
pub(crate) fn build_entries(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    changes: &[FileChange],
) -> Result<Vec<FileEntry>> {
    build_entries_with_worktree_blobs(
        repo,
        work_tree,
        head_files,
        changes,
        &HashMap::new(),
        RenderOptions::default(),
    )
}

fn build_entries_with_worktree_blobs(
    repo: &dyn Repository,
    work_tree: &Path,
    head_files: &HashMap<&str, &ManifestEntry>,
    changes: &[FileChange],
    worktree_blobs: &HashMap<Hash, Vec<u8>>,
    options: RenderOptions,
) -> Result<Vec<FileEntry>> {
    let mut entries = Vec::with_capacity(changes.len());
    for change in changes {
        let lines = render_change_lines_with_options(
            repo,
            work_tree,
            head_files,
            change,
            worktree_blobs,
            options,
        )?;
        let display_path = match (change.change_type, &change.old_path) {
            (ChangeType::Renamed, Some(old)) => format!("{old} -> {}", change.path),
            _ => change.path.clone(),
        };
        let emphasis = compute_line_emphasis(&lines);
        entries.push(FileEntry {
            path: change.path.clone(),
            display_path,
            change_type: change.change_type,
            lines,
            emphasis,
            category: super::review::classify_change(&change.path, None, None, false),
            conflict: false,
        });
    }
    Ok(entries)
}

/// A node in the changed-files tree: either a directory (with children) or a
/// leaf file (carrying its index into the entries vec).
struct Node {
    name: String,
    is_dir: bool,
    expanded: bool,
    file_idx: Option<usize>,
    children: Vec<Node>,
}

fn insert_path(nodes: &mut Vec<Node>, parts: &[&str], idx: usize) {
    let (head, rest) = match parts.split_first() {
        Some(v) => v,
        None => return,
    };
    if rest.is_empty() {
        nodes.push(Node {
            name: (*head).to_string(),
            is_dir: false,
            expanded: false,
            file_idx: Some(idx),
            children: Vec::new(),
        });
        return;
    }
    if let Some(pos) = nodes.iter().position(|n| n.is_dir && n.name == *head) {
        insert_path(&mut nodes[pos].children, rest, idx);
    } else {
        let mut dir = Node {
            name: (*head).to_string(),
            is_dir: true,
            expanded: true,
            file_idx: None,
            children: Vec::new(),
        };
        insert_path(&mut dir.children, rest, idx);
        nodes.push(dir);
    }
}

/// Sort each level: directories first, then files, alphabetically within each.
fn sort_nodes(nodes: &mut [Node]) {
    nodes.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    for n in nodes.iter_mut() {
        if n.is_dir {
            sort_nodes(&mut n.children);
        }
    }
}

fn build_tree(entries: &[FileEntry]) -> Vec<Node> {
    let mut roots = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let parts: Vec<&str> = e.path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        insert_path(&mut roots, &parts, i);
    }
    sort_nodes(&mut roots);
    roots
}

/// Collapse every directory whose leaf descendants are all noise-classified
/// (lockfiles, vendored, generated). The files stay reachable — expanding
/// the node shows them — they just stop dominating the tree.
fn auto_collapse_noise(nodes: &mut [Node], entries: &[FileEntry]) -> bool {
    let mut all_noise = true;
    for node in nodes.iter_mut() {
        let node_noise = if node.is_dir {
            let children_noise = auto_collapse_noise(&mut node.children, entries);
            if children_noise {
                node.expanded = false;
            }
            children_noise
        } else {
            node.file_idx
                .is_some_and(|idx| entries[idx].category.is_some())
        };
        all_noise &= node_noise;
    }
    all_noise && !nodes.is_empty()
}

/// A flattened, currently-visible tree row (collapsed subtrees omitted).
struct Row {
    depth: usize,
    idx_path: Vec<usize>,
    is_dir: bool,
    expanded: bool,
    name: String,
    file_idx: Option<usize>,
}

fn flatten(nodes: &[Node], depth: usize, prefix: &mut Vec<usize>, out: &mut Vec<Row>) {
    for (i, n) in nodes.iter().enumerate() {
        prefix.push(i);
        out.push(Row {
            depth,
            idx_path: prefix.clone(),
            is_dir: n.is_dir,
            expanded: n.expanded,
            name: n.name.clone(),
            file_idx: n.file_idx,
        });
        if n.is_dir && n.expanded {
            flatten(&n.children, depth + 1, prefix, out);
        }
        prefix.pop();
    }
}

fn node_at_mut<'a>(nodes: &'a mut [Node], idx_path: &[usize]) -> Option<&'a mut Node> {
    let (first, rest) = idx_path.split_first()?;
    let node = nodes.get_mut(*first)?;
    if rest.is_empty() {
        Some(node)
    } else {
        node_at_mut(&mut node.children, rest)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    Diff,
}

struct App {
    entries: Vec<FileEntry>,
    roots: Vec<Node>,
    rows: Vec<Row>,
    selected: usize,
    focus: Focus,
    cur_file: Option<usize>,
    diff_scroll: usize,
    title: String,
    quit: bool,
    /// Visible rows in each pane, recorded at draw time so paging follows the
    /// window like `less` instead of a hard-coded 20 lines.
    tree_rows: usize,
    diff_rows: usize,
    /// Whether the `?` key-binding overlay is up.
    show_help: bool,
}

impl App {
    fn new(entries: Vec<FileEntry>, title: String) -> Self {
        let mut roots = build_tree(&entries);
        // Fold away directories that hold nothing but noise (vendored
        // trees, lockfiles): present, but not in the way.
        auto_collapse_noise(&mut roots, &entries);
        let mut app = App {
            entries,
            roots,
            rows: Vec::new(),
            selected: 0,
            focus: Focus::Tree,
            cur_file: None,
            diff_scroll: 0,
            title,
            quit: false,
            // Sensible pre-first-draw defaults; `draw` overwrites both with
            // the real pane heights before any key can be pressed.
            tree_rows: 20,
            diff_rows: 20,
            show_help: false,
        };
        app.rebuild_rows();
        // Start on the first actual file, not a parent directory.
        if let Some(pos) = app.rows.iter().position(|r| r.file_idx.is_some()) {
            app.selected = pos;
        }
        app.sync_current();
        app
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        flatten(&self.roots, 0, &mut Vec::new(), &mut rows);
        self.rows = rows;
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    /// Keep `cur_file`/`diff_scroll` in step with the selected row: when the
    /// selection lands on a different file, show its diff from the top.
    fn sync_current(&mut self) {
        let new = self.rows.get(self.selected).and_then(|r| r.file_idx);
        if new != self.cur_file {
            self.cur_file = new;
            self.diff_scroll = 0;
        }
    }

    fn move_sel(&mut self, delta: i32) {
        if self.rows.is_empty() {
            return;
        }
        let cur = self.selected as i32;
        let next = (cur + delta).clamp(0, self.rows.len() as i32 - 1) as usize;
        self.selected = next;
        self.sync_current();
    }

    fn select_row(&mut self, n: usize) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = n.min(self.rows.len() - 1);
        self.sync_current();
    }

    fn toggle_dir(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if !row.is_dir {
            return;
        }
        let idx_path = row.idx_path.clone();
        if let Some(node) = node_at_mut(&mut self.roots, &idx_path) {
            node.expanded = !node.expanded;
        }
        self.rebuild_rows();
        self.sync_current();
    }

    /// Enter / l: expand a collapsed dir, step into an expanded one, or jump to
    /// the diff pane on a file.
    fn expand_or_focus(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if row.file_idx.is_some() {
            if self.cur_file.is_some() {
                self.focus = Focus::Diff;
            }
            return;
        }
        if row.is_dir {
            if row.expanded {
                self.move_sel(1);
            } else {
                self.toggle_dir();
            }
        }
    }

    /// h / ←: collapse an expanded dir.
    fn collapse(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if row.is_dir && row.expanded {
            self.toggle_dir();
        }
    }

    fn activate(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if row.is_dir {
            self.toggle_dir();
        } else if self.cur_file.is_some() {
            self.focus = Focus::Diff;
        }
    }

    fn scroll_diff(&mut self, delta: i32) {
        let next = self.diff_scroll as i32 + delta;
        self.diff_scroll = next.max(0) as usize; // upper bound clamped at draw
    }
}

/// RAII restore of terminal state — see the twin in `log.rs` for why `Show` is
/// load-bearing. Raw mode + alternate screen on `enter`, undone on `drop` even
/// through `?` early-returns and panics.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = io::stdout().execute(LeaveAlternateScreen);
        let _ = io::stdout().execute(Show);
        let _ = disable_raw_mode();
    }
}

/// Run the browser over a prepared set of file entries. Returns immediately with
/// a "No differences" notice if the set is empty.
pub(crate) fn run_tui_entries(entries: Vec<FileEntry>, title: &str) -> Result<()> {
    if entries.is_empty() {
        output::info("No differences");
        return Ok(());
    }

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(entries, title.to_string());

    while !app.quit {
        terminal.draw(|f| draw(f, &mut app))?;
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(&mut app, key);
                }
            }
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, ev: KeyEvent) {
    let key = ev.code;

    // The `?` overlay swallows everything: any key dismisses it.
    if app.show_help {
        app.show_help = false;
        return;
    }
    if keys::is_help(&ev) {
        app.show_help = true;
        return;
    }

    // Motions first, in every dialect (arrows / vi+less / emacs). In the tree
    // `Space` stays the expand/collapse toggle it has always been; in the diff
    // pane it advances a page, the way `less` does.
    let space = match app.focus {
        Focus::Tree => SpaceKey::Reserved,
        Focus::Diff => SpaceKey::Pages,
    };
    if let Some(motion) = keys::nav(&ev, space) {
        match app.focus {
            Focus::Tree => match motion {
                Nav::Top => app.select_row(0),
                Nav::Bottom => app.select_row(usize::MAX),
                other => {
                    if let Some(d) = keys::delta(other, app.tree_rows) {
                        app.move_sel(d);
                    }
                }
            },
            Focus::Diff => match motion {
                Nav::Top => app.diff_scroll = 0,
                Nav::Bottom => app.diff_scroll = usize::MAX,
                other => {
                    if let Some(d) = keys::delta(other, app.diff_rows) {
                        app.scroll_diff(d);
                    }
                }
            },
        }
        return;
    }

    match app.focus {
        Focus::Tree => match key {
            KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
            KeyCode::Enter => app.activate(),
            KeyCode::Char(' ') => app.toggle_dir(),
            KeyCode::Right | KeyCode::Char('l') => app.expand_or_focus(),
            KeyCode::Left | KeyCode::Char('h') => app.collapse(),
            KeyCode::Tab => {
                if app.cur_file.is_some() {
                    app.focus = Focus::Diff;
                }
            }
            _ => {}
        },
        Focus::Diff => match key {
            KeyCode::Char('q')
            | KeyCode::Esc
            | KeyCode::Left
            | KeyCode::Char('h')
            | KeyCode::Tab => app.focus = Focus::Tree,
            _ => {}
        },
    }
}

fn status_glyph(ct: ChangeType) -> (&'static str, Color) {
    match ct {
        ChangeType::Added => ("A", Color::Green),
        ChangeType::Modified => ("M", Color::Yellow),
        ChangeType::Deleted => ("D", Color::Red),
        ChangeType::Renamed => ("R", Color::Cyan),
    }
}

/// Style a raw diff line for the browser — same prefix heuristic as the ANSI
/// [`output::format_diff_line`] used by `--print`.
fn style_diff_line(line: &str) -> Style {
    if line.starts_with('+') && !line.starts_with("+++") {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') && !line.starts_with("---") {
        Style::default().fg(Color::Red)
    } else if line.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else if line.starts_with("diff ") || line.starts_with("--- ") || line.starts_with("+++ ") {
        Style::default().bold()
    } else {
        Style::default()
    }
}

/// Build the ratatui line for one diff row: the base prefix style plus
/// REVERSED emphasis over the intra-line changed ranges, so word-level
/// edits pop out of long lines.
fn styled_diff_line(line: &str, emphasis: &[std::ops::Range<usize>]) -> Line<'static> {
    let base = style_diff_line(line);
    if emphasis.is_empty() {
        return Line::from(Span::styled(line.to_string(), base));
    }
    let highlight = base.add_modifier(Modifier::REVERSED);
    let mut spans = Vec::with_capacity(emphasis.len() * 2 + 1);
    let mut cursor = 0;
    for range in emphasis {
        // Ranges come from the same string; clamp defensively anyway.
        let start = range.start.min(line.len());
        let end = range.end.min(line.len());
        if start > cursor {
            spans.push(Span::styled(line[cursor..start].to_string(), base));
        }
        if end > start {
            spans.push(Span::styled(line[start..end].to_string(), highlight));
        }
        cursor = end.max(cursor);
    }
    if cursor < line.len() {
        spans.push(Span::styled(line[cursor..].to_string(), base));
    }
    Line::from(spans)
}

fn draw(f: &mut Frame, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(36), Constraint::Min(20)])
        .split(outer[0]);

    draw_tree(f, app, panes[0]);
    draw_diff(f, app, panes[1]);

    let help = if app.focus == Focus::Tree {
        " ↑↓/jk/^P^N move · PgDn/^V page · ←/→ collapse/expand · Space fold · Enter/Tab view diff · ? keys · q quit"
    } else {
        " ↑↓/jk/^P^N scroll · Space/PgDn/^V page · ^U^D half · g/G top/bottom · ←/Esc/Tab back to tree · ? keys"
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            help,
            Style::default().fg(Color::DarkGray),
        ))),
        outer[1],
    );

    if app.show_help {
        draw_help_overlay(f, f.area());
    }
}

/// The `?` overlay: every binding the viewer answers to, in one centered box.
/// Built from [`keys::HELP_ROWS`] so it can't drift from the matcher. The file
/// tree keeps `Space` as its fold toggle, which the extra row spells out.
fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let mut lines: Vec<Line<'static>> = keys::HELP_ROWS
        .iter()
        .map(|(k, what)| {
            Line::from(vec![
                Span::styled(format!("{k:<42}"), Style::default().fg(Color::Cyan)),
                Span::styled((*what).to_string(), Style::default().fg(Color::Gray)),
            ])
        })
        .collect();
    lines.push(Line::from(vec![
        Span::styled(
            format!("{:<42}", "Space (file tree)"),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(
            "fold / unfold a directory".to_string(),
            Style::default().fg(Color::Gray),
        ),
    ]));

    let width = 62.min(area.width.saturating_sub(2));
    let height = (lines.len() as u16 + 2).min(area.height);
    let box_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, box_area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" keys — any key to close "),
        ),
        box_area,
    );
}

fn draw_tree(f: &mut Frame, app: &mut App, area: Rect) {
    let tree_focused = app.focus == Focus::Tree;
    // Remember the visible row count so a page key advances one screenful,
    // the way `less` does, instead of a fixed 20 rows.
    app.tree_rows = area.height.saturating_sub(2) as usize;
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth);
            if row.is_dir {
                let arrow = if row.expanded { "▾" } else { "▸" };
                ListItem::new(Line::from(vec![Span::styled(
                    format!("{indent}{arrow} {}/", row.name),
                    Style::default().fg(Color::Blue).bold(),
                )]))
            } else {
                let entry = row.file_idx.map(|i| &app.entries[i]);
                let (letter, color) = match entry {
                    Some(entry) if entry.conflict => ("!", Color::Red),
                    Some(entry) => status_glyph(entry.change_type),
                    None => (" ", Color::Gray),
                };
                let name_style = match entry {
                    // Noise files stay visible but recede.
                    Some(entry) if entry.category.is_some() => Style::default().fg(Color::DarkGray),
                    Some(entry) if entry.conflict => Style::default().fg(Color::Red),
                    _ => Style::default(),
                };
                ListItem::new(Line::from(vec![
                    Span::raw(indent),
                    Span::styled(format!("{letter} "), Style::default().fg(color)),
                    Span::styled(row.name.clone(), name_style),
                ]))
            }
        })
        .collect();

    let n = app.entries.len();
    let title = format!(
        " {} · {} file{} ",
        app.title,
        n,
        if n == 1 { "" } else { "s" }
    );
    let border_style = if tree_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let highlight = if tree_focused {
        Style::default()
            .bg(Color::DarkGray)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title),
        )
        .highlight_style(highlight)
        .highlight_symbol("");

    let mut state = ListState::default();
    if !app.rows.is_empty() {
        state.select(Some(app.selected));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_diff(f: &mut Frame, app: &mut App, area: Rect) {
    let diff_focused = app.focus == Focus::Diff;
    app.diff_rows = area.height.saturating_sub(2) as usize;
    let border_style = if diff_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let (title, lines): (String, Vec<Line>) = match app.cur_file {
        Some(i) => {
            let content_height = area.height.saturating_sub(2) as usize;
            let max_scroll = app.entries[i].lines.len().saturating_sub(content_height);
            // Clamp here so PageDown / G can overshoot in the handler.
            if app.diff_scroll > max_scroll {
                app.diff_scroll = max_scroll;
            }
            let scroll = app.diff_scroll;
            let entry = &app.entries[i];
            let lines: Vec<Line> = entry
                .lines
                .iter()
                .enumerate()
                .skip(scroll)
                .take(content_height)
                .map(|(index, l)| {
                    styled_diff_line(l, entry.emphasis.get(index).map_or(&[][..], |e| e))
                })
                .collect();
            (format!(" {} ", entry.display_path), lines)
        }
        None => (
            " diff ".to_string(),
            vec![Line::from(Span::styled(
                "Select a file to view its diff",
                Style::default().fg(Color::DarkGray),
            ))],
        ),
    };

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(title),
    );
    f.render_widget(para, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::SqliteRepository;
    use tempfile::TempDir;

    fn change(path: &str, old_path: Option<&str>) -> FileChange {
        FileChange {
            path: path.to_string(),
            change_type: if old_path.is_some() {
                ChangeType::Renamed
            } else {
                ChangeType::Modified
            },
            old_path: old_path.map(str::to_string),
            old_blob_hash: None,
            new_blob_hash: None,
            old_mode: None,
            new_mode: None,
        }
    }

    fn paths(changes: &[FileChange]) -> Vec<&str> {
        changes.iter().map(|c| c.path.as_str()).collect()
    }

    #[test]
    fn filter_matches_directories_component_wise_not_by_string_prefix() {
        let changes = vec![
            change("src/a.txt", None),
            change("src2/b.txt", None),
            change("src/nested/c.txt", None),
        ];
        // `src` must match files *under* src/, never src2/.
        let filtered = filter_changes(changes, &["src".to_string()]);
        assert_eq!(paths(&filtered), vec!["src/a.txt", "src/nested/c.txt"]);
    }

    #[test]
    fn filter_matches_exact_files_and_trailing_slash_dirs() {
        let changes = vec![change("src/a.txt", None), change("docs/x.md", None)];
        let filtered = filter_changes(changes.clone(), &["docs/x.md".to_string()]);
        assert_eq!(paths(&filtered), vec!["docs/x.md"]);
        // A trailing slash on a directory filter is accepted.
        let filtered = filter_changes(changes, &["src/".to_string()]);
        assert_eq!(paths(&filtered), vec!["src/a.txt"]);
    }

    #[test]
    fn filter_matches_renames_on_either_side() {
        let changes = vec![change("new/dest.txt", Some("old/source.txt"))];
        // Filtering by where the file *was* still surfaces the rename.
        let filtered = filter_changes(changes.clone(), &["old".to_string()]);
        assert_eq!(paths(&filtered), vec!["new/dest.txt"]);
        let filtered = filter_changes(changes, &["new".to_string()]);
        assert_eq!(paths(&filtered), vec!["new/dest.txt"]);
    }

    #[test]
    fn empty_filter_set_passes_everything_and_root_filter_matches_all() {
        let changes = vec![change("a.txt", None), change("b/c.txt", None)];
        assert_eq!(filter_changes(changes.clone(), &[]).len(), 2);
        // `oak diff .` from the repo root resolves to the empty repo-relative
        // path, which matches everything.
        assert_eq!(filter_changes(changes, &[String::new()]).len(), 2);
    }

    #[test]
    fn render_change_uses_ephemeral_worktree_bytes_without_rereading_disk() {
        let temp = TempDir::new().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
        let new_bytes = b"line one\nline two\n".to_vec();
        let new_hash = oak_core::hash_bytes(&new_bytes);
        let change = FileChange {
            path: "tracked.txt".to_string(),
            change_type: ChangeType::Added,
            old_path: None,
            old_blob_hash: None,
            new_blob_hash: Some(new_hash.clone()),
            old_mode: None,
            new_mode: None,
        };
        let mut worktree_blobs = HashMap::new();
        worktree_blobs.insert(new_hash, new_bytes);

        let lines = render_change_lines_with_options(
            &repo,
            temp.path(),
            &HashMap::new(),
            &change,
            &worktree_blobs,
            RenderOptions::default(),
        )
        .unwrap();

        assert!(
            lines.iter().any(|line| line == "+line two"),
            "diff should reuse the scan's worktree bytes instead of rereading a missing file: {lines:?}"
        );
    }
}

/// Key-binding wiring for the diff browser. The shared table in
/// [`crate::commands::keys`] is unit-tested there; these cover what only this
/// viewer can get wrong — chiefly that `Space` keeps its fold meaning in the
/// file tree while paging in the diff pane.
#[cfg(test)]
mod key_tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn entry(path: &str, lines: usize) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            display_path: path.to_string(),
            change_type: ChangeType::Modified,
            lines: (0..lines).map(|i| format!("+line {i}")).collect(),
            emphasis: Vec::new(),
            category: None,
            conflict: false,
        }
    }

    fn app() -> App {
        App::new(
            vec![
                entry("src/a.rs", 400),
                entry("src/b.rs", 400),
                entry("docs/c.md", 400),
            ],
            "test".to_string(),
        )
    }

    fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        handle_key(app, KeyEvent::new(code, mods));
    }

    fn plain(app: &mut App, c: char) {
        press(app, KeyCode::Char(c), KeyModifiers::NONE);
    }

    /// The one binding the two panes disagree about: in the tree `Space` folds
    /// a directory (as it always has), and in the diff pane it advances a page
    /// like `less`.
    #[test]
    fn space_folds_in_the_tree_and_pages_in_the_diff() {
        let mut app = app();
        // Park on a directory row and fold it — the row count must drop.
        app.select_row(0);
        assert!(app.rows[app.selected].is_dir, "first row is a directory");
        let before = app.rows.len();
        plain(&mut app, ' ');
        assert!(app.rows.len() < before, "Space folded the directory");

        // In the diff pane the same key pages instead.
        app.focus = Focus::Diff;
        app.diff_rows = 30;
        plain(&mut app, ' ');
        assert_eq!(app.diff_scroll, 28);
    }

    /// Emacs and less motions reach the diff pane, and paging follows the pane
    /// height rather than a fixed 20 rows.
    #[test]
    fn emacs_and_less_motions_scroll_the_diff_pane() {
        let mut app = app();
        app.focus = Focus::Diff;
        app.diff_rows = 50;

        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(app.diff_scroll, 1);
        press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(app.diff_scroll, 0);

        press(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert_eq!(app.diff_scroll, 48, "C-v pages by the pane height");
        press(&mut app, KeyCode::Char('v'), KeyModifiers::ALT);
        assert_eq!(app.diff_scroll, 0, "M-v pages back");

        plain(&mut app, 'd');
        assert_eq!(app.diff_scroll, 25, "d is a half page");
        plain(&mut app, 'u');
        assert_eq!(app.diff_scroll, 0);
    }

    /// The tree's own keys survive the shared motion layer sitting in front of
    /// them: `l`/`h` still expand and collapse, `Tab` still crosses panes.
    #[test]
    fn tree_navigation_keys_still_work_alongside_the_motion_layer() {
        let mut app = app();
        app.select_row(0);
        plain(&mut app, 'h');
        assert!(!app.rows[0].expanded, "h collapses the directory");
        plain(&mut app, 'l');
        assert!(app.rows[0].expanded, "l expands it again");

        plain(&mut app, 'j');
        assert_eq!(app.selected, 1, "j still moves a single row");

        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::Diff);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.focus, Focus::Tree);
    }

    /// `?` raises the binding overlay and any key dismisses it without also
    /// acting.
    #[test]
    fn help_overlay_opens_and_swallows_the_next_key() {
        let mut app = app();
        plain(&mut app, '?');
        assert!(app.show_help);
        let selected = app.selected;
        plain(&mut app, 'j');
        assert!(!app.show_help);
        assert_eq!(app.selected, selected);
    }

    /// The overlay is a centered box sized against the terminal, so a window
    /// smaller than the box must clamp rather than draw out of bounds — a
    /// panic here would take down a viewer someone opened in a split pane.
    #[test]
    fn help_overlay_renders_at_any_terminal_size() {
        use ratatui::{backend::TestBackend, Terminal};

        for (w, h) in [(120u16, 40u16), (60, 20), (20, 6), (8, 3), (1, 1)] {
            let mut app = app();
            app.show_help = true;
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| draw(f, &mut app)).unwrap();
        }
    }
}
