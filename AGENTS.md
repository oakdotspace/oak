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
oak diff --branch --json --hunks # same local contribution scope, with bounded/paged JSON options
oak diff <branch>   # checkout-free branch diff (contribution view; --mode tree|contribution|net-merge, --against <base>)
oak diff <rev> <rev>          # diff two branches/commits (unique hash prefixes ok; paths after `--`)
oak diff <branch> --json      # per-file summary JSON; add --hunks for patches, --max-bytes <n> to bound them
oak diff <branch> --json --hunks --line-numbers # + structured hunks with old_line/new_line per line (--print --line-numbers numbers text)
oak diff --remote OLD NEW --json [--hunks]      # two exact remote commits (full hashes), checkout-free; no ref changes
oak branch train A B --against main --json # cumulative read-only sibling preview; validate each candidate
oak diff --exit-code          # exit 1 when differences exist, 0 when none (like git)
oak diff --check [--json]     # whitespace errors / conflict markers on added lines; exit 1 when found
oak diff --json --fingerprint # append read-only change_id (= `oak change capture` id) + base; also on status --json
oak diff --verify-fingerprint ID # exit 1 unless the working tree still has that change_id
oak commit          # checkpoint (no message — no -m); `oak watch`, not commit, pushes it
oak commit --push   # checkpoint, then publish the branch
oak commit <paths>  # checkpoint only the changes under the given files/directories
oak change capture --json [paths...] # store immutable local change objects without committing
oak log             # show commit history
oak log --remote --json -n 20 # remote main first-parent history (verified; merge_source_branch, feedback_refs; --from continues)
oak push            # publish the current branch to the remote
oak push --json     # publish and return the exact branch/head receipt; merge_check reports conflicts with main
oak push --plan --json # read-only: outgoing commits/edges + blobs the server already has, via push's planner
oak pull            # bring the current branch up to date
oak pull --json     # same, one receipt on stdout: heads, parent heads, description, convergence
oak pull --branch-only # refresh only this branch + description; skips parent sync (status shows parent_sync_deferred)

