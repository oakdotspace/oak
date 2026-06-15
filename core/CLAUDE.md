# Oak Repository

This project uses [Oak](https://oak.space) for version control — **not Git**.
Do not run `git` commands. Use `oak` instead.

## Key commands

```bash
oak status          # show changed files
oak diff            # show changes vs HEAD
oak commit          # snapshot the working directory (no message needed)
oak log             # show commit history
oak push            # push commits to the remote server
oak pull            # pull latest commits from the remote server
```

## Branching

```bash
oak switch -c my-feature    # create a branch and switch to it
oak desc "what this branch does"   # set the current branch's description
oak switch my-feature       # switch back to an existing branch
oak merge                   # merge current branch into its parent
```

You are currently on branch **zdgeier-ca5ed2** (parented onto `main`).
Commit freely — your changes are isolated until you `oak merge` or open a PR
on oak.space.

## What Oak is

Oak is a version control system designed for AI-assisted workflows.
Every session gets its own branch. Commits have no messages; the branch
description is the narrative. Large binary files are handled natively via
content-defined chunking — no LFS required.

See `oak --help` for the full command reference.
