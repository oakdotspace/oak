# Conflicts and recovery

Nothing in Oak is a dead end. Every in-progress operation (merge, pull
parent-sync, mount pull) can be inspected, continued, or aborted, and
history is an escape hatch away from any bad working-tree state.

## Where am I?

```bash
oak conflict status [--json]   # summarizes any in-progress conflicted operation
oak agent state --json --compact  # broader: full state + recommended next commands
```

Exit code 5 from any command means conflicts; 4 means a dirty working tree
blocked the operation (commit or reset, then retry); 3 means the repo is
locked by another oak process.

## Resolving conflicts

When `oak merge` or `oak pull` stops on conflicts:

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
starts a new branch off main for the later picks. Branches are flat, so each
segment must stand alone against main — a dependent segment conflicts and
nothing at all is written (the operation is atomic). Preview with
`--dry-run`, then run without it.

## Interrupted finish / mount finish

`oak finish` and `oak mount finish` are retryable sagas. On a failed leg the
JSON reports the phase reached and the exact next command — run that command.
The mount stays intact until publish succeeds; work is never dropped.

## Escape hatch to git

```bash
oak export /path/to/new-git-repo [--git-branch main]
```

Replays the current branch's linear ancestry as git commits (author +
timestamp preserved). Use when someone needs the history in git tooling —
not for day-to-day work.
