# Oak command reference

Authoritative source is always `oak <command> --help`. This is a working
summary for agents. Oak is **not** Git — none of these take a `-m` message.

## Inspecting & committing

| Command | What it does |
| --- | --- |
| `oak status` | Show working-tree changes. Long branch descriptions show their first line only. |
| `oak status --porcelain` | Stable compact changed-path rows for scripts. Works in regular checkouts and mounts. |
| `oak status --short` | Git-compatible spelling for `oak status --porcelain`. |
| `oak status --json` | Machine-readable working-tree status. Works in regular checkouts and mounts; mount JSON includes virtual-branch and overlay metadata. |
| `oak info --json` | Machine-readable repo and current branch metadata. Works in regular checkouts and mounts. |
| `oak agent state --json --compact` | Lowest-token preflight for unattended agents: context, branch, dirty/conflict/progress state, blocked/finish state, and recommended next commands. |
| `oak agent state --json [--refresh]` | Full preflight document for unattended agents: context, repo/branch, dirty changes, conflict/progress state, unpushed count, freshness fields, mount metadata when applicable, and recommended next commands. Ordinary reads stay offline; `--refresh` explicitly contacts the remote to populate remote parent/current-branch heads where supported. |
| `oak diff [PATHS] [--print] [--stat]` | Browse changes in a TUI, `--print` to dump to stdout, `--stat` for per-file `+added -removed` counts. PATHS (files or directories) scope the diff — prefer scoping over full-repo dumps. |
| `oak diff --name-only [PATHS]` | Changed paths only. Use this before requesting full diff text. |
| `oak commit [--no-verify]` | Snapshot the working tree. **No message** — descriptions carry intent. `--no-verify` skips hooks. |
| `oak log [-n N] [-v]` | Commit history. Piped output is one line per commit and defaults to the latest 20, ending with a `... more commits` hint; `-n` widens the window, `-v` is verbose. |
| `oak log --oneline [-n N]` | Explicit compact commit history, one row per commit. |
| `oak log --json` | Machine-readable commit history. Works in regular checkouts and mounts. |
| `oak hash` | Print the current HEAD commit hash (bare, for scripting). |
| `oak restore [PATHS] [-s HASH] [-f]` | Restore files from HEAD (or `-s <commit>`). |
| `oak reset [PATH] [-f]` | Discard uncommitted changes in the working tree. |
| `oak checkout <HASH>` | Detach HEAD at a commit (full hash or ≥4-char prefix). |

## Branching (flat model — everything parents onto `main`)

| Command | What it does |
| --- | --- |
| `oak switch [NAME] [-c] [--clean] [-d]` | Switch branches. No name picks interactively; `-c` creates a generated branch from latest available `main`; `-c NAME` creates a named branch from latest available `main`; both keep dirty files unless `--clean` is passed; `NAME --clean` switches to an existing branch without carrying dirty files. `--clean` discards current working-tree changes. `-d` detaches. |
| `oak desc "<text>"` | Set the **current branch's description** — the squash-merge message it lands with on `main`. |
| `oak desc --file <file>` | Set the current branch's description from a UTF-8 file. Prefer this in agent workflows, especially for multiline text. |
| `oak desc --file -` | Set the current branch's description by reading stdin. Use when piping generated text directly. |
| `oak finish --desc "<text>" [--json]` | Finish the current checkout or mount: refuses unresolved merge/sync state and empty descriptions, writes the branch description, commits dirty work if needed, pushes committed work, and syncs branch metadata. In a mount it delegates to mount finalization and ends the mount. |
| `oak finish --desc-file <file> [--json]` | Same as `oak finish --desc`, reading the final description from a UTF-8 file or `-` for stdin. Prefer this for agent-generated multiline summaries. |
| `oak branch --json` / `oak branch list --json` | Machine-readable branch inventory. Both spellings are accepted. |
| `oak branch list --remote --json [--status open\|closed]` | Read branch metadata directly from the configured remote without switching branches. Use this for triage queues. |
| `oak branch show <NAME> --remote --json` | Show one remote branch's metadata without switching to it. |
| `oak branch diff <NAME> --remote --against main --json` | Checkout-free changed-file evidence for a remote branch. The command refreshes remote branch metadata/manifests but does not materialize files. |
| `oak branch review <NAME> --remote --merge-preview --json` | Checkout-free branch review with changed files, lineage, identical-file sample, caveats, recommendations, and optional merge preview. |
| `oak merge [--continue\|--abort]` | Merge the current branch into `main`. Resume/abort a conflicted merge with the flags. |
| `oak conflict status --json` | Inspect any in-progress checkout or mount merge/pull conflict: kind, recorded paths, state files, and next commands. |
| `oak conflict show --json` | Show per-path conflict facts: recorded, exists, marker state, resolution state, and whether `take` is available. |
| `oak conflict take <path> --ours` / `--theirs` | In regular checkouts, resolve one conflicted path by choosing the branch or parent side of each marker block while preserving already-merged hunks. Mount conflicts still require editing the mounted file manually. |
| `oak close [NAME]` | Close a branch (defaults to current). |
| `oak close <NAME> --remote --json` | Close a remote branch without switching to it. Use this to clean up stale remote-only branches during triage. |
| `oak split [--from BRANCH] [--plan FILE\|-] [--dry-run]` | Split a branch's commits into independent branches off `main` (and reorder/drop) — like `git rebase -i` + split. Interactive TUI by default; **agents must pass `--plan`** to run it headless. |

