# Conflicts and recovery

Nothing in Oak is a dead end. Every in-progress operation (merge, pull
parent-sync, mount pull) can be inspected, continued, or aborted, and
history is an escape hatch away from any bad working-tree state.

## Where am I?

```bash
oak conflict status [--json]   # summarizes any in-progress conflicted operation
oak agent state --json --compact  # broader: full state + recommended next commands
```

Exit code 5 from any command means conflicts; 4 means the working tree
blocked the operation — it is dirty (commit or reset, then retry), or an
interrupted update must be recovered first (below); 3 means the repo is
locked by another oak process.

## Uncommitted work in the way of an update

Moving the working tree to another snapshot — `oak start` / `oak switch -c`
onto a `main` that moved, `oak pull` merging the parent in, a conflicted
pull's write-out — writes only the files the two snapshots disagree on. Your
uncommitted edits to other files are carried. When a file the update writes
also holds uncommitted work (an edit, an untracked file at a path the update
adds, a file you deleted that the update changes, an edit to a file the
update deletes), Oak picks neither side: the command exits 4 before writing
anything, with `error.code: "local_changes_would_be_overwritten"` and
`error.local_changes` (`paths[]` of `{path, state}`, `total`, `truncated`).
`oak pull --json` reports it as `parent_sync_error_code` with
`parent_sync_local_changes`.

What resolves it depends on the command; `recommended_next_commands` names
exactly that:

```bash
# oak pull refused:
oak commit            # record your side
oak pull              # now a merge, or a reported conflict

# oak start / oak switch -c refused (the new branch starts from the newer main):
oak commit            # keep the edit on the CURRENT branch ...
oak pull              # ... and merge main into it (a conflict is reported)
oak switch -c work --clean   # or: the same command with --clean — DISCARDS the edit
```

Committing and then re-running `oak switch -c` does not carry the edit: it
stays committed on the old branch. Some things a commit cannot resolve — an
ignored file at a path the update adds, an untracked directory where it writes
a file (or a file where it needs a directory), a name differing only in case
from one it creates on a case-insensitive filesystem. Then only `oak status
--json` is recommended: move the path aside and run the command again. Oak
never deletes such content implicitly, ignored files included.

`oak switch NAME` and a fast-forward of the branch itself still require a
clean tree (`dirty_worktree`, exit 4). `oak pull --abort` after a conflicted
pull undoes only the files that pull wrote; unrelated uncommitted edits stay.

## An oak command was killed mid-update

If a command that rewrites the working tree (`oak pull`, `oak watch`'s pull,
`oak merge`, `oak reset`, `oak restore`, `oak sparse`, ...) is killed while it
writes files — a harness timeout, `kill -9` — the tree holds part of two
snapshots. Oak records that in `.oak/WORKTREE_UPDATE`, and until it is
recovered:

- `oak status --json`, `oak agent state --json` and `oak conflict status
  --json` report `progress_state.kind: "interrupted_update"` with
  `interrupted_operation`, a `detail` sentence, and the recovery in
  `next_commands`;
- every command that records or rewrites the tree (`oak commit`, `oak commit
  --push`, `oak finish`, `oak reset`, `oak restore`, `oak switch`, `oak
  merge`, ...) exits 4 with `error.code: "interrupted_worktree_update"`.
  `oak fetch` and read-only commands still run.

```bash
oak pull --abort --json   # recover and stop (offline unless content must be fetched)
oak pull                  # after an interrupted pull only: recover, then pull again
```

`oak pull --abort` is always listed first because it never overwrites
anything of yours. What it does depends on where the branch pointed when the
command died, and `detail` says which:

- **rolled back** (pull, parent sync, sparse change): files the update had
  written get their committed content back, files it had added are removed,
  files it had removed return.
