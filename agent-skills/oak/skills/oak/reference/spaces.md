# Oak spaces & mounts

This is the agent workflow for working against a repo **without cloning it** —
and for running **several isolated tasks in parallel**. Use it when the user
mentions an "Oak space", asks to run tasks concurrently, or works against a
very large / binary-heavy repo where a full clone is impractical.

## Mounts: a lazy virtual filesystem

`oak mount <owner>/<repo> [dest]` mounts a repo as a virtual filesystem at
`dest` (default `./<repo>`). File content hydrates **on demand** as you read
it — nothing is copied up front — and writes go to a fresh *virtual branch*
that lives locally until you publish it with `oak push`. The mount runs as a
detached background daemon, so the command returns as soon as the mount is
live.

```bash
oak mount acme/blog ./blog     # mount at ./blog on a new virtual branch
cd ./blog
oak status                     # normal oak commands work inside a mount
DESC_FILE="$(mktemp)"          # write the final branch description here
cat > "$DESC_FILE" <<'DESC'
Short summary of the change.
DESC
oak mount list                 # show active mounts + their virtual branches
oak finish --desc-file "$DESC_FILE"
cd ..
```

Inside a mount, the everyday commands (`oak status`, `oak diff`, `oak commit`,
`oak push`, `oak desc`) all just work — they route through the mount onto the
virtual branch automatically. `oak commit` creates a local checkpoint only;
publish mounts with `oak push`. To wrap up, write the branch
description to a temp file and run `oak finish --desc-file <file>` from inside
the mount. From outside the mount, use `oak mount finish [path] --desc-file
<file>`. Both require a linked remote before mutating, then stepwise set the
description, checkpoint dirty overlay work if needed, publish committed work,
and tear the mount down without `--force`. They are not rollback-atomic; if
finish fails, it leaves the mount intact and names the next manual command to
run.

> Older docs may say `oak mount start ...` or `oak mount status`. Those were
> removed. Use `oak mount <spec> <dest>` to start one and `oak mount list` to
> inspect them.

## Spaces: one mount per task

An **Oak space** is a directory for one org. Each task gets a subdirectory, and
inside it you mount whichever repo or repos the task touches — Oak's answer to
git worktrees for parallel agent work, but without a clone behind each mount.

```bash
oak space new acme [dir]        # scaffold a space (default ./acme)
```

That lays down, in the space directory:

- `AGENTS.md` — the per-task workflow (read it; it's the source of truth for
  that space).
- `CLAUDE.md` — a one-line `@AGENTS.md` import so Claude Code loads it too.
- `.claude/settings.json` — space settings for Claude Code: pre-approved
  permissions for the `oak` commands the workflow uses (including
  `oak space clean`) and a Stop hook that lists active mounts. (The
  `oak mount worktree-create` / `worktree-remove` hook subcommands exist,
  but a space's settings don't wire them — the create hook needs a fixed
  `<owner>/<repo>`, and a space spans the whole org.)
- `.oak-space` — the org marker used by `oak space repos`.

Then, per task:

```bash
oak space repos
oak mount acme/blog ./<task-slug>/blog   # start a repo mount for the task
cd ./<task-slug>/blog
# ...edit...
cd ../..
DESC_FILE="$(mktemp)"
cat > "$DESC_FILE" <<'DESC'
Short summary of the change.
DESC
cd ./<task-slug>/blog
oak finish --desc-file "$DESC_FILE"
cd ../..
```

When a task is done and merged, reclaim any already-finished mounts with
`oak space clean`:

```bash
oak space clean [dir] [--force]
```

`oak space clean` tears down mounts whose working tree is clean (committed +
pushed). Mounts with in-flight work — uncommitted changes or unpushed
commits — are skipped so it is never lost; `--force` discards and removes
those too.

## Key rules for agents working in a space

- The space root is **not** a checkout. Do all editing under a task
  repo mount (`./<slug>/<repo>/`), never at the space root, the task directory
  itself, or in a sibling task.
- The repo's own instructions live in `./<slug>/<repo>/AGENTS.md` — read it
  after mounting; the space root's cwd won't auto-load it.
- Commits are still messageless; `oak commit` creates a local checkpoint and
  never publishes. `oak finish --desc-file <file>` inside a mount requires a
  linked remote before mutating, then sets the virtual branch's merge message
  from a file, checkpoints if needed, publishes, and ends the mount. Use `oak
  mount finish [path] --desc-file <file>` when you need to finalize by path
  from outside the mount. Use `oak desc --file <file>` inside the mount only
  when you need to update the description without finalizing yet.

## Conflict recovery

When a pull, merge, or finish reports a conflict, inspect it before editing:

```bash
oak conflict status --json
oak conflict show --json
```

In a regular checkout, `oak conflict take <path> --ours` or `--theirs` can
resolve one path by selecting that side inside each marker block while keeping
already-merged hunks. In a mount, edit the mounted file normally, then run the
recommended `oak pull --continue`. Do not use a separate JSON patch format for
conflict resolution.
