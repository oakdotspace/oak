---
name: oak
description: >
  How to use Oak version control (the `oak` CLI) instead of Git. Use whenever
  working in an Oak repository — a `.oak/` directory (or `.oak/oak.db`) is
  present, an AGENTS.md says the project "uses Oak", or the user mentions
  `oak commit`/`push`/`pull`/`switch`/`desc`/`merge`/`mount`, an "Oak space",
  or oakvcs.com. Also use when you are about to run a `git` command in a
  project that turns out to use Oak. Covers the flat branch-per-task model,
  messageless commits, push/pull, `oak mount`, and `oak space`.
---

# Using Oak

Oak (<https://oakvcs.com>) is a version control system built for AI-assisted
workflows. If a project uses Oak, **use `oak` for everything version-control
— never `git`.** Running `git` in an Oak repo does nothing useful (there is
no `.git`) and will confuse the user.

**How to tell you're in an Oak repo:** a `.oak/` directory exists at the repo
root, or the project's `AGENTS.md` says it uses Oak.

## The mental model (read this first)

- **Flat branch-per-task.** Every branch is parented directly onto the trunk,
  `main` — you cannot stack a branch on another branch. Each task/session gets
  its own branch; you merge it back into `main` and move on. New clones start
  you on an auto-named personal branch like `zach-3f2a8b`.
- **Commits have no messages.** `oak commit` takes no `-m`. The *branch
  description* is the narrative — it becomes the squash-merge message when
  the branch lands on `main`. Commit freely and often; set a good description
  once. For agent workflows, write multiline descriptions to a temp file and
  run `oak desc --file <file>`; use `oak desc --file -` only when piping the
  text on stdin.
- **Your work is isolated until you publish/merge.** `oak commit` only creates
  a local checkpoint. Nothing is shared until `oak push`, `oak commit --push`,
  or `oak merge`.
- **Large/binary files are first-class** via content-defined chunking — no LFS,
  no special handling.

## Everyday commands

```bash
oak status                 # what's changed in the working tree
oak status --porcelain     # compact stable changed-path rows (regular checkouts)
oak status --short         # git-compatible spelling for --porcelain
oak status --json          # machine-readable status (regular checkouts)
oak info --json            # machine-readable repo/branch metadata
oak agent state --json --compact # lowest-token preflight with recommended_action
oak diff --print [PATHS]   # changes vs HEAD, to stdout, optionally scoped to paths
oak diff --stat            # per-file +added/-removed counts only (cheapest overview)
oak diff --name-only       # changed paths only
oak commit                 # local checkpoint only — NO message, no -m; never pushes
oak commit --push          # checkpoint, then publish the current branch
oak commit --json --quiet [--push] # machine-readable checkpoint/publish state
oak log                    # commit history (-n N to limit, -v for detail)
oak log --oneline          # one compact commit row per line
oak log --json             # machine-readable log

oak push                   # publish the current branch to the server
oak push --repo org/repo   # headless first push: link/create this remote repo
oak pull                   # fetch + bring the current branch up to date

oak switch -c              # create a generated branch from latest available main (keeps dirty files)
oak switch -c my-feature   # create a named branch from latest available main (keeps dirty files)
oak switch -c --clean      # create a clean generated branch, discarding dirty files
oak switch my-feature --clean # switch cleanly, discarding dirty files instead of carrying them
oak switch                 # pick a branch interactively (avoid in headless agent runs)
oak switch my-feature      # switch to an existing branch
oak desc --file /tmp/desc  # set THIS branch's description (the merge message)
oak desc --file -          # same, reading description text from stdin
oak finish --desc-file /tmp/desc # preflight remote, desc + checkpoint-if-needed + publish; mounts end
oak merge                  # merge the current branch into main
oak conflict status --json # inspect merge/pull conflict state
oak conflict show --json   # inspect conflicted paths
oak conflict take path --ours   # choose one side for a checkout conflict

oak mount list --json      # machine-readable active mount inventory
oak mount finish [path] --desc-file /tmp/desc # path-based mount finalization
```

Piped output is compact by design: `oak log` (non-TTY) prints one line per
commit (hash, date, branch, subject) and defaults to the latest 20 — a
trailing `... more commits` line tells you the exact `-n` to widen the
window. Start diff inspection with `oak diff --stat` or scope it
(`oak diff --print src/`); a full-repo diff is the most expensive output you
can request.

Two rules that trip up agents coming from Git:

1. **Never pass `-m` to `oak commit`.** It has no message flag. If you want to
   record intent, use `oak desc --file <file>` for anything multiline or
   generated by an agent. Avoid shell-quoted multiline descriptions.
2. **Don't try to branch off a branch.** `oak switch -c` creates a generated
   branch from latest available `main`; `oak switch -c x` creates a named branch
   parented onto `main`. Both keep dirty files unless `--clean` is passed.
   Oak uses recently refreshed local `main` immediately; otherwise it tries a
   best-effort remote refresh and falls back to local `main` when offline. If
   the current branch already has committed divergence, carrying the worktree
   can make that divergence appear as uncommitted work on the new branch. Use
   `--clean` when you need an exact clean `main` tree or a clean switch to an
   existing branch; it discards current working-tree changes. Bare `oak switch`
   is the human branch picker, so avoid it in headless agent runs. The model
   is flat by design.

To create and publish a brand-new repo non-interactively, use:

```bash
oak init
oak commit
oak push --repo <org>/<repo>
```

`--repo` links the local checkout to the target organization/repo and creates
the server repo on first push if it does not already exist. The left side must
be an existing Oak organization slug, including a user's personal organization.

Plain `oak commit` is local-only; use `oak commit --push` only when you
explicitly want to publish an already-linked repo.

For automation, start with the lowest-output surface that answers the question:
`oak agent state --json --compact` for preflight state and `recommended_action`,
`oak status --porcelain` or `oak status --short` for compact changed-path
rows, `oak diff --name-only` for changed paths only, `oak diff --stat` for
line counts, `oak log --oneline -n N` for compact history, `oak status --json`
for full machine-readable status, `oak info --json` for repo metadata, `oak
agent state --json` for full preflight state and compatibility next commands,
`oak agent state --json --refresh` when you explicitly want remote freshness fields, `oak log --json`
for history, `oak branch
--json` / `oak branch list --json` for local branch inventory,
`oak branch list --remote --json` for remote branch inventory, and
`oak mount list --json` for active mount inventory. Use
`oak branch show|diff|review <branch> --remote --json` to inspect a remote
branch without switching or materializing its files. Close an unneeded remote
branch with `oak close <branch> --remote --json`. `oak status --json`, `oak
status --porcelain`, `oak info --json`, and `oak log --json` apply to regular
checkouts and mounts. In regular checkouts, use `oak commit --json --quiet
[--push]` when you need parseable checkpoint and optional publish state. To finalize work, prefer
`oak finish --desc-file <file>` from inside a checkout or mount; it requires a
linked remote before mutating, then stepwise sets the description, checkpoints
dirty work if needed, publishes, and in mounts ends the mount. It is not a
rollback boundary. From outside a mount, use `oak mount finish [path]
--desc-file <file>`.

When `oak pull`, `oak merge`, or `oak finish --json` reports a conflict, run
`oak conflict status --json` first, then `oak conflict show --json` for
per-path facts. In regular checkouts, `oak conflict take <path> --ours` and
`oak conflict take <path> --theirs` resolve a path by choosing one side inside
each marker block while preserving already-merged hunks.
Do not invent a JSON patch flow; edit files normally or take a side, then run
the recommended `oak pull --continue` or `oak merge --continue`.

## Going further

- For the **full command reference** (clone, init, reset/restore, checkout,
  split — headless commit splitting via `oak split --plan`, export to git,
  mounts, releases, self-hosting), read
  [reference/commands.md](reference/commands.md).
- For **Oak spaces and mounts** — the agent workflow where you mount a repo
  once per task instead of cloning, used for parallel/isolated tasks — read
  [reference/spaces.md](reference/spaces.md). Reach for this when the user
  talks about an "Oak space", running multiple tasks in parallel, or working
  against a very large repo without a full clone.

If a command isn't covered here, `oak <command> --help` is authoritative.
