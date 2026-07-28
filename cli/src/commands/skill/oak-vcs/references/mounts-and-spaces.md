# Mounts and Oak spaces

Oak's answer to `git worktree`: isolated, concurrent checkouts without a full
clone behind each one. A **mount** presents a remote repo as a lazy virtual
filesystem — file content hydrates on first access, so mounting is
near-instant and cheap on disk even for huge or binary-heavy repos. Writes
land on a **virtual branch** that exists only locally until pushed. A
**space** is a directory organizing one mount-per-repo per task across a
whole org.

Platform notes: macOS uses Apple FSKit (macOS 26+, signed "Oak Mount" app
installed and enabled — no kernel extension). Linux uses FUSE
(`fusermount3` from the `fuse3` package).

## Mount lifecycle

```bash
oak mount ORG/REPO [DEST]     # mount at DEST (default ./REPO); returns once live
oak mount ORG/REPO [DEST] --branch NAME   # mount an existing remote branch
oak mount list                # active mounts and their virtual branches
oak mount end [DEST] [-f]     # unmount, drop state, remove dir (no DEST: all under ~/oaktree)
oak mount forget [--force]    # drop a stale registry entry (crash/reboot leftovers)
```

Each mount runs as a detached background daemon. Inside the mount directory,
normal commands (`oak status`, `oak diff`, `oak commit`, `oak push`, …)
operate on the virtual branch automatically. The uncommitted overlay is the
"active commit": `oak commit` checkpoints it; the next edit starts a new one.

A plain mount starts a **fresh** virtual branch off the trunk. `--branch
NAME` instead serves an existing remote branch — its history and files —
and `oak pull` / `oak commit` / `oak push` inside the mount continue that
branch. That's the cheap way to resolve a branch's merge conflicts locally
(`oak pull`, edit the markers, `oak pull --continue`, `oak push`) without a
full clone. `oak switch` never works inside a mount — a mount is pinned to
its branch for life; mount the other branch at another path instead.

## Finishing a mounted task

```bash
# from inside the mount:
oak finish --desc-file /tmp/desc.txt --json
# from outside (e.g. the space root):
oak mount finish ./<task>/<repo> --desc-file /tmp/desc.txt --json
```

Both require a linked remote before mutating, then stepwise: set the branch
description → checkpoint the active overlay if dirty → publish the virtual
branch → end the mount after publish succeeds. This is a **retryable saga,
not a rollback-atomic transaction**: if a leg fails, the mount stays intact
and the JSON names the completed phase and the exact next manual command —
run that command; don't start over.

## Oak spaces

A space spans an org; each task gets a subdirectory holding one mount per
repo the task touches.

```bash
oak space new ORG [DIR]    # scaffold: AGENTS.md, CLAUDE.md, .claude/settings.json, .oak-space
oak space repos [ORG]      # list the org's repos (reads .oak-space when in a space)
oak space clean [DIR]      # tear down finished mounts (clean = committed + pushed)
```

Task workflow inside a space:

1. **Pick a slug** — 2–4 word kebab-case describing the *work*, e.g.
   `fix-auth-redirect`. It names the task directory and the virtual branch
   (`<slug>--<id8>`), so never name it after the repo.
2. **Pick repos** — `oak space repos`, then for each:
   `oak mount ORG/<repo> ./<slug>/<repo>`. A cross-repo task gets sibling
   mounts under one task dir, one virtual branch per repo.
3. **Read each mounted repo's own `AGENTS.md`** explicitly
   (`./<slug>/<repo>/AGENTS.md`) — cwd stays at the space root, so it won't
   auto-load.
4. Edit only under `./<slug>/<repo>/`. Never write to the space root, the
   task directory itself, or a sibling task's directory.
5. Finish each repo's mount separately (see above), even within one task.

`oak space clean` skips mounts with uncommitted or unpushed work so nothing
is lost; `--force` discards those too.

## Keep build output off the mount

A mount is ideal for reading and editing source, poor for build output and
caches. A build cache on the mount can corrupt its own artifacts and emit
baffling, wrong errors on healthy code — and it's slow. Redirect output to
real disk:

- Rust: `CARGO_TARGET_DIR=/tmp/<name> cargo build|check|test|clippy`
- Node/Bun: install and build in a temp dir; point the tool at the mount for
  sources only
- Anything with a `target/`, `dist/`, `build/`, or cache dir: send it to
  `/tmp`

Also: the filesystem refuses `symlink(2)` and `link(2)` with `EPERM` — steps
that lay down symlinks/hardlinks (`npm`/`bun install`, some codegen) must run
off-mount.

## Claude Code worktree hooks

`oak mount worktree-create ORG/REPO` and `oak mount worktree-remove`
implement Claude Code's `WorktreeCreate`/`WorktreeRemove` hooks. Wired into a
repo's `.claude/settings.json`, `isolation: "worktree"` and
`claude --worktree` transparently get an `oak mount` on a fresh virtual
branch instead of a git worktree; removal tears the mount down only when it
holds no uncommitted or unpushed work. The create hook needs a fixed
ORG/REPO, so `oak space new` does **not** wire these (a space spans an org);
add them in a single-repo project's settings yourself. Other agents with
create/remove worktree hooks work the same way — the create hook reads the
worktree path from stdin JSON and prints it back.
