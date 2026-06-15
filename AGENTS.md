# AGENTS.md

Guidance for AI coding agents working in this repository.

## This repo uses Oak, not Git

This project is version-controlled with [Oak](https://oak.space) — **not
Git**. Do not run `git` commands; use `oak` instead.

```bash
oak status          # show changed files
oak diff            # show changes vs HEAD
oak commit          # snapshot the working directory (no message — no -m)
oak commit <paths>  # commit only the changes under the given files/directories
oak log             # show commit history
oak push            # push the current branch to the remote
oak pull            # bring the current branch up to date

oak switch -c                      # create a generated branch from latest available main (keeps dirty files)
oak switch -c my-feature           # create a named branch from latest available main (keeps dirty files)
oak switch -c --discard            # create a clean generated branch, discarding dirty files
oak desc "what this branch does"   # set the current branch's description
oak switch                         # pick a branch interactively
oak switch my-feature              # switch to an existing branch (fetched from the remote when not local)
oak merge                          # merge the current branch into main
```

Branches are flat: every branch parents directly onto `main` (you can't stack
one branch on another). Commits carry no message — the **branch description**
(`oak desc`) is the narrative and becomes the squash-merge message. Your work
is isolated until you `oak push` or `oak merge`. `oak switch -c` uses recently
refreshed local `main` immediately; otherwise it tries to refresh `main` and
falls back to local `main` if the remote is unavailable. Run `oak --help` for
the full command reference.

> Claude Code reads `CLAUDE.md`, which is just a one-line `@AGENTS.md` import
> pointing here — so this file is the single source of truth.

## What an Oak Space is

An **Oak space** is a directory where an agent mounts a repo once per
task. You create it with:

```bash
oak space new <owner>/<repo> [dir]   # e.g. oak space new acme/blog
```

Inside that directory, **every task gets its own subdirectory**, and each
subdirectory is a separate `oak mount` of the repo on its own virtual
branch. So a space for `acme/blog` might hold:

```
blog/                  # the space (created by `oak space new acme/blog`)
├── AGENTS.md          # how to run tasks in this space
├── CLAUDE.md          # one-line `@AGENTS.md` import (for Claude Code)
├── .claude/
│   └── settings.json  # worktree hooks → oak mount
├── new-page/          # task 1 — a mount on branch new-page--<id>
└── restyle/           # task 2 — a mount on branch restyle--<id>
```

### Why not just use git worktrees?

Spaces fill the same role git worktrees do for parallel agent work —
isolated, concurrent checkouts that don't step on each other — **without
the downsides of a full clone per worktree.** Each mount is a lazy,
content-defined view: file content hydrates on demand instead of being
copied up front, so creating a new task directory is near-instant and
cheap on disk, even for large or binary-heavy repos. One task = one
mount = one virtual branch, all under a single space directory.

### Worktree hook integration

Oak ships `oak mount worktree-create <owner>/<repo>` and
`oak mount worktree-remove` subcommands implementing Claude Code's
[`WorktreeCreate` / `WorktreeRemove`
hooks](https://code.claude.com/docs/en/worktrees). Wired into a
project's `.claude/settings.json`, the Agent tool's
`isolation: "worktree"` and `claude --worktree <name>` transparently get
an `oak mount` (on a fresh virtual branch) instead of a `git worktree`;
on cleanup the mount is torn down only when it holds no uncommitted or
unpushed work — in-flight work is left in place, never discarded.

Note that the `.claude/settings.json` written by `oak space new` does
**not** wire these hooks: a space spans a whole org, and the create hook
needs a fixed `<owner>/<repo>`, which only a single-repo project can pin
down. A space's settings ship agent permissions and a Stop hook instead;
add the worktree hooks yourself in a repo-specific settings file if you
want them.

Other agents that support create/remove worktree hooks work the same
way — point the "create" hook at `oak mount worktree-create <owner>/<repo>`
(it reads the worktree path from stdin JSON and prints it back) and the
"remove" hook at `oak mount worktree-remove`.

## Working in a space

The full per-task workflow ships inside each space as `AGENTS.md`. In
short:

```bash
oak mount <owner>/<repo> ./<slug>   # start a task (detached; returns once live)
cd ./<slug>
# ...edit, then at the end of EVERY prompt, unattended:
oak commit            # finalize the active commit (no message — descriptions are the narrative)
oak push              # publish the virtual branch
oak desc "<summary>"  # set the squash-merge message
cd .. && oak mount end ./<slug>   # tear the mount down — the branch is already pushed
```

The agent finalizes after every prompt without waiting for confirmation:
commit, push, set the description, then end the mount. A follow-up just
remounts (`oak mount <owner>/<repo> ./<slug>`) and finalizes again. A
remount always branches off the trunk, so to build on already-pushed
work, merge it into the trunk first.

To sweep up any mounts left behind, clean every finished mount in the
space at once:

```bash
oak space clean [dir] [--force]
```

`oak space clean` tears down mounts whose working tree is clean (already
committed and pushed/merged). Mounts with in-flight work — uncommitted
changes or commits not yet pushed — are skipped so it is never lost;
`--force` discards and removes those too. Use `oak mount list` to see
active mounts and their virtual branches.

## Working on the Oak source in this repo

This repository *is* the Oak CLI. When changing it:

- It's an Oak repo — use `oak`, never `git` (see the rules at the top).
- The `oak space` command lives in
  [cli/src/commands/spaces/](cli/src/commands/spaces/); its templates
  (`AGENTS.md.tmpl`, `settings.json.tmpl`) are `include_str!`'d into the
  binary, so editing a template changes what `oak space new` writes.
- The shippable agent skill (for global install into Claude / Codex) lives
  in [agent-skills/](agent-skills/).
- Build with `cargo build -p oakvcs-cli`; the package is `oakvcs-cli`,
  not `oak-cli`.
