# Git backend: host Oak repos on GitHub

Oak can store a repository as a plain git repository instead of `.oak/oak.db`,
the way jj's git backend works. Commits, trees, blobs, branches, and HEAD are
ordinary git objects and refs, so the repo can be hosted on GitHub, GitLab,
or any git server, and `git` and `oak` can be used side by side.

## Getting started

```bash
# New repo
oak init --git my-project
cd my-project
git remote add origin git@github.com:me/my-project.git   # or: gh repo create --source . --remote origin

# Existing GitHub repo: keep it git-backed instead of importing into Oak storage
oak clone --git git@github.com:me/my-project.git

# Any existing git checkout already works: oak resolves `.git/` when there's no `.oak/`.
```

Then the usual loop:

```bash
oak switch -c my-feature
oak desc "What this branch does"   # included in each commit message
oak commit                         # a git commit on refs/heads/my-feature
oak push                           # git push --set-upstream origin my-feature (prints a PR link on GitHub)
oak pull                           # fetch, merge origin/my-feature, then origin/main
oak finish --desc-file desc.txt    # set description, checkpoint, push
```

## How it maps

| Oak | git |
| --- | --- |
| commit / tree / blob hash | git commit / tree / blob OID (SHA-1) |
| branch head | `refs/heads/<branch>` |
| current branch | symbolic `HEAD` |
| branch description, parent, status | sidecar `.git/oak/branches.toml` |
| commit message | generated per checkpoint (see below) |
| commit author | `GIT_AUTHOR_*`, then git `user.name` / `user.email`, then Oak's author |

### Commit messages

Oak checkpoints have no message of their own, so the git backend writes one
that describes the checkpoint:

```
Add greet.txt, update README.md

Checkpoint 2 on feat (parent: main): 2 files changed, +12 -3

  A  greet.txt  +1
  M  README.md  +11 -3

Branch description:
  Add greeting

Oak-Branch: feat
Oak-Checkpoint: 2
```

- **Subject:** a summary of this checkpoint's changes (`Add m1.rs, m2.rs and
  m3.rs in src/cmd`, `Rename a.txt to b.txt, change mode of run.sh`,
  `Update 7 files in cli/ and core/ (2 added, 5 modified)`), at most 72 chars.
- **Body:** checkpoint number, parent branch, totals, then a per-file table
  like `git diff --stat` (binary/large files say so; up to 50 files listed).
- **Branch description:** included for context when reading a single commit.
- **Trailers:** `Oak-Branch` and `Oak-Checkpoint`, readable with
  `git log --format='%(trailers)'`. The number continues across merge
  commits from `oak pull`.

Whenever Oak moves HEAD (commit, switch), it resets git's index to HEAD's
tree, so `git status` agrees with `oak status`.

Remote operations shell out to the system `git` binary (as jj does), so SSH
keys, credential helpers, and `gh auth setup-git` all just work. `-r/--remote`
takes a git remote name or URL; it defaults to the branch's upstream remote,
then `origin`.

## Differences from an Oak-hosted repo

- `main` is an ordinary local branch: `oak switch main` works, and `oak pull`
  on main fast-forwards it.
- `oak merge` doesn't merge (that is a CI-gated, server-side Oak operation).
  It points you at the host instead: push, then open a pull request (`gh pr create --fill`).
- `oak pull` conflicts leave git's normal merge state. Resolve, then
  `git add <paths> && git commit --no-edit`, or `git merge --abort`.
  `oak pull --json` reports `conflict_paths` and exits non-zero.
- Not supported: mounts/spaces, Oak CI, releases, chunked storage, `oak push --plan`.
  Submodules are skipped when reading trees.