oak start "task summary"           # start every task here: branch from main, described, pushed, `oak watch` running
oak watch status --json            # live-sync daemon: merges main in, pushes commits, reports conflicts via hooks
oak config set watch.enabled false # turn watch off here (--global: ~/.oak/config.json; env OAK_WATCH=0)
oak config set watch.share_working true # show uncommitted text files on the branch page (off by default)
oak switch -c                      # create a generated branch from latest available main (keeps dirty files)
oak switch -c my-feature           # create a named branch from latest available main (keeps dirty files)
oak switch -c --clean              # create a clean generated branch, discarding dirty files
oak switch -c part-2 --stack       # stack a new branch on the current branch (lands after it)
oak desc "what this branch does"   # set the current branch's description
oak desc --file F --json           # description receipt: local_saved, remote_synced (true/false/"unknown"/"not_linked")
oak desc --append "more" --json    # append after a blank line; refuses (description_stale + state; exit 5, or 6 if remote unreadable) when local is pending/stale
oak open --print                   # print the branch (or repo) web URL without launching a browser
oak repo list --json               # repositories visible to you on the remote (owner, name, full_name)
oak repo visibility [public|private] # show or change whether this repo is public (--repo ORG/REPO for another)
oak site create NAME --dir public  # Oak Pages: serve a folder of main at https://NAME.oak.page/ (list|show|set|rename|rm|deploys|redeploy|rollback|check)
oak release create v1.2.0 dist/* --title T --notes-file F --json # tag main + attach assets (`oak tag` is an alias)
oak release list|view NAME [--offline] --json # tags/releases; pull syncs metadata, never asset bytes
oak release download v1.2.0 [ASSET...] -d DIR --json # fetch assets on demand, verified by content hash
oak auth status --json             # credential source (never the secret), identity, admin reachability
oak mcp [--allow-write]             # MCP server over stdio: read-only tools; the flag adds commit, desc, push. Never merges or closes
oak access --json                   # ownership boundaries (server-side): dir, owner team, approval, readers/writers
oak owners PATH --json              # the deepest boundary covering a path
oak owners --branch --json          # boundaries the branch touches + approval state (also on finish/merge --json)
oak team list|show|create|add|remove --json # the org's teams (nested: sub-team members count for the parent)
oak clone acme/mono --path apps/web --path libs/ui # only those directories (a view where the server has boundaries)
oak ls|cat|grep --remote ... --json # peek at paths outside your checkout (read-only, permission-filtered)
oak mount acme/mono --path apps/web --path libs/ui ./task # one partial mount holding both directories
oak sparse show|add|remove [PATH] --json # show or widen/narrow a partial checkout (a view refuses with local work)
oak repo move acme/web --into acme/mono:apps/web --owner-team web # move a repo, with history, into a directory
oak switch                         # pick a branch interactively
oak switch my-feature              # switch to an existing branch (fetched from the remote when not local)
oak merge                          # merge the current branch into main (CI-gated server-side)
oak merge --wait                   # wait for CI on the branch head to conclude, then merge (up to 30 min; --wait N for N minutes)
oak queue add [--wait] --json      # repos with a merge queue: queue instead of merging; lands when the combined tree passes
oak queue status --json            # queued entries, trains under test, recent landed/ejected results

oak ci status                      # CI state for the current branch head — what the merge gate checks
oak ci wait RUN_ID [RUN_ID...]     # wait read-only for exact runs; add --commit HASH to fence the head
                                   # (exit 0 success, 1 failure/no runs, 3 still running)
oak ci wait RUN_ID --commit HASH --summary --json # compact metadata; no embedded logs, explicit detail commands
oak ci wait --current --json --summary # resolve the checkout head once, bind its runs, wait
                                   # (--events: JSONL step transitions on stderr; stdout stays one document)
oak ci runs [--limit N]            # list recent CI runs (id, branch, commit, status, duration)
oak ci logs <run-id>               # a run's step-by-step logs
oak ci trigger --expected-commit HASH --idempotency-key KEY --branch BRANCH --json
                                   # exact-head ordinary CI; requires ordinary_trigger_v1; reuse key and full request on retry
oak ci rerun <run-id>              # legacy rerun; reports all IDs, but older servers may use current branch head
oak ci cancel RUN_ID --commit HASH --json
                                   # exact ordinary push/merge run only; never branch-latest, manual, protected, or release
oak ci cancel --superseded [--yes] --json
                                   # dry-run list (or cancel with --yes) older in-flight push runs on this branch
oak ci approve RUN_ID --commit HASH --json
                                   # release a run held because its author can't read every path
oak ci publish -e CHANNEL [--commit HASH] [--yes|--dry-run] [--wait[=MIN]] --json # preview, then authorize a publish (`deploy` is an alias)

oak feedback show fb-165 --json    # admin-only redacted detail; revision is optional on older servers
oak feedback transition fb-165 --expected-revision TOKEN --status in_progress --json
                                   # requires globally enabled feedback_transition_v1; never substitutes a newer token
```

For feedback transitions, pass `--notes TEXT` or `--clear-notes` only when that
edit is intended. A 412 means refresh and reconcile; an UNCONFIRMED outcome may
have committed. Never automatically retry or overwrite using a refreshed token.
The CLI neither activates the server feature nor falls back to legacy PATCH.

Branches parent onto `main` unless stacked: `oak switch -c NAME --stack` (or
`--parent BRANCH`) builds on unmerged work. Stacks land bottom-up: merge the
bottom branch, and the server restacks the rest onto `main`. Commits carry no message — the **branch description**
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

## Parallel tasks with mounts

An `oak mount` fills the same role a git worktree does for parallel agent
work — an isolated, concurrent checkout on its own virtual branch — **without
a full clone behind it.** Each mount is a lazy, content-defined view: file
content hydrates on demand instead of being copied up front, so starting a
task is near-instant and cheap on disk, even for large or binary-heavy repos.
One task = one mount = one virtual branch.

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

Other agents that support create/remove worktree hooks work the same
way — point the "create" hook at `oak mount worktree-create <owner>/<repo>`
(it reads the worktree path from stdin JSON and prints it back) and the
"remove" hook at `oak mount worktree-remove`.

## Working in a mount

The per-task workflow, in short:

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
after publish succeeds. It is a multi-leg saga, not a rollback-atomic
transaction. Explicitly rejected legs can name a manual retry, but an
unconfirmed publication has no retry command: use `oak agent state --refresh
--json --compact` to inspect its durable local operation record before deciding
what to do. From outside the mount, use
`oak mount finish ./<slug> --desc-file <file> --json`.

Use `oak mount list` to see active mounts and their virtual branches, and
`oak mount end <dir>` to tear one down.

## Working on the Oak source in this repo

This repository *is* the Oak CLI. When changing it:

- It's an Oak repo — use `oak`, never `git` (see the rules at the top).
- Do not mutate canonical checkouts such as `/Users/mrmrs/o/oak`,
  `/Users/mrmrs/o/benchmarks`, or `/Users/mrmrs/o/oakspace`. Work in an
  isolated `worker-mrmrs-*` checkout and report the full worker path with
  every branch, commit, and validation result.
- The `oak-vcs` agent skill (what `oak skill install` writes into
  `.claude/skills/`) is canonical in
  [cli/src/commands/skill/oak-vcs/](cli/src/commands/skill/oak-vcs/) and
  `include_str!`'d into the binary — keep it in sync when CLI flags or
  workflows change. Its eval suite lives next to it (`evals/`, not shipped).
- Build with `cargo build -p oakvcs-cli`; the package is `oakvcs-cli`,
  not `oak-cli`.

### Starting a task

Begin with `oak start "<summary>"`. `oak watch` then merges main in as it
moves and pushes each commit. This repo ignores `.claude/`, so run
`oak skill install --global` once to get its hooks; without them, check
`oak watch status --json` for conflicts between steps.

### Finishing a task

Before handing work back, leave the repo in a reviewable, server-visible
state:

```bash
oak status --json --compact
oak commit --json --quiet
oak finish --desc-file /tmp/oak-branch-desc.txt --json
```

`oak commit` does not publish by itself, though a running `oak watch` pushes
each commit. `oak finish` stops watch unless its preflight refuses; run
`oak watch start` if work continues after it. `oak finish
--json` preflights before mutating, stages the branch description, publishes
once, and reports any stopped phase as machine-readable JSON. If publication
is unconfirmed, run `oak agent state --refresh --json --compact`; its retained
operation remains unacknowledged even when the exact desired state is now
visible remotely. Use
`oak commit --push --json --quiet` only when you explicitly want a
checkpoint-plus-publish before final finish. Do not merge your own branch
unless the user explicitly asks for a safe-merge/land flow.
