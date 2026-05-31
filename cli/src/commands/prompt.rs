//! `oak prompt` — a compact, shell-embeddable repo-status segment for PS1.
//!
//! `oak prompt` prints a single line like `● my-feature ⇡2 +2 ~1 -1 `
//! summarizing the current branch, any commits not yet merged into its parent
//! (`⇡N`), and working-tree changes. It's designed to be dropped
//! into a shell prompt (see [`offer_shell_integration`], which `oak login`
//! calls to wire it up): it prints **nothing** when you're not inside an Oak
//! repo, and it **never errors out** (every failure path collapses to empty
//! output), so it's safe to invoke on every prompt render.
//!
//! ## Prompt-width correctness
//!
//! Color escapes are zero-width, but the shell's line editor counts bytes
//! unless told otherwise. The `--shell` flag wraps each escape in the markers
//! that shell honors *inside command-substituted prompt output*:
//!
//! - **bash**: readline's `\x01 … \x02` ("ignore these bytes") markers — *not*
//!   `\[ \]`, which bash only expands in the literal `PS1` string, before the
//!   `$(oak prompt)` substitution runs, so they'd appear verbatim.
//! - **zsh**: `%{ … %}`, honored inside `$(…)` when `PROMPT_SUBST` is set.
//! - **omitted**: raw ANSI, for previewing the segment directly in a terminal.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use dialoguer::Confirm;
use oak_core::{ChangeType, Result};

use crate::output::{self, colors};

/// Sentinel comments bracketing the block we manage in the user's shell rc
/// file. Matching the opening line is how we detect an existing install (so a
/// second `oak login` doesn't append a duplicate block), and the closing line
/// makes the block easy to find and delete by hand.
const BEGIN_MARKER: &str = "# >>> oak prompt >>>";
const END_MARKER: &str = "# <<< oak prompt <<<";

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// How to bracket (zero-width) color escapes so the target shell measures the
/// prompt width correctly. `Raw` emits bare ANSI for previewing in a terminal.
#[derive(Clone, Copy, PartialEq)]
enum Wrap {
    Bash,
    Zsh,
    Raw,
}

impl Wrap {
    /// The (start, end) markers placed around each non-printing escape.
    fn markers(self) -> (&'static str, &'static str) {
        match self {
            // RL_PROMPT_START_IGNORE / RL_PROMPT_END_IGNORE — the raw bytes
            // readline scans for. We emit these directly because `\[ \]` would
            // not survive command substitution (see module docs).
            Wrap::Bash => ("\u{1}", "\u{2}"),
            Wrap::Zsh => ("%{", "%}"),
            Wrap::Raw => ("", ""),
        }
    }
}

/// Colorizer that knows the active shell wrapping. Produced once per render.
struct Painter {
    wrap: Wrap,
    color: bool,
}

impl Painter {
    /// Wrap `text` in `code` (e.g. a color) plus a reset, bracketing both
    /// escapes so they don't count toward the prompt's visible width. With
    /// color off (or an empty code) the text passes through untouched.
    fn paint(&self, code: &str, text: &str) -> String {
        if !self.color || code.is_empty() {
            return text.to_string();
        }
        let (s, e) = self.wrap.markers();
        format!("{s}{code}{e}{text}{s}{reset}{e}", reset = colors::RESET)
    }
}

/// The bits of repo state the segment renders. Branch label, the count of
/// committed-but-unmerged commits on the branch, and per-kind working-tree
/// change counts.
struct PromptData {
    branch: String,
    /// Commits authored on this branch but not yet merged into its parent.
    ahead: usize,
    added: usize,
    modified: usize,
    deleted: usize,
    renamed: usize,
}

impl PromptData {
    /// Whether the *working tree* has uncommitted edits. Deliberately ignores
    /// `ahead`: commits are already saved, so a branch with unmerged commits
    /// and a clean tree is still "clean" (green glyph) — the `⇡N` marker, not
    /// the glyph color, carries the unmerged signal.
    fn dirty(&self) -> bool {
        self.added + self.modified + self.deleted + self.renamed > 0
    }
}

