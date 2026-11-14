---
name: oak-vcs
description: >-
  Work with repositories version-controlled by Oak (the `oak` CLI from
  oak.space), which those repos use instead of git. Use this skill for
  ANY version-control operation (status, diff, commit, branch, push, pull,
  merge, clone, log, worktree-style parallel tasks) in a repo tracked by Oak,
  and before starting any task in one (`oak start`).
  An Oak repo has a `.oak/` directory and NO `.git` — if `.git` is missing but
  `.oak/` exists, git commands will fail or corrupt state; use `oak` instead.
  Also use whenever the user mentions Oak, oak.space, `oak mount`, or a
  virtual branch, and when finishing/publishing agent work in
  such a repo.
---

# Oak VCS

Oak is a version control system. It replaces git in repos it
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
3. **Branches parent onto `main` unless you stack them.** `oak switch -c`
   always branches off `main`. To build on unmerged work, stack explicitly:
   `oak switch -c NAME --stack` (on the current branch) or `--parent BRANCH`.
   Stacks land bottom-up: `oak merge` the bottom branch; the server restacks
   the branches above it onto `main`, and `oak pull` in each brings it up to
   date. `oak merge` on a stacked branch refuses and names the branch to land
   first. Details: [references/commands.md](references/commands.md#stacked-branches).
4. **`oak commit` itself is local, but `oak watch` publishes it.** On a task
   begun with `oak start`, the watch daemon pushes each new commit within
   seconds. Without watch, publishing is explicit: `oak push`,
   `oak commit --push`, or `oak finish`.
5. **Merging is server-side; CI runs on main after it.** `oak merge`
   squash-merges the current branch into its parent on the server. Branch
   pushes don't run CI and nothing waits on it: test locally, then merge as
   soon as you're up to date with main. CI then runs each service (workflow
   file) on main; `oak ci status` shows them. If a service breaks on main,
   fix it on main with a new task. Path rules that need approval can still
   refuse a merge until an owner approves.
   Details: [references/ci-and-merging.md](references/ci-and-merging.md).
   To answer whether a branch already landed, use that reference's "Did this
   branch land?" recipe: remote closure plus squash provenance, never stale
   local open status or branch/main hash equality.
6. **No stash.** `oak switch -c` keeps your dirty files that main didn't
   change and carries them onto the new branch; it refuses (exit 4) when main
   changed one. Add `--clean` to discard them and start from fresh main.
7. **Worktree-style parallelism uses mounts, not clones.** `oak mount`
   presents a repo as a lazy virtual filesystem on its own virtual branch.
   Nothing is downloaded up front, so creating one is quick even for a huge
   repo. Details:
   [references/mounts.md](references/mounts.md).
8. **Clone proof is bounded and structural.** Clone negotiates a metadata-only
   preflight even when credentials are present, then hash-verifies transferred
   objects during pull. Budget-only exhaustion is *inconclusive* (exit `8`),
   not evidence of loss: prefer the recommended `--shallow` retry, or pass the
   explicit `--allow-unverified-integrity` escape hatch. Clone never falls back
   on its own, and confirmed corruption is never waivable.
   Scoped legacy fallback uses `--allow-legacy-scope` and waives only the
   pre-download proof. See [references/commands.md](references/commands.md).
   On capable servers, `oak clone --branch NAME` binds the selected open
   branch to the pull and materializes it directly; older servers retain the
   legacy clone-then-fetch behavior for an unpinned branch.
   For reproducible acquisition, `oak clone --branch NAME --expected-head FULL`
   requires an exact server-bound open branch/head and never falls back or
   adopts a moved head; it remains full-history unless `--shallow` is added.
   `oak clone --detached --expected-head FULL` pins main the same way for a
   read-only checkout: detached HEAD at exactly that head, no personal branch.
   For automation, add `--json` to a named Oak clone. It emits one bounded
   receipt separating requested scope, proof disposition, observed object and
   materialization counts, and explicitly unknown transport facts.
   `oak doctor --verify metadata` is the cheap default diagnostic; physical
   `existence`/`bytes` proofs require authentication and explicit bounded
   budgets. Platform admins can inspect one target with `oak blob info HASH`;
   `--depth N [--branch NAME]` selects a bounded history scope.

## Core loop

