# AGENTS.md

Guidance for AI coding agents working in this repository.

Oak is **version control at the speed of agents**. This repo *is* its
open-source core: the reusable VCS library plus the `oak` CLI agents drive.
So you're an agent working on the agent-native VCS itself.

## This repo uses Oak, not Git

This project is version-controlled with [Oak](https://oak.space) — **not
Git**. Do not run `git` commands; use `oak` instead.

```bash
oak status          # show changed files
oak diff            # show changes vs HEAD
oak diff --branch   # show the whole branch's diff (commits + dirty files) vs its fork point on main
oak diff <branch>   # checkout-free branch diff (contribution view; --mode tree|contribution|net-merge, --against <base>)
oak diff <rev> <rev>          # diff two branches/commits (unique hash prefixes ok; paths after `--`)
oak diff <branch> --json      # per-file summary JSON; add --hunks for patches, --max-bytes <n> to bound them
oak diff --exit-code          # exit 1 when differences exist, 0 when none (like git)
oak commit          # local checkpoint only (no message — no -m; never pushes)
oak commit --push   # checkpoint, then publish the branch
oak commit <paths>  # checkpoint only the changes under the given files/directories
oak log             # show commit history
oak push            # publish the current branch to the remote
oak push --json     # publish and return the exact branch/head receipt
oak pull            # bring the current branch up to date

oak switch -c                      # create a generated branch from latest available main (keeps dirty files)
oak switch -c my-feature           # create a named branch from latest available main (keeps dirty files)
oak switch -c --clean              # create a clean generated branch, discarding dirty files
oak desc "what this branch does"   # set the current branch's description
oak switch                         # pick a branch interactively
oak switch my-feature              # switch to an existing branch (fetched from the remote when not local)
oak merge                          # merge the current branch into main (CI-gated server-side)
oak merge --wait                   # wait for CI on the branch head to conclude, then merge (up to 30 min; --wait N for N minutes)

oak ci status                      # CI state for the current branch head — what the merge gate checks
oak ci wait RUN_ID [RUN_ID...]     # wait read-only for exact runs; add --commit HASH to fence the head
                                   # (exit 0 success, 1 failure/no runs, 3 still running)
oak ci runs [--limit N]            # list recent CI runs (id, branch, commit, status, duration)
oak ci logs <run-id>               # a run's step-by-step logs
oak ci rerun <run-id>              # re-dispatch a run at the same commit (infra flakes); prints the new run id
```

Branches are flat: every branch parents directly onto `main` (you can't stack
one branch on another). Commits carry no message — the **branch description**
(`oak desc`) is the narrative and becomes the squash-merge message. Your work
is isolated until you `oak push`, `oak commit --push`, or `oak merge`. `oak
switch -c` uses recently refreshed local `main` immediately; otherwise it
tries to refresh `main` and falls back to local `main` if the remote is
unavailable. Run `oak --help` for the full command reference.

> Claude Code reads `CLAUDE.md`, which is just a one-line `@AGENTS.md` import
> pointing here — so this file is the single source of truth.

## Machine-readable output contract

Oak's `--json` output is a stable surface agents may build durable habits
on. The rules:

- **Schemas are append-only within a `schema_version`.** New fields may
  appear at any time; existing fields are never removed or renamed and
  never change meaning without a `schema_version` bump. Parse leniently:
  ignore fields you don't recognize.
- **Absent means default.** Optional fields are omitted when they carry
  their default value (e.g. a missing `category` means `"source"`, a
  missing `binary_or_large` means `false`).
- **Every payload is self-describing.** `recommended_next_commands`
  contains exact invocations for the natural next step — prefer running
  one of those over guessing flags. Paged payloads carry a
  `changed_files_page` block with `next_offset` and a ready-to-run
  `next_page_command`.
- **Budgets are explicit.** Hunk-emitting output honors `--max-bytes`;
  when truncated, the payload says so (`hunks_truncated: true`, per-file
  `patch_omitted: true`) and names the command that fetches the rest.
- **Scripting without parsing:** `oak diff --exit-code` exits `1` when
  differences exist and `0` when there are none (matching `git diff
  --exit-code`), on top of the exit-code contract in `oak --help`.
  Predicted conflicts count as differences.
- **External diff tools:** set `OAK_DIFF_TOOL` to replace the interactive
  browser with your own tool over two materialized trees. The tool must
  block until you are done (the trees are temporary) — e.g.
  `OAK_DIFF_TOOL="code --wait --diff"`.

## What an Oak Space is

An **Oak space** is a directory where an agent mounts a repo once per
task. You create it with:

```bash
oak space new <org> [dir]   # e.g. oak space new acme
```

Inside that directory, **every task gets its own subdirectory**, and each
repo subdirectory is a separate `oak mount` on its own virtual branch. So a
space for the `acme` org might hold:

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
cat > /tmp/oak-finish-desc.txt <<'EOF'
<summary>
EOF
oak finish --desc-file /tmp/oak-finish-desc.txt --json
```

The agent finalizes after every prompt without waiting for confirmation:
`oak finish` preflights the mount, writes the branch description, checkpoints
dirty overlay work if needed, publishes the virtual branch, and ends the mount
after publish succeeds. It is a retryable saga, not a rollback-atomic
transaction; if one leg fails, the JSON names the completed phase and the next
manual command. From the space root, use
`oak mount finish ./<slug>/<repo> --desc-file <file> --json`.

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
- Do not mutate canonical checkouts such as `/Users/mrmrs/o/oak`,
  `/Users/mrmrs/o/benchmarks`, or `/Users/mrmrs/o/oakspace`. Work in an
  isolated `worker-mrmrs-*` checkout and report the full worker path with
  every branch, commit, and validation result.
- The `oak space` command lives in
  [cli/src/commands/spaces/](cli/src/commands/spaces/); its templates
  (`AGENTS.md.tmpl`, `settings.json.tmpl`) are `include_str!`'d into the
  binary, so editing a template changes what `oak space new` writes.
- Likewise the `oak-vcs` agent skill (what `oak skill install` writes into
  `.claude/skills/`) is canonical in
  [cli/src/commands/skill/oak-vcs/](cli/src/commands/skill/oak-vcs/) and
  `include_str!`'d into the binary — keep it in sync when CLI flags or
  workflows change. Its eval suite lives next to it (`evals/`, not shipped).
- Build with `cargo build -p oakvcs-cli`; the package is `oakvcs-cli`,
  not `oak-cli`.

### Finishing a task

Before handing work back, leave the repo in a reviewable, server-visible
state:

```bash
oak status --json --compact
oak commit --json --quiet
oak finish --desc-file /tmp/oak-branch-desc.txt --json
```

`oak commit` is a local checkpoint only; it does not publish. `oak finish
--json` preflights before mutating, stages the branch description, publishes
retryably, and reports any stopped phase as machine-readable JSON. Use
`oak commit --push --json --quiet` only when you explicitly want a
checkpoint-plus-publish before final finish. Do not merge your own branch
unless the user explicitly asks for a safe-merge/land flow.