/// Build the one-line segment. Trailing space included so it separates cleanly
/// from whatever prompt it's prepended to.
fn render(p: &Painter, d: &PromptData) -> String {
    // Green when clean, yellow when there's uncommitted work — a glanceable
    // "is my tree dirty?" signal carried by the leading glyph and branch name.
    let state = if d.dirty() {
        colors::YELLOW
    } else {
        colors::GREEN
    };
    let glyph = std::env::var("OAK_PROMPT_GLYPH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "●".to_string());

    let head_code = format!("{}{}", colors::BOLD, state);
    let mut out = p.paint(
        &head_code,
        &format!("{glyph} {}", escape_text(&d.branch, p.wrap)),
    );

    // Commits committed here but not yet merged into the parent (usually main).
    // A branch-level signal, so it sits next to the branch name and ahead of
    // the working-tree counts. `⇡` is the conventional "ahead" arrow.
    if d.ahead > 0 {
        out.push(' ');
        out.push_str(&p.paint(colors::MAGENTA, &format!("⇡{}", d.ahead)));
    }

    for (n, sym, color) in [
        (d.added, "+", colors::GREEN),
        (d.modified, "~", colors::YELLOW),
        (d.deleted, "-", colors::RED),
        (d.renamed, "»", colors::CYAN),
    ] {
        if n > 0 {
            out.push(' ');
            out.push_str(&p.paint(color, &format!("{sym}{n}")));
        }
    }
    out.push(' ');
    out
}

/// Escape literal text that lands in the prompt. zsh treats `%` as a prompt
/// escape introducer even inside substituted output, so a `%` in a branch name
/// has to be doubled; other shells pass it through.
fn escape_text(s: &str, wrap: Wrap) -> String {
    match wrap {
        Wrap::Zsh => s.replace('%', "%%"),
        _ => s.to_string(),
    }
}

// ---------------------------------------------------------------------------
// `oak prompt` entry point
// ---------------------------------------------------------------------------

/// Render the prompt segment for `path` and print it (no trailing newline).
/// Always returns `Ok(())` — a prompt command must never break the shell, so
/// any error (not a repo, scan failure, …) collapses to empty output.
pub fn run(path: &Path, shell: Option<&str>, no_color: bool) -> Result<()> {
    let wrap = parse_wrap(shell);
    let painter = Painter {
        wrap,
        color: decide_color(wrap, no_color),
    };
    if let Some(seg) = segment_for(path, &painter) {
        if !seg.is_empty() {
            print!("{seg}");
            let _ = std::io::stdout().flush();
        }
    }
    Ok(())
}

/// Compute the segment string, or `None` when there's nothing to show (outside
/// a repo, or on any internal error — the prompt stays clean either way).
fn segment_for(path: &Path, painter: &Painter) -> Option<String> {
    if std::env::var_os("OAK_PROMPT_DISABLE").is_some() {
        return None;
    }

    // Inside an `oak mount`, the real state lives in the mount's overlay, not a
    // local `.oak`. Read counts straight from the overlay (no working-tree scan).
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    if let Ok(Some(dest)) = crate::commands::mount::mount_dest_for(path) {
        let (branch, added, modified, deleted, renamed) =
            crate::commands::mount::prompt_counts(&dest).ok()?;
        return Some(render(
            painter,
            &PromptData {
                branch,
                // Mounts track work as a virtual-branch overlay (the active-commit
                // model), not as commits-on-a-branch, so the unmerged `⇡N` signal
                // doesn't map cleanly here — leave it off.
                ahead: 0,
                added,
                modified,
                deleted,
                renamed,
            },
        ));
    }

    // Regular repo: bail quietly if this isn't one.
    crate::resolve::resolve(path).ok()?;
    let (changes, head, branch) = crate::commands::commit::get_status(path).ok()?;

    // Count commits on this branch not yet merged into its parent. Re-opening
    // the repo is cheap (local SQLite) next to the working-tree scan
    // `get_status` just ran; any failure collapses to 0 so the prompt stays
    // quiet. Only meaningful when we're on a named branch (a detached HEAD has
    // no branch to be "ahead" of).
    let ahead = match &branch {
        Some(b) => crate::resolve::resolve(path)
            .ok()
            .and_then(|ctx| ctx.open().ok())
            .and_then(|repo| crate::commands::commit::unmerged_commit_count(repo.as_ref(), b).ok())
            .unwrap_or(0),
        None => 0,
    };

    // Prefer the branch name; fall back to a short hash for a detached HEAD so
    // the prompt still tells you where you are.
    let label = match branch {
        Some(b) => b,
        None => format!("@{}", head?.short()),
    };

    let mut data = PromptData {
        branch: label,
        ahead,
        added: 0,
        modified: 0,
        deleted: 0,
        renamed: 0,
    };
    for c in &changes {
        match c.change_type {
            ChangeType::Added => data.added += 1,
            ChangeType::Modified => data.modified += 1,
            ChangeType::Deleted => data.deleted += 1,
            ChangeType::Renamed => data.renamed += 1,
        }
    }
    Some(render(painter, &data))
}

fn parse_wrap(shell: Option<&str>) -> Wrap {
    match shell.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("zsh") => Wrap::Zsh,
        Some("bash") => Wrap::Bash,
        // Unknown / omitted: raw ANSI. Safe to print; only the width-aware
        // wrapping (which we can't do for an unknown shell) is skipped.
        _ => Wrap::Raw,
    }
}