```bash
oak status                        # what changed (add --json for machines)
oak diff                          # working tree vs HEAD (--print for stdout)
oak diff --branch                 # the whole branch vs its fork point on main
oak diff --branch --json --hunks  # committed contribution plus local edits; honors hunk/page budgets
oak change capture --json         # immutable local change objects; no commit/ref update
oak change export CAPTURE_ID --output FILE --json  # local self-contained archive; no overwrite/network
oak commit                        # checkpoint (no message; only `oak watch` pushes it for you)
oak desc "add rate limiting"      # set the branch narrative
oak desc --file f.md --json       # receipt: local_saved + remote_synced (true/false/"unknown"/"not_linked")
oak desc --append "follow-up"      # append after a blank line; refuses (exit 5) if local desc is stale vs remote
oak open --print                  # print the branch/repo web URL (no browser); --json adds identity
oak push                          # publish the branch to the server
oak push --json                   # publish with exact branch/head receipt; merge_check reports conflicts with main
oak push --plan --json            # read-only preview of what push would send (no mutation); `transport` says how,
                                  # `publication_blocked` says push would refuse (exit 6 push_capability_unavailable)
oak merge                         # squash-merge into main (test locally first; CI runs on main after)
oak ci status                     # each service on main: passing / failing since <commit> / running
```

Starting work — **begin every task with `oak start`**:

```bash
oak start "fix the auth redirect" # branch from latest main named after the task, description set,
                                  # pushed right away, and `oak watch` started (--json for a receipt)
oak switch -c fix-auth-redirect   # new branch off latest available main (keeps dirty files that main didn't change; refuses (exit 4) when main changed one)
oak switch -c                     # same, with a generated name
oak switch -c --clean             # new branch, discard dirty files, fresh main
oak switch -c part-2 --stack      # new branch stacked on the current branch (not main)
oak switch my-feature             # switch to an existing branch (fetches if remote-only)
oak pull                          # update current branch: fetch it, then merge in parent
oak pull --json                   # same, one receipt: heads before/after, description, convergence
oak pull --branch-only            # refresh only this branch + desc; parent NOT synced (status says so)
oak fetch                         # refresh local main only; touches nothing else
```

Inspecting without switching (checkout-free):

```bash
oak branch list                   # branches (--json available)
oak branch --show-current         # like git branch --show-current
oak diff <branch>                 # a branch's contribution vs its fork point
oak diff <rev> <rev> -- <paths>   # any two revisions; unique hash prefixes ok
oak diff --remote OLD NEW --json  # two exact remote commits (full hashes); --hunks/--print; no ref changes
oak log -n 10 --oneline           # history; -S/-G search like git log
oak log --remote --json -n 20     # remote main's first-parent history: merge_source_branch, feedback_refs
```

Review evidence: `--json --hunks --line-numbers` adds structured `hunks`
(`old_line`/`new_line` per line) and `--print --line-numbers` numbers the
patch. Branch diff/review JSON says what its rows prove with `evidence_kind`
(`snapshot`, `contribution`, `predicted_merge`); in snapshot rows,
`basis: "snapshot_absence"` means "absent on the branch", not "the branch
deleted it". `ancestry_diagnostic` names missing ancestry edges and whether
they can matter for the merge base (`below_proven_base: true|"unknown"`).
When a verified straight ancestry path proves one compared tip is the base,
older ancestry diagnostics are bounded. `residual_scan_complete: false` with
`truncated: true` means gap counts cover only the inspected sample; it does
not mean all old history is present. The merge-base proof itself is complete.
Divergent or uncertain ancestry still requires the full certification walk.

## Live sync: `oak watch`

`oak start` runs `oak watch`, a per-checkout background daemon that keeps the
task branch live while you work: when main moves it checkpoints your dirty
work and merges main in, it pushes every new `oak commit`, and it marks the
branch Active on oak.space. With the Claude Code hooks installed (`oak skill
install` / `oak init` add them to `.claude/settings.json`), it tells you what
happened as extra context after tool calls and prompts:

- **"Merged the latest main ... N file(s) changed: ..."** — re-read those files
  before editing them.
- **"CONFLICT: ..."** — stop and resolve it **immediately**, before anything
  else: `oak conflict status`, edit each listed file to remove the markers,
  then `oak pull --continue` (or `oak pull --abort`). Auto-pull is paused until
  you do; auto-push resumes after.
