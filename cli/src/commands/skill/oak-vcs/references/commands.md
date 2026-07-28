# Oak command reference

Compiled from `oak <command> --help` (v0.101). Global: `oak [--verbose] <command>`.

## Contents

- [Start a repo](#start-a-repo): init, clone, login/logout/whoami
- [Snapshot changes](#snapshot-changes): status, info, diff, commit, restore, reset
- [History](#history): log, hash
- [Branches](#branches): branch, switch, desc, finish, close, split, merge
- [Sync](#sync): push, pull, fetch
- [Sparse checkouts](#sparse-checkouts)
- [More](#more): export, archive, open, feedback, completions, upgrade

## Start a repo

### oak init [PATH]
Initialize a repository in the current (or given) directory.

### oak clone [ORG/REPO] [DEST]
Clone from the server. Bare `repo` defaults to your personal org. Omitting
the repo opens an interactive picker — always pass it when unattended.
- `--branch NAME` — switch to that remote branch after cloning
- `--shallow` — only the most recent commit on the default branch (working
  tree identical; pure download/disk optimization)
- `--path PREFIX` — sparse clone: check out only files under these prefixes
  (repeatable or comma-separated); manage later with `oak sparse`
- `-r, --remote URL` — server URL

### oak login / logout / whoami [-r URL]
Authenticate against an Oak server; `whoami` prints the logged-in username.

## Snapshot changes

### oak status
- `--json` — machine-readable; `--compact` — bounded JSON with recall metadata
- `--porcelain` / `-s, --short` — stable compact changed-path rows (like
  `git status --short`)
- `--reconcile` — apply pending remote-merge branch reconciliation first

### oak info [--json]
Repo/branch metadata (org/repo link, remote, current branch).

### oak agent state [--json] [--compact] [--refresh]
One compact JSON document with current state and the next useful agent
actions. `--refresh` also refreshes remote freshness fields.

### oak diff [REVS] [PATHS]
Up to two leading args naming a branch/commit (unique hash prefix ≥ 4 hex
chars) select endpoints; remaining args (or after `--`) filter paths.
- Bare `oak diff` = working tree vs HEAD, **interactive browser** — use
  `--print`, `--stat`, `--name-only`, or `--json` when unattended
- `oak diff <branch>` — branch contribution vs fork point, checkout-free
- `oak diff <rev> <rev>` — two revisions (default mode `tree`)
- `--branch` — whole current branch (commits + dirty files) vs fork point
- `--mode contribution|tree|net-merge`; `--against BASE` for branch endpoints
- `--json` with `--hunks` adds patches; `--max-bytes N` bounds them
  (truncation is flagged: `hunks_truncated`, per-file `patch_omitted`);
  `--changed-files-limit/-offset` page the summaries
- `--exit-code` — exit 1 when differences exist (predicted conflicts count)
- `-U N` context lines; `--word-diff`; `-a/--text` for oversized text files

### oak commit [PATHS]
Local checkpoint of the whole tree, or only the given paths. Never pushes.
- No `-m` — commits are messageless; the branch description is the narrative
- `--push` — checkpoint, then publish pending branch commits
- `--no-verify` — skip pre-/post-commit hooks
- `--json --quiet` — machine-readable, no human text

### oak restore [PATHS] [-s COMMIT] [-f]
Restore files to HEAD (or `--source` commit) state. `-f` skips confirmation.

### oak reset [PATH] [-f]
Discard uncommitted changes (whole tree, or one path). `-f` skips
confirmation.

## History

### oak log [PATHS]
- `-n N`, `--oneline`, `-v` (changed files), `--json`
- `-S TERM` — commits changing occurrence count of a literal (git log -S)
- `-G PATTERN` — commits whose changed lines match a regex (git log -G)

### oak hash
Print the current HEAD commit hash.

## Branches

### oak branch [list|show|diff|review|triage|rename] [--json]
- bare / `list` — list branches; `--show-current` prints the current name
- `show NAME` — one branch
- `diff NAME` — checkout-free diff summary
- `review NAME` — checkout-free review evidence; `--merge-preview` adds local
  conflict prediction; `--remote` reviews the remote branch without switching
- `triage` — batch analysis over many branches (`--against`, `--status`,
  `--only BUCKET`, `--limit`, `--analysis-depth`)
- `rename OLD NEW`

### oak switch [NAME]
- `NAME` — switch to a branch (fetched from remote when not local), or with
  `-d/--detach`, detach HEAD at a commit hash
- `-c, --create [NAME]` — create off latest available main (generated name if
  omitted); keeps dirty files; falls back to local main if remote unreachable
- `--clean` — discard working-tree changes and start from fresh main
- Bare `oak switch` is an interactive picker — avoid unattended

### oak desc [TEXT] | --file FILE
Set the current branch description (`-` for stdin). This becomes the
squash-merge message.

### oak finish [--desc TEXT | --desc-file FILE] [--json]
Finalize the branch: preflight, save description, checkpoint dirty work,
publish. Retryable saga — on failure the JSON names the completed phase and
the next manual command.

### oak close [NAME]
Close a branch.

### oak split [--from BRANCH] [--plan FILE|-] [--dry-run]
Reorder/drop/split a branch's commits (git rebase -i + histedit split).
Interactive editor by default; `--plan` drives it headless. Plan directives,
one per line (`#` comments); every commit appears exactly once:

```
pick <hash>      # replay (unique prefix ok)
drop <hash>      # omit
split <branch>   # start a new branch off main; later picks go there
```

Picks before the first `split` rewrite the source branch in place. Branches
are flat, so each split segment must stand alone on main — a dependent
segment conflicts and **nothing is written**. Preview with `--dry-run`.

### oak merge [BRANCH]
Squash-merge into the parent, server-side, CI-gated. See
[ci-and-merging.md](ci-and-merging.md) for the gate, `--wait`, `--force`,
`--dry-run --json`, and `--continue`/`--abort`.

## Sync

### oak push
- `-f` — force: overwrite diverged remote history
- `--repo ORG/REPO` (env `OAK_REPO`) — link a fresh repo on first push
  without the interactive org picker (repo is created if missing)
- `-r URL` remote override (env `OAK_REMOTE`)

### oak pull
Fetch the current branch, then merge in the parent branch.
- `-f` — discard local commits not on remote; sync to remote HEAD
- `--continue` / `--abort` — resume/abandon a conflicted parent-sync

### oak fetch
Refresh local `main` only; never touches the working tree or merges.

## Sparse checkouts

### oak sparse [list|set|add|disable] [--json]
Scope the working tree to path prefixes (the "cone"). Out-of-cone files stay
in the repo and ride along in commits, but aren't downloaded or written.
`set` replaces the cone, `add` extends it (both re-sync), `disable` returns
to a full checkout (`oak pull` hydrates never-downloaded files).

## More

### oak export DEST [-b BRANCH] [--git-branch NAME] [-f]
Replay the branch's linear history into a fresh **git** repo, preserving
author + timestamp. The documented escape hatch off Oak.

### oak archive [-o OUTPUT]
Zip archive of the tree.

### oak open
Open the project on the server in a browser.

### oak feedback [-m TEXT|--file FILE]
Send feedback to the Oak team.

### oak completions SHELL / oak upgrade [-f] [--canary]
Shell completions; self-upgrade.

Mount and space commands: see [mounts-and-spaces.md](mounts-and-spaces.md).
CI commands: see [ci-and-merging.md](ci-and-merging.md).