/// Decide whether to emit color. When a shell is targeted the output is bound
/// for `PS1` (never a TTY directly), so we can't use `isatty` — default on and
/// let the shell render it. `NO_COLOR` / `OAK_NO_COLOR` / `--no-color` win.
fn decide_color(wrap: Wrap, no_color: bool) -> bool {
    if no_color
        || std::env::var_os("NO_COLOR").is_some()
        || std::env::var_os("OAK_NO_COLOR").is_some()
    {
        return false;
    }
    match wrap {
        Wrap::Raw => std::io::stdout().is_terminal(),
        Wrap::Bash | Wrap::Zsh => true,
    }
}

// ---------------------------------------------------------------------------
// Shell-prompt integration (offered by `oak login`)
// ---------------------------------------------------------------------------

/// A shell we know how to wire `oak prompt` into automatically.
#[derive(Clone, Copy)]
enum Shell {
    Bash,
    Zsh,
}

impl Shell {
    fn name(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
        }
    }

    /// The `oak prompt --shell <flag>` value this shell needs.
    fn flag(self) -> &'static str {
        self.name()
    }
}

/// Offer, once and interactively, to add a live `oak prompt` segment to the
/// user's shell prompt. Called at the end of `oak login`.
///
/// Best-effort and quiet by design: it returns without a peep when stdin/stdout
/// aren't a terminal (scripts, CI), when `OAK_NO_PROMPT_SETUP` is set, or when
/// the shell isn't one we can edit automatically. It never propagates an error
/// — failing to set up a prompt should never fail a login.
pub fn offer_shell_integration() {
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return;
    }
    if std::env::var_os("OAK_NO_PROMPT_SETUP").is_some() {
        return;
    }

    let Some((shell, rc_path)) = detect_shell_rc() else {
        // Unknown shell (fish, nushell, …): we can't safely edit its config,
        // so just point the way and move on.
        output::blank();
        output::info("Tip: add live Oak repo status to your prompt — see `oak prompt --help`.");
        return;
    };

    // Already wired up from a previous login? Don't append a second block.
    if rc_contains_marker(&rc_path) {
        output::blank();
        output::info(&format!(
            "Oak status is already in your prompt ({}).",
            rc_path.display()
        ));
        return;
    }

    // Show what it looks like before asking — color sells it.
    let sample = render(
        &Painter {
            wrap: Wrap::Raw,
            color: true,
        },
        &PromptData {
            branch: "my-feature".to_string(),
            ahead: 2,
            added: 2,
            modified: 1,
            deleted: 1,
            renamed: 0,
        },
    );
    output::blank();
    output::print_line(&format!(
        "  {sample}{dim}~/code/app ❯{reset}",
        dim = colors::DIM,
        reset = colors::RESET,
    ));
    output::blank();

    let yes = Confirm::new()
        .with_prompt(format!(
            "Add Oak repo status to your {} prompt?",
            shell.name()
        ))
        .default(true)
        .interact()
        .unwrap_or(false);

    if !yes {
        output::info(&format!(
            "Skipped. To enable it later, re-run `oak login` or add this to {}:",
            rc_path.display()
        ));
        print_snippet(shell);
        return;
    }

    match install(&rc_path, shell) {
        Ok(()) => {
            output::success(&format!(
                "Added Oak status to your prompt ({}).",
                rc_path.display()
            ));
            output::info(&format!(
                "Restart your shell or run: {green}source {}{reset}",
                rc_path.display(),
                green = colors::GREEN,
                reset = colors::RESET,
            ));
        }
        Err(e) => {
            output::warning(&format!("Couldn't update {}: {e}", rc_path.display()));
            output::info("Add this manually to enable it:");
            print_snippet(shell);
        }
    }
}

