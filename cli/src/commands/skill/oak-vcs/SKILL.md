---
name: oak-vcs
description: >-
  Work with repositories version-controlled by Oak (the `oak` CLI from
  oak.space) — a git replacement built for coding agents. Use this skill for
  ANY version-control operation (status, diff, commit, branch, push, pull,
  merge, clone, log, worktree-style parallel tasks) in a repo tracked by Oak.
  An Oak repo has a `.oak/` directory and NO `.git` — if `.git` is missing but
  `.oak/` exists, git commands will fail or corrupt state; use `oak` instead.
  Also use whenever the user mentions Oak, oak.space, `oak mount`, an "Oak
  space", or a virtual branch, and when finishing/publishing agent work in
  such a repo.
---

# Oak VCS

Oak is version control at the speed of agents. It replaces git in repos it
tracks. The mental model is close to git but simpler, and several habits from
git actively don't apply — read the differences below before running anything.

**Never run `git` in an Oak repo.** An Oak repo has a `.oak/` directory at its
root and no `.git`. Check with `ls .oak` or `oak info`. (`git init` in an Oak
repo creates a second, conflicting source of truth; `git status` just fails.)

## How Oak differs from git

1. **No staging area.** There is no `add`/index. `oak commit` checkpoints
   every change in the working tree, or only the given paths
   (`oak commit src/ docs/README.md`).
2. **Commits carry no message.** `oak commit -m` is refused. The **branch
   description** is the narrative of the work: set it with
   `oak desc "..."` or `oak desc --file <f>` (or `-` for stdin). It becomes
   the squash-merge message when the branch lands on main.
3. **Branches are flat.** Every branch parents directly onto `main`. You
   cannot stack a branch on another branch.
4. **Commits are local until you publish.** `oak commit` never pushes.
   Publishing is explicit: `oak push`, `oak commit --push`, or `oak finish`.
5. **Merging is server-side and CI-gated.** `oak merge` squash-merges the
   current branch into its parent on the server. If CI for the branch head is
   running or failed, the server refuses (HTTP 412). Use `oak merge --wait`
   to ride out a running CI. Details: [references/ci-and-merging.md](references/ci-and-merging.md).
6. **No stash.** `oak switch -c` keeps your dirty files and carries them onto
   the new branch; add `--clean` to discard them and start from fresh main.
7. **Worktree-style parallelism uses mounts, not clones.** `oak mount`
   presents a repo as a lazy virtual filesystem on its own virtual branch —
   near-instant to create even for huge repos. Details:
   [references/mounts-and-spaces.md](references/mounts-and-spaces.md).

## Core loop

```bash
oak status                        # what changed (add --json for machines)
oak diff                          # working tree vs HEAD (--print for stdout)
oak diff --branch                 # the whole branch vs its fork point on main
oak commit                        # local checkpoint (no message, never pushes)
oak desc "add rate limiting"      # set the branch narrative
oak push                          # publish the branch to the server
oak merge --wait                  # squash-merge into main once CI passes
```

Starting work:

```bash
oak switch -c fix-auth-redirect   # new branch off latest available main (keeps dirty files)
oak switch -c                     # same, with a generated name
oak switch -c --clean             # new branch, discard dirty files, fresh main
oak switch my-feature             # switch to an existing branch (fetches if remote-only)
oak pull                          # update current branch: fetch it, then merge in parent
oak fetch                         # refresh local main only; touches nothing else
```

Inspecting without switching (checkout-free):

```bash
oak branch list                   # branches (--json available)
oak branch --show-current         # like git branch --show-current
oak diff <branch>                 # a branch's contribution vs its fork point
oak diff <rev> <rev> -- <paths>   # any two revisions; unique hash prefixes ok
oak log -n 10 --oneline           # history; -S/-G search like git log
```

## git → oak translation