- **completed** (`oak merge`'s move to the fresh branch): the branch already
  points at the new snapshot, so the remaining files are written.
- **cleared** (`oak reset`, `oak restore`): nothing is replayed; rerun the
  command to finish it.

What is and is not preserved:

- A path whose content is neither what the update replaced nor what it wrote
  — your edit made after the kill, a file you created — is **left exactly as
  it is** and listed under `recovered.preserved` in the receipt. Check those
  paths yourself.
- Untracked files, edits and deletions at paths the update does not touch are
  never touched.
- An uncommitted edit or untracked file at a path the update **had already
  overwritten** before it died (`oak pull` itself overwrites such files) is
  put back as it was: the update stores what it is about to overwrite.
- `oak pull` (the second command) pulls again after recovering, so it
  rewrites the incoming paths, including any you edited after the kill.

The receipt (`oak pull --abort --json`): `recovered.action` (`rolled_back` /
`completed` / `cleared`), `written`, `removed`, `preserved` (`path`,
`reason`), `superseded`, plus `progress_state` and
`recommended_next_commands` for the state afterwards.

If recovery cannot proceed it exits 4, changes nothing and keeps the record:

- content it must write is not stored locally and could not be fetched — run
  it again with network access;
- the record is unreadable — Oak cannot tell what was written. Keep the
  record, inspect with `oak diff --json`, and only when the tree holds what
  you want delete `.oak/WORKTREE_UPDATE` by hand (the tree then counts as
  ordinary uncommitted changes). This is the only case where deleting the
  record is right;
- the record was written by a newer oak — `oak upgrade`, then recover.

`oak watch` recovers an interrupted update by itself only when nothing would
be left for you to decide; otherwise it stops syncing, never checkpoints the
tree, and reports the command to run.

An interrupted `oak switch` is separate (its commands also exit 4): rerun the
same `oak switch` it names.

## Resolving conflicts

When `oak merge` or `oak pull` stops on conflicts — including a pull that
`oak watch` ran for you (it reports "CONFLICT: ..." through the hooks; resolve
it before continuing your task, since auto-pull stays paused until you do):

```bash
oak conflict status            # which operation, how many paths
oak conflict show --json       # per-path facts
# For each conflicted file, either edit the <<<<<<< markers by hand, or:
oak conflict take PATH --ours    # keep this branch's side of every block
oak conflict take PATH --theirs  # keep the parent/remote side
# then resume the operation that stopped:
oak merge --continue   # or: oak pull --continue
# or abandon it:
oak merge --abort      # or: oak pull --abort
```

`--ours` is always the current branch's side; `--theirs` is the
parent/remote side.

Blocks are diff3-style: `<<<<<<< <branch>`, ours, `||||||| original`, the
common base, `=======`, theirs, `>>>>>>> <parent>`. When editing by hand,
delete the base section along with the markers. Any column-0 marker line
left behind shows as `has_marker_lines: true` in `oak conflict show --json`,
and `--continue` refuses it.

## Discarding and restoring work

```bash
oak status                      # ALWAYS check what you'd discard first
oak reset -f                    # discard all uncommitted changes
oak reset path/ -f              # discard under one path
oak restore file.rs             # restore files to HEAD state
oak restore -s <commit> file.rs # restore from a specific commit
oak pull -f                     # discard local commits not on remote; sync to remote HEAD
oak push -f                     # overwrite diverged remote with local history
```

`reset`/`restore` prompt without `-f`. The force forms of pull/push discard
real history on one side — state which side wins and why before using them.

## Inspecting old states

```bash
oak log -n 20 --oneline        # find the commit
oak log -S "some_symbol"       # commits changing occurrences of a literal
oak switch -d <hash>           # detach HEAD there to look around
oak switch <branch>            # come back
oak restore -s <hash> <path>   # pull one file out of an old commit
```

## Untangling a branch: oak split

When one branch accumulated unrelated work, split it headlessly:

```bash
oak split --dry-run --plan - <<'EOF'
pick a1b2c3
pick d4e5f6
split docs-cleanup
pick 778899
EOF
```

Every commit on the branch must appear exactly once (`pick` or `drop`).
Picks before the first `split` rewrite the source branch; each `split NAME`
starts a new branch off main for the later picks. Split segments are
parented on main, so each segment must stand alone against main — a dependent segment conflicts and
nothing at all is written (the operation is atomic). Preview with
`--dry-run`, then run without it.

## Interrupted finish / mount finish

`oak finish` and `oak mount finish` are multi-leg sagas. JSON reports the phase
reached. An explicit rejection may name a manual retry; an unconfirmed
publication has a null retry command and supplies read-only reconciliation
commands instead. The mount stays intact until publish succeeds; work is never
dropped. Covered branch-head attempts survive process restart; inspect them with
`oak agent state --refresh --json --compact`. Current state is not an
acknowledgement, so Oak never retires a record merely because the target is now
visible.

## Escape hatch to git

```bash
oak export /path/to/new-git-repo [--git-branch main]
```

Replays the current branch's linear ancestry as git commits (author +
timestamp preserved). Use when someone needs the history in git tooling —
not for day-to-day work.