/// Resolve the current shell (`$SHELL`) and the rc file we'd edit for it.
/// Returns `None` for shells we don't auto-configure.
fn detect_shell_rc() -> Option<(Shell, PathBuf)> {
    let home = dirs::home_dir()?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    let base = Path::new(&shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    if base.contains("zsh") {
        // Honor ZDOTDIR — zsh users who relocate their dotfiles expect it.
        let dir = std::env::var_os("ZDOTDIR")
            .map(PathBuf::from)
            .unwrap_or(home);
        Some((Shell::Zsh, dir.join(".zshrc")))
    } else if base.contains("bash") {
        Some((Shell::Bash, home.join(".bashrc")))
    } else {
        None
    }
}

/// The guarded block we append to the rc file. Prepends the segment to the
/// existing prompt so it sits at the front of the line without us having to
/// understand the user's prompt structure — and contributes nothing outside a
/// repo, since `oak prompt` prints empty there.
fn snippet(shell: Shell) -> String {
    let flag = shell.flag();
    match shell {
        // zsh needs PROMPT_SUBST so the `$(…)` is re-evaluated each render.
        Shell::Zsh => format!(
            "{BEGIN_MARKER}\n\
             # Shows Oak repo status (branch + changes) at the front of your prompt.\n\
             # Added by `oak login`. Delete this block to remove it. Docs: oak prompt --help\n\
             setopt PROMPT_SUBST\n\
             PROMPT='$(oak prompt --shell {flag} 2>/dev/null)'\"$PROMPT\"\n\
             {END_MARKER}\n"
        ),
        // bash performs command substitution in PS1 on every render by default.
        Shell::Bash => format!(
            "{BEGIN_MARKER}\n\
             # Shows Oak repo status (branch + changes) at the front of your prompt.\n\
             # Added by `oak login`. Delete this block to remove it. Docs: oak prompt --help\n\
             PS1='$(oak prompt --shell {flag} 2>/dev/null)'\"$PS1\"\n\
             {END_MARKER}\n"
        ),
    }
}

/// Print the snippet, dimmed and indented, for the "do it yourself" paths.
fn print_snippet(shell: Shell) {
    for line in snippet(shell).lines() {
        output::print_line(&format!("  {}{line}{}", colors::DIM, colors::RESET));
    }
}

fn rc_contains_marker(rc_path: &Path) -> bool {
    std::fs::read_to_string(rc_path)
        .map(|c| c.contains(BEGIN_MARKER))
        .unwrap_or(false)
}

/// Append the guarded block to the rc file (creating it, and any parent dirs,
/// if missing), separated from existing content by a blank line.
fn install(rc_path: &Path, shell: Shell) -> std::io::Result<()> {
    if let Some(parent) = rc_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut contents = std::fs::read_to_string(rc_path).unwrap_or_default();
    if !contents.is_empty() {
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push('\n');
    }
    contents.push_str(&snippet(shell));
    std::fs::write(rc_path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(branch: &str, a: usize, m: usize, d: usize, r: usize) -> PromptData {
        PromptData {
            branch: branch.to_string(),
            ahead: 0,
            added: a,
            modified: m,
            deleted: d,
            renamed: r,
        }
    }

    #[test]
    fn raw_segment_has_no_color_when_disabled() {
        let p = Painter {
            wrap: Wrap::Raw,
            color: false,
        };
        let s = render(&p, &data("main", 2, 1, 1, 0));
        assert_eq!(s, "● main +2 ~1 -1 ");
        assert!(!s.contains('\u{1b}'), "no ANSI escapes when color is off");
    }

    #[test]
    fn ahead_marker_shows_unmerged_commits_before_worktree_counts() {
        let p = Painter {
            wrap: Wrap::Raw,
            color: false,
        };
        let mut d = data("feat", 1, 0, 0, 0);
        d.ahead = 2;
        // `⇡N` sits right after the branch, ahead of the `+/~/-` counts.
        assert_eq!(render(&p, &d), "● feat ⇡2 +1 ");
    }

    #[test]
    fn ahead_marker_omitted_when_nothing_unmerged() {
        let p = Painter {
            wrap: Wrap::Raw,
            color: false,
        };
        // ahead == 0 (the `data` default) renders no `⇡` marker.
        assert!(!render(&p, &data("main", 0, 0, 0, 0)).contains('⇡'));
    }

    #[test]
    fn ahead_only_clean_tree_keeps_green_glyph() {
        // Unmerged commits with a clean tree must not flip the dirty (yellow)
        // state — the tree is clean, the work is just unmerged.
        let mut d = data("feat", 0, 0, 0, 0);
        d.ahead = 3;
        assert!(
            !d.dirty(),
            "unmerged commits alone don't make the tree dirty"
        );
    }

    #[test]
    fn clean_tree_shows_only_branch() {
        let p = Painter {
            wrap: Wrap::Raw,
            color: false,
        };
        assert_eq!(render(&p, &data("main", 0, 0, 0, 0)), "● main ");
    }

    #[test]
    fn bash_wraps_escapes_in_readline_ignore_markers() {
        let p = Painter {
            wrap: Wrap::Bash,
            color: true,
        };
        let s = render(&p, &data("main", 0, 0, 0, 0));
        // Every escape run must be bracketed by \x01 … \x02 so readline doesn't
        // count it toward the visible prompt width.
        assert!(s.contains('\u{1}') && s.contains('\u{2}'));
        assert!(!s.contains("%{"), "bash must not use zsh markers");
    }

    #[test]
    fn zsh_wraps_escapes_and_doubles_percent() {
        let p = Painter {
            wrap: Wrap::Zsh,
            color: true,
        };
        let s = render(&p, &data("feat%x", 1, 0, 0, 0));
        assert!(s.contains("%{") && s.contains("%}"));
        assert!(
            s.contains("feat%%x"),
            "literal % in branch is doubled for zsh"
        );
    }

    #[test]
    fn counts_omit_zero_kinds() {
        let p = Painter {
            wrap: Wrap::Raw,
            color: false,
        };
        // Only renamed is non-zero.
        assert_eq!(render(&p, &data("b", 0, 0, 0, 3)), "● b »3 ");
    }

    #[test]
    fn snippet_is_guarded_and_targets_the_right_shell() {
        let z = snippet(Shell::Zsh);
        assert!(z.contains(BEGIN_MARKER) && z.contains(END_MARKER));
        assert!(z.contains("setopt PROMPT_SUBST"));
        assert!(z.contains("--shell zsh"));

        let b = snippet(Shell::Bash);
        assert!(b.contains("--shell bash"));
        assert!(!b.contains("PROMPT_SUBST"), "bash needs no PROMPT_SUBST");
    }

    #[test]
    fn install_appends_block_and_preserves_existing_content() {
        let dir = std::env::temp_dir().join(format!("oak-prompt-test-{}", std::process::id()));
        let rc = dir.join(".zshrc");
        let _ = std::fs::remove_dir_all(&dir);

        // Pre-existing prompt config with no trailing newline.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&rc, "export PATH=$PATH:/usr/local/bin").unwrap();

        assert!(!rc_contains_marker(&rc), "fresh file has no marker");
        install(&rc, Shell::Zsh).unwrap();

        let after = std::fs::read_to_string(&rc).unwrap();
        assert!(after.starts_with("export PATH"), "existing content kept");
        assert!(after.contains(BEGIN_MARKER) && after.contains(END_MARKER));
        // Blank-line separator between their config and our block.
        assert!(after.contains("/usr/local/bin\n\n# >>> oak prompt >>>"));
        // Now the install is detectable, so a second `oak login` would skip it.
        assert!(rc_contains_marker(&rc));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_creates_missing_file_and_parent_dirs() {
        let dir = std::env::temp_dir().join(format!("oak-prompt-mk-{}", std::process::id()));
        let rc = dir.join("nested").join(".bashrc");
        let _ = std::fs::remove_dir_all(&dir);

        install(&rc, Shell::Bash).unwrap();
        let after = std::fs::read_to_string(&rc).unwrap();
        assert!(
            after.starts_with(BEGIN_MARKER),
            "no stray leading blank line"
        );
        assert!(after.contains("--shell bash"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
