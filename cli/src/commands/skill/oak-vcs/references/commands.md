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
- `--shallow` — only the newest commit on the selected branch (the default
  branch when `--branch` is omitted). Its working tree remains complete, but
  local history plus integrity-proof and recovery scope are narrower.
- `--path PREFIX` — sparse clone: check out only files under these prefixes
  (repeatable or comma-separated); manage later with `oak sparse`
- Clone first negotiates a bounded metadata proof. Authentication grants repo
  access but never silently upgrades this to physical object probing; pull
  independently verifies every transferred hash.
- `--allow-unverified-integrity` — proceed only when that bounded proof stops
  on its typed history/tree/path budget. It never overrides corruption.
- `--allow-legacy-scope` — when an accessible legacy server lacks proof
  capabilities, waive only the pre-download proof for `--shallow`, `--branch`,
  or `--path`; the pull request still carries the scope and verifies hashes.
- `-r, --remote URL` — server URL

### oak login / logout / whoami [-r URL]
Authenticate against an Oak server; `whoami` prints the logged-in username.

### oak doctor --repo ORG/REPO [-r URL] --verify metadata [--json]
Inspect remote commit/tree/blob/mapping integrity. `--verify metadata` is the
cheap structural mode. Physical `existence`/`bytes` modes require login;
`--verify bytes` additionally requires `--depth` and an explicit
`--max-chunks` or `--max-bytes`. JSON may include non-fatal operator
`advisories` for safely readable legacy trees above current write policy.

### oak blob info HASH --repo ORG/REPO [-r URL] [--depth N [--branch NAME]] [--json]
Platform-admin, target-only byte evidence. It is deliberately bounded at
100,000 chunks / 256 MiB and reports truncation instead of adding an unbounded
bypass; unrelated reachable blobs do not consume the target byte budget.
Target history is bounded by the server's advertised profile (modern servers
cover up to 100,000 reachable commits). If an older server's smaller history
window is the only truncation, the command succeeds and prints the proven
scope; pass `--depth N` (and optionally `--branch NAME`) to choose an explicit
bounded primary-chain scope.

## Snapshot changes

### oak status
- `--json` — machine-readable; `--compact` — bounded JSON with recall metadata
- `--porcelain` / `-s, --short` — stable compact changed-path rows (like
  `git status --short`)
- `--reconcile` — apply pending remote-merge branch reconciliation first

Full JSON retains `changes` for compatibility and also separates
`working_changes` (uncommitted, relative to current HEAD) from
`branch_changes` (committed contribution, exact fork base to branch head).

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
- `--json` — publish and emit the exact branch, pushed head, remote, review
  URL, and next commands as one append-only result (regular checkouts)
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
Send feedback to the Oak team. Non-interactive callers must pass `-m` or
`--file` (`-` for stdin); otherwise the command exits 2. The
`feature-request` alias, optional attribution flags, and submit JSON
`{"id", "ref", "status"}` remain unchanged.

### oak feedback link ITEM / oak feedback unlink ITEM

Admin-only manual links between a feedback item and a branch or commit.
`ITEM` is `fb-165`, `165`, or its raw id.

```bash
oak feedback link fb-165                         # current repo and branch
oak feedback link fb-165 --commit HASH            # commit-only link
oak feedback link fb-165 --branch NAME --commit HASH
oak feedback unlink fb-165 --link-id ID           # unambiguous row
oak feedback unlink fb-165 --commit HASH
oak feedback unlink fb-165 --branch NAME
```

- `--repo ORG/REPO` defaults to the current checkout's repository.
- `link --commit` without `--branch` does not infer the current branch.
- `unlink` accepts only one of `--link-id`, `--commit`, or `--branch`.
  With no selector it uses the current branch. `--link-id` needs no repo;
  commit matching includes branch-plus-commit links. Ambiguity deletes
  nothing and lists exact link IDs to choose from.
- `--remote URL` and `--json` work before or after `link`/`unlink`.
  Child `--remote` takes precedence if both positions are supplied.
- JSON schema v1 includes `status`, `feedback_id`, optional `feedback_ref`,
  `link` (or `removed` and `removed_link_id`), and
  `recommended_next_commands`. Unknown link fields and optional `source`
  are preserved. A link's undo recommendation names its exact link ID.
  Undo/relink recommendations and ambiguity commands retain the effective
  remote, so an override cannot silently revert to the checkout origin.
  Every argument is POSIX-shell quoted when needed; response-derived names
  are data, never executable shell syntax. Parse-failure diagnostics do not
  include response values (including serde type-error excerpts).

Credentials are explicit `OAK_API_KEY`, then the login saved for the effective
remote, never a checkout repository key or another server's login. Missing
credentials fail before network access. URL userinfo, query, and fragment
are stripped before lookup or network use; URL basic-auth is not supported.

Number lookup requires the updated server's `GET /api/feedback?status=all`
so spam-marked items remain addressable. Older servers rejecting that query
fail clearly before any write; there is no incomplete default-list fallback.
A raw item ID bypasses number lookup, but link/unlink still requires the
deployed admin feedback-links API. This command does not activate rollout
flags or prove that production has deployed that API.

Exit codes follow feedback's narrow contract: 0 success, 1 server/network or
ambiguity, 2 usage. Server failures remain on stderr (no success JSON).

### oak completions SHELL / oak upgrade [-f] [--canary]
Shell completions; self-upgrade.

Mount and space commands: see [mounts-and-spaces.md](mounts-and-spaces.md).
CI commands: see [ci-and-merging.md](ci-and-merging.md).