Branches can only be parented onto `main`. Attempting to parent onto another
branch is rejected — Oak is deliberately flat (branch-per-task, each merges
back into `main`).

### Splitting commits headlessly (`oak split --plan`)

A plan is one directive per line (`#` comments allowed). Every commit on the
branch must appear exactly once as `pick` or `drop`; hash prefixes are fine
(get them from `oak log --json`). Picks before the first `split` rewrite the
source branch in place; each `split <name>` starts a new branch off `main`
that collects the picks after it.

```bash
oak split --dry-run --plan - <<'EOF'   # preview first; drop --dry-run to apply
pick a1b2c3        # stays on the current branch
drop d4e5f6        # discarded
split fix-typos    # new branch off main with the picks below
pick 778899
EOF
```

Because branches are flat, every split segment must stand alone on `main`. If
a segment depends on changes in another (it edits a file another segment
introduced), the replay conflicts and **nothing is written** — keep dependent
commits in the same segment. The working tree must be clean, and the
rewritten source branch needs `oak push --force` afterward.

Avoid shell-quoted multiline branch descriptions in agent workflows. Write the
description text to a temp file and run `oak desc --file <file>` instead.

## Syncing with a server

| Command | What it does |
| --- | --- |
| `oak push [-f] [--repo OWNER/NAME] [-r URL]` | Push the current branch. `-f` overwrites diverged remote history. `--repo` links a fresh repo without the interactive org picker (useful for agents/CI). |
| `oak pull [-f] [--continue\|--abort] [-r URL]` | Fetch the current branch and bring it up to date (merges parent changes). `-f` discards local commits not on the remote. |
| `oak fetch [-r URL]` | Refresh the local copy of `main` without merging or touching the working tree. |
| `oak login [-r URL]` | Log in to an Oak server (default `https://oakvcs.com`). |

`OWNER` is an **organization** (your personal account or a shared org) — the
left side of `owner/repo`, e.g. `oak/oak`.

Headless first-push recipe:

```bash
oak init
oak commit
oak push --repo <org>/<repo>
```

`oak push --repo` persists the local repo identity and creates the server repo
on first push if needed, so agents do not need the interactive organization
picker.

## Creating & moving repos

| Command | What it does |
| --- | --- |
| `oak init [PATH]` | Initialize a repo in the current/given dir. Offers to import existing git history and to write an `AGENTS.md` so agents know it's an Oak repo. |
| `oak clone [OWNER/NAME] [DEST] [--branch NAME] [--shallow]` | Clone from the server (interactive picker if no spec). `--branch` checks out a named Oak branch after cloning and requires `OWNER/NAME`; it is not supported for git remote URLs. `--shallow` skips full history. |
| `oak export DEST [-b BRANCH] [--git-branch NAME] [-f]` | Replay history into a fresh **git** repo (escape hatch off Oak), preserving author + timestamp. |
| `oak archive [-o PATH]` | Zip up the current directory. |

