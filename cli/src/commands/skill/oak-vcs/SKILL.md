---
name: oak-vcs
description: >-
  Version control in repos tracked by Oak (the `oak` CLI from oak.space)
  instead of git. Use for ANY version-control step in such a repo (status,
  diff, commit, branch, push, pull, merge, clone, log, parallel checkouts),
  before starting or finishing a task there, and whenever the user mentions
  Oak or oak.space. An Oak repo has a `.oak/` directory and no `.git`; git
  commands fail there, so use `oak`.
---

# Oak VCS

Oak replaces git in the repos it tracks. The model is close to git but
simpler, and several git habits don't apply. Read the differences before
running anything.

**Never run `git` in an Oak repo.** It has `.oak/` at its root and no `.git`.
Check with `ls .oak` or `oak status`.

## How Oak differs from git

1. **No staging area.** `oak commit` checkpoints every change, or only the
   given paths (`oak commit src/ docs/README.md`).
2. **Commits carry no message.** `oak commit -m` is refused. The **branch
   description** is the narrative: `oak desc "..."`, `oak desc --file F`
   (`-` for stdin). It becomes the squash-merge message on main.
3. **Branches are flat: every branch parents onto `main`.** `oak switch -c`
   and `oak start` always branch off `main`, and a branch lands on `main`.
   To break a large branch into pieces that land separately, use `oak split`
   ([references/conflicts-and-recovery.md](references/conflicts-and-recovery.md#untangling-a-branch-oak-split)).
4. **Commits are local until published** with `oak push`,
   `oak commit --push` or `oak finish`.
5. **Merging is a server-side squash** (`oak merge`), and nothing runs on the
   server: test locally first. Teams record what passed by moving **marks**,
   named pointers on main (`oak mark move server-good <commit>`), and deploy
   machines follow marks. Path rules can require an owner's approval before
   a merge. There is no `oak ci` or `oak workflow`. See
   [references/merging.md](references/merging.md).
6. **No stash.** `oak switch -c` carries dirty files that main didn't change
   onto the new branch, and refuses (exit 4) when main changed one. `--clean`
   discards them and starts from fresh main.
7. **Parallel tasks use shallow clones**, one per task:
   `oak clone ORG/REPO --shallow ./<slug>` (add `--path DIR` to take only some
   directories).

## Start, work, finish

```bash
oak start "fix the auth redirect"   # every task starts here: branch from main's head, described, pushed
oak status --json --compact         # what changed
oak diff --print                    # working tree vs HEAD as text
oak diff --branch --stat            # what the whole branch changes vs main
oak commit                          # local checkpoint (no message, never pushes)
oak pull                            # fetch this branch, then merge in main
oak log --remote --json -n 20       # remote main's history
```

Finish by writing the description to a file and publishing in one step:

```bash
cat > /tmp/oak-desc.txt <<'EOF'
What this branch does and why, in a few sentences.
EOF
oak diff --branch --stat            # confirm it lists only your files
oak finish --desc-file /tmp/oak-desc.txt --json
```

`oak finish` preflights, sets the description, checkpoints dirty work and
publishes. If a phase fails, its JSON names the exact next command: run that.

**Don't `oak merge` your own branch unless the user asked you to land it.**
Pushing makes work reviewable; merging changes main.

## Staying up to date

Run `oak pull` when `oak status` says main has moved. If it reports a
**CONFLICT**, resolving it is your next action: `oak conflict status`, edit
each listed file to remove the markers, then `oak pull --continue` (or
`oak pull --abort`).

If `oak start`, `oak switch -c` or `oak pull` exits 4 with `error.code`
`local_changes_would_be_overwritten`, an uncommitted edit of yours touches a
file the update changes. Nothing was written. Follow
`recommended_next_commands`. Exit 4 with `interrupted_worktree_update` means
an earlier update was killed part-way: run `oak pull --abort --json`, never
commit around it. Details:
[references/conflicts-and-recovery.md](references/conflicts-and-recovery.md).

## git → oak

| git | oak |
|---|---|
| `git add` + `git commit -m "msg"` | `oak commit`, then `oak desc "msg"` |
| `git switch -c foo` | `oak switch -c foo` |
| `git diff` | `oak diff --print` |
| `git diff main...HEAD` | `oak diff --branch` |
| `git log -S term` | `oak log -S term` |
| `git push` | `oak push` (first push of a new repo: `oak push --repo ORG/REPO`) |
| `git pull` / `git fetch` | `oak pull` / `oak fetch` (main only) |
| merge a PR | `oak merge` (squash, server-side) |
| `git restore p` / `git reset --hard` | `oak restore p -f` / `oak reset -f` |
| `git stash` | not needed: `oak switch -c` carries dirty files |
| `git rebase -i` | `oak split --plan FILE` |
| `git worktree add` | `oak clone ORG/REPO --shallow ./dir` |
| `git branch --show-current` | `oak branch --show-current` |
| look at a branch without checking it out | `oak diff BRANCH`, `oak branch review BRANCH --json` |
| escape hatch | `oak export DEST` replays history into a git repo |

## Groups, path rules and partial checkouts

Repo access comes from **groups**. A repo can also have **path rules** on
directories: an owner group, whether changes there need its approval, and who
may read or write. Directories you can't read are simply absent.

- `oak access --json` lists the rules; `oak owners PATH --json` names the one
  covering a path. `oak ls|cat|grep --remote` peeks without cloning.
- On `path_outside_view`, run its `recommended_next_commands`
  (`oak sparse add DIR`). Don't recreate the files by hand.
- Before finishing, `oak owners --branch --json` names the groups whose
  approval is needed. Put them in the description and your report. Never try
  to approve your own branch.

Details: [references/commands.md](references/commands.md#groups-and-path-rules).

## Rules for agents

- Prefer `--json`. Every payload has `recommended_next_commands` with exact
  invocations, and every failure is one JSON document with an `error.code`.
  Run a recommended command rather than guessing flags. Contract:
  [references/json-and-automation.md](references/json-and-automation.md).
- Always pass the non-interactive form. Bare `oak diff` opens a browser UI in
  a terminal (use `--print`, `--stat` or `--json`); `oak switch` needs a name,
  `oak clone` a repo, `oak split` a `--plan`.
- `oak reset` and `oak restore` ask for confirmation. Pass `-f` only after
  `oak status` shows what you'd discard.
- Lost? `oak agent state --json --compact` prints the repo's state and the
  next useful actions.
- A half-finished merge or pull is never a dead end: `oak conflict status`,
  then `--continue` or `--abort`.
- In a clone that's behind main, check `oak diff --branch --stat` before every
  push. It should list only your files.

## Exit codes

`0` ok · `1` failure · `2` usage · `3` repo locked · `4` dirty tree blocked
it · `5` conflicts · `6` network/server/auth · `7` merge prediction
uncertified · `8` integrity proof inconclusive (retry with `--shallow`; not
data loss).

## Reference files: read when the task needs them

- [references/commands.md](references/commands.md): every command and flag,
  including clone integrity options, marks, sites, roots, retention and
  feedback administration.
- [references/merging.md](references/merging.md): merging, approvals, marks,
  checkout-free review, and "did this branch land?". Read before merging or
  moving a mark.
- [references/json-and-automation.md](references/json-and-automation.md):
  the JSON contract, `oak agent state`, environment variables
  (`oak environment --json` lists them), scripting.
- [references/conflicts-and-recovery.md](references/conflicts-and-recovery.md):
  conflicts, aborting and continuing, discarding work, `oak split`, export to
  git.

`oak <command> --help` covers anything else. Docs: https://oak.space/docs
(all pages in one file: https://oak.space/docs/llms-full.txt).