- **"could not push ..."** — run `oak push` to see the error.

If a command exits 4 with `error.code: "interrupted_worktree_update"` (an
`oak pull` or other tree update was killed part-way), run `oak pull --abort
--json` and read its receipt — never commit around it or delete the record.
See `references/conflicts-and-recovery.md`.

If `oak start`, `oak switch -c` or `oak pull` exits 4 with `error.code` (or a
pull receipt's `parent_sync_error_code`) `"local_changes_would_be_overwritten"`,
a file you changed without committing is one the update also changes (or
something of yours is in the way of it); the paths are listed and nothing was
written. Follow `recommended_next_commands`: for `oak pull`, `oak commit` then
`oak pull` (the overlap is merged or reported as a conflict); for `oak start` /
`oak switch -c`, either keep the edit on the current branch (`oak commit`, then
`oak pull` there) or rerun the same command with `--clean` (discards the edit).
When only `oak status --json` is offered, the path must be moved aside first.

Watch acts on the checkout while you work. When main moves, your uncommitted
edits become a commit (hooks skipped) that is then pushed, so keep secrets and
scratch files out of the tree or in `.oakignore`, and expect files to change
between your tool calls.

Only Claude Code gets these messages automatically. Without the hooks (other
agents, or a clone with none installed) check `oak watch status --json`
between steps: `conflict`, `sync_in_progress`, `last_push_error` and
`push_blocked` carry the same news. `push_blocked` means the server cannot take
this head yet (a capability it has not activated): watch stops resending it and
tries again after your next commit or, with no new commit, every 30 minutes
(`next_retry_at`), so keep committing and do not discard work. `oak clone` does not install hooks; `oak skill install --global`
covers every checkout on the machine. `oak agent state --json` carries the same
under `watch`.

Watch stops itself after 4 hours with no agent activity (`stop_reason:
idle_timeout`; `OAK_WATCH_IDLE_SECS` tunes it, 0 disables) and the hooks restart
it when an agent returns. With the hooks, it keeps running until the last
session in the checkout ends.

```bash
oak watch status --json           # running?, branch, last pull/push, pending conflict, settings
oak watch start                   # (re)start it on the current branch, e.g. after `oak finish`
oak watch stop                    # stop it and mark the branch idle (`oak finish` does this too)
oak config set watch.enabled false            # this checkout; --global for ~/.oak/config.json
oak config list --json            # effective watch.enabled/auto_pull/auto_push and their sources
```

Settings resolve env (`OAK_WATCH`, `OAK_WATCH_AUTO_PULL`, `OAK_WATCH_AUTO_PUSH`)
> repo config > global config > your oak.space preference > default (on).
`watch.share_working` is the exception: off by default, and when on, watch
sends uncommitted text files to the branch page before any commit.
Watch only runs in regular checkouts linked to a remote, never in mounts.

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
| merge a PR | `oak merge` (squash, server-side; CI runs on main after) |
| `git restore <p>` / `git reset --hard` | `oak restore <p>` / `oak reset -f` |
| `git stash` | not needed — `oak switch -c` carries dirty files that main didn't change (refuses, exit 4, when main changed one) |
| `git rebase -i` (reorder/drop/split) | `oak split --plan <file>` |
| `git worktree add` | `oak mount ORG/REPO ./dir` |
| `git clone org/repo` | `oak clone org/repo` (`--shallow`, `--path <prefix>` sparse) |
| `git branch --show-current` | `oak branch --show-current` |
| escape hatch to git | `oak export <dest>` — replays history into a fresh git repo |

`oak restore --source HEAD <p>` is equivalent to omitting `--source`: it
restores from the current branch's HEAD without creating a commit. A full
commit hash selects historical content instead. Noninteractive discards
require `--force`.

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

`oak finish` stops `oak watch`, preflights, sets the branch description,
checkpoints any dirty work, and publishes — retryably. If a phase fails, the JSON names the
completed phase and the exact next command; run it rather than improvising.
Watch stays stopped after a finish that went ahead: run `oak watch start` if you
keep working on the branch. A preflight refusal changes nothing and restarts it.

**Do not `oak merge` your own branch unless the user explicitly asked you to
land it.** Pushing makes work reviewable; merging changes main.

## Groups, path rules and partial checkouts

Who can reach a repo is set by **groups**: an org gives a group Read or
Write on a repo, and everyone in it (and the agent keys made for it) gets
that. `oak groups` lists the groups you're in. A repo can also have **path
rules** (ownership boundaries), set on the server, not in the tree: a
directory, the group that owns it (`@group/web`), whether a change there
needs that group's approval (`required`, `advisory` or `off`), and who may
read and write it. You see everything you can read; directories you
can't read are simply absent. Recipe:

1. `oak access --json` lists the boundaries; `oak owners PATH --json` names
   the one covering a path. Peek with `oak ls|cat|grep --remote` before
   cloning more than you need.
2. For a big repo, take only the directories the task needs:
   `oak clone ORG/REPO --path apps/web --path libs/ui`, or
   `oak mount ORG/REPO --path apps/web --path libs/ui ./<slug>`.
3. If you hit `path_outside_view`, run its `recommended_next_commands`
   (`oak sparse add DIR`). Don't work around it by recreating files.
4. Before finishing, run `oak owners --branch --json` and name the owner
   groups whose approval is needed in the branch description and your final
   report.
5. Finish as usual. Never try to approve your own branch.

Details: [references/commands.md](references/commands.md#groups-and-path-rules).

## Rules for agents

- Prefer `--json` on `status`, `diff`, `log`, `commit`, `push`, `merge`, `ci`,
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
- Feedback administrators should read `oak feedback show ITEM --json` before
  `oak feedback transition ITEM --expected-revision TOKEN ...`. The latter
  requires server capability activation, never falls back to unconditional
  writes, and never retries conflicts or UNCONFIRMED outcomes automatically.
  An omitted capability means unavailable, not malformed; JSON errors identify
  whether a transition was attempted and whether its outcome is unconfirmed.
  Refresh and reconcile first; details are in the command reference.
  Feedback list/show/link/unlink also return standard JSON error envelopes
  with `--json`; their established numeric exit codes remain unchanged.
- Start each task with `oak start "<summary>"` and end it with `oak finish`.
  If `oak watch` reports a CONFLICT, resolving it is your next action.
- A conflicted or half-finished operation (merge, pull) is never a dead end:
  `oak conflict status` tells you where you are, and every in-progress
  operation has `--continue` / `--abort`. See
  [references/conflicts-and-recovery.md](references/conflicts-and-recovery.md).

## Exit codes

`0` success · `1` generic failure · `2` usage · `3` repo locked ·
`4` dirty working tree blocked the operation · `5` conflicts ·
`6` network/server/auth · `7` merge prediction uncertified ·
`8` integrity proof inconclusive (budget ran out; not evidence of missing
content). (`oak ci status`: `0` passed, `1` failed or no runs,
`3` still running. `oak ci wait`: `0` passed, `1` failed, `3` timed out
(or, with `--current`, no run dispatched yet for the head);
an absent requested run or a head mismatch fails closed with `6`.
`oak ci publish`: `0` authorized, `1` not ready/declined/run failed,
`2` needs `--yes`, `3` wait timed out, `6` refused or caller can't approve.)

## Reference files — read when the task needs them

- [references/commands.md](references/commands.md) — full command and flag
  reference for every subcommand. Read when you need a flag not shown above.
- [references/mounts.md](references/mounts.md) — lazy
  virtual-filesystem mounts, the mount finish saga, and Claude Code
  worktree-hook integration. Read before any `oak mount` work or when
  working inside a mount.
- [references/ci-and-merging.md](references/ci-and-merging.md) — the CI merge
  gate, `oak ci` subcommands, waiting out or bypassing the gate, and
  checkout-free branch review. Read before merging or when a merge is
  refused.
- [references/json-and-automation.md](references/json-and-automation.md) —
  the machine-readable output contract, `oak agent state`, porcelain output,
  env vars (`OAK_REMOTE`, `OAK_API_KEY`, `OAK_NO_UPDATE_CHECK`, ...; `oak environment
  --json` lists all), scripting patterns.
- [references/conflicts-and-recovery.md](references/conflicts-and-recovery.md)
  — resolving conflicts, aborting/continuing operations, discarding or
  restoring work, force pull, splitting branches, exporting to git.

Run `oak <command> --help` for anything not covered — the help text is
thorough. Docs: https://oak.space/docs