## Mounts & spaces (agent / large-repo workflows)

See [spaces.md](spaces.md) for the full workflow. Quick surface:

| Command | What it does |
| --- | --- |
| `oak mount <owner>/<repo> [dest]` | Mount a repo as a lazy virtual filesystem at `dest` (default `./<repo>`) on a new virtual branch. Runs detached — returns once live. |
| `oak mount` / `oak mount <owner>` | Mount everything you can see / a whole org under `~/oaktree`. |
| `oak mount list` | List active mounts and their virtual branches. |
| `oak mount list --json` | Machine-readable active mount inventory, including mount point, repo, virtual branch, base commit, dirty overlay summary, and unpushed commit count. |
| `oak mount finish [dest] --desc-file <file> [--json]` | Path-based mounted-agent finalization. Reads the branch description from a UTF-8 file or `-` for stdin, commits dirty overlay work if needed, pushes all committed work, then ends the mount without `--force`. `--json` emits one success result with repo, virtual branch, heads, pushed/committed/ended flags, and branch URL. Defaults to the current directory when `dest` is omitted. Failure leaves the mount intact and names the next manual command to run. |
| `oak mount end [dest] [-f]` | Unmount, drop local state, remove the dir. `-f` discards uncommitted overlay. No `dest` = end all under `~/oaktree`. |
| `oak space new <org> [dir]` | Scaffold an Oak space spanning an org's repos. Writes `AGENTS.md`, `CLAUDE.md`, `.claude/settings.json`, and `.oak-space`. A legacy `org/repo` spec is accepted but only the org segment is used. |
| `oak space repos [org]` | List the repos in a space's org. With no org, reads `.oak-space` from the current directory or an ancestor. |
| `oak space clean [dir] [-f]` | Tear down finished (clean) mounts in a space. `-f` also discards dirty ones. |

> Note: older docs may mention `oak mount start` or `oak mount status`. Those
> were removed — use `oak mount <spec> <dest>` and `oak mount list`.

## Agent-facing structured surfaces

Prefer these for automation:

- `oak agent state --json --compact` for the lowest-token preflight document with recommended next commands.
- `oak status --porcelain` or `oak status --short` for compact, stable changed-path rows.
- `oak diff --name-only` for changed paths only; widen to `oak diff --stat` or scoped `oak diff --print PATH` only when needed.
- `oak log --oneline -n N` for compact history.
- `oak status --json` for full machine-readable working-tree status.
- `oak info --json` for repo and current branch metadata.
- `oak log --json` for machine-readable commit history.
- `oak agent state --json` for one full offline preflight document with recommended next commands.
- `oak agent state --json --refresh` when you explicitly want remote freshness fields populated.
- `oak branch --json` / `oak branch list --json` for branch inventory.
- `oak branch list --remote --json` and `oak branch show|diff|review <branch> --remote --json` for checkout-free remote branch triage.
- `oak close <branch> --remote --json` to close a remote branch without switching.
- `oak mount list --json` for active mount inventory.
- `oak finish --json --desc-file <file>` for parseable checkout or in-mount finalization.
- `oak mount finish --json --desc-file <file>` for a parseable path-based mounted-task finalization result.
- `oak conflict status --json` and `oak conflict show --json` for parseable conflict recovery state.

`oak status --json`, `oak status --porcelain`, `oak info --json`, and
`oak log --json` also work inside mounts. Inside mounts they describe the
virtual branch and overlay state instead of a `.oak/` checkout.

## Less common

| Command | What it does |
| --- | --- |
| `oak open` | Open the project on oakvcs.com in a browser. |
| `oak upgrade [-f] [--canary]` | Upgrade the CLI. `--canary` tracks pre-release. |
| `oak maintenance compact` | Compact the local database (VACUUM + compact format). |
| `oak serve [-d DIR] [-p PORT] [--token T]` | Run a minimal self-hosted Oak server (SQLite, optional bearer token). |
| `oak site <enable\|disable\|show\|list>` | Pages-style static hosting for an org. |
| `oak release <new\|list\|show\|edit\|publish\|upload\|delete>` | GitHub-style releases (where enabled). |