| git habit | oak equivalent |
|---|---|
| `git status` | `oak status` |
| `git add` | nothing — no staging area |
| `git commit -m "msg"` | `oak commit`, then `oak desc "msg"` for the branch |
| `git checkout -b foo` / `git switch -c foo` | `oak switch -c foo` |
| `git diff` | `oak diff --print` (bare `oak diff` opens an interactive browser) |
| `git diff main...HEAD` | `oak diff --branch` |
| `git log` / `git log -S term` | `oak log` / `oak log -S term` |
| `git push` | `oak push` (first push of a new repo: `oak push --repo ORG/REPO`) |
| `git pull` | `oak pull` |
| `git fetch` | `oak fetch` |
| merge a PR | `oak merge` (CI-gated, squash, server-side) |
| `git restore <p>` / `git reset --hard` | `oak restore <p>` / `oak reset -f` |
| `git stash` | not needed — `oak switch -c` carries dirty files |
| `git rebase -i` (reorder/drop/split) | `oak split --plan <file>` |
| `git worktree add` | `oak mount ORG/REPO ./dir` |
| `git clone org/repo` | `oak clone org/repo` (`--shallow`, `--path <prefix>` sparse) |
| `git branch --show-current` | `oak branch --show-current` |
| escape hatch to git | `oak export <dest>` — replays history into a fresh git repo |

## Finishing a task (agents)

When your work on a branch is done, leave it reviewable and server-visible.
The one-shot way:

```bash
cat > /tmp/oak-desc.txt <<'EOF'
What this branch does and why, a few sentences. This becomes the
squash-merge message.
EOF
oak finish --desc-file /tmp/oak-desc.txt --json
```

`oak finish` preflights, sets the branch description, checkpoints any dirty
work, and publishes — retryably. If a phase fails, the JSON names the
completed phase and the exact next command; run it rather than improvising.

**Do not `oak merge` your own branch unless the user explicitly asked you to
land it.** Pushing makes work reviewable; merging changes main.

## Rules for agents

- Prefer `--json` on `status`, `diff`, `log`, `commit`, `merge`, `ci`,
  `finish`, `agent state`. Payloads include `recommended_next_commands` with
  exact invocations for the natural next step — run one of those instead of
  guessing flags. Full contract: [references/json-and-automation.md](references/json-and-automation.md).
- Some bare commands are interactive and will hang without a TTY:
  `oak diff` (browser UI — use `--print`, `--stat`, or `--json`),
  `oak switch` with no name, `oak clone` with no repo, `oak split` without
  `--plan`. Always pass the non-interactive form.
- `oak reset` and `oak restore` prompt for confirmation; pass `-f` when
  running unattended — but only after `oak status` confirms what you are
  discarding.
- Lost? `oak agent state --json --compact` prints one document with the
  repo's current state and the next useful actions.
- A conflicted or half-finished operation (merge, pull) is never a dead end:
  `oak conflict status` tells you where you are, and every in-progress
  operation has `--continue` / `--abort`. See
  [references/conflicts-and-recovery.md](references/conflicts-and-recovery.md).

## Exit codes

`0` success · `1` generic failure · `2` usage · `3` repo locked ·
`4` dirty working tree blocked the operation · `5` conflicts ·
`6` network/server/auth. (`oak ci status`: `0` CI passed, `1` failed or no
runs, `3` still running.)

## Reference files — read when the task needs them

- [references/commands.md](references/commands.md) — full command and flag
  reference for every subcommand. Read when you need a flag not shown above.
- [references/mounts-and-spaces.md](references/mounts-and-spaces.md) — lazy
  virtual-filesystem mounts, Oak spaces (org-wide multi-task directories),
  the mount finish saga, and Claude Code worktree-hook integration. Read
  before any `oak mount` / `oak space` work or when working inside a mount.
- [references/ci-and-merging.md](references/ci-and-merging.md) — the CI merge
  gate, `oak ci` subcommands, waiting out or bypassing the gate, and
  checkout-free branch review. Read before merging or when a merge is
  refused.
- [references/json-and-automation.md](references/json-and-automation.md) —
  the machine-readable output contract, `oak agent state`, porcelain output,
  env vars (`OAK_REMOTE`, `OAK_REPO`, `OAK_DIFF_TOOL`), scripting patterns.
- [references/conflicts-and-recovery.md](references/conflicts-and-recovery.md)
  — resolving conflicts, aborting/continuing operations, discarding or
  restoring work, force pull, splitting branches, exporting to git.

Run `oak <command> --help` for anything not covered — the help text is
thorough. Docs: https://oak.space/docs
