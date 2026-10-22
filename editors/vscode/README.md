# Oak Version Control

Source control integration for [Oak](https://oak.space) repositories — the
same workflow the built-in Git extension gives you, driven by the `oak` CLI.

The extension activates in any folder with an `.oak/` directory (and scans one
level of subfolders, so a directory of checkouts works too). It never talks to
the server itself: every action runs `oak` with `--json`, so behaviour matches
the CLI exactly.

## Requirements

The `oak` CLI on `PATH`, or set `oak.path`. Commands that reach oak.space
(push, pull, merge, CI) need `oak login`; **Oak: Log In to oak.space** opens a
terminal running it.

## How Oak maps onto the Source Control view

Oak has no staging area and commits carry no message, so a few things look
different from Git:

| Git | Oak in VS Code |
|---|---|
| Staged / Changes groups | **Changes** (everything dirty; new files are tracked automatically) |
| Commit message box | The **branch description** — it becomes the squash-merge message. Edit it and press ⌘Enter / Ctrl+Enter: the description is saved, then a checkpoint is made. The box keeps showing the description afterwards. |
| Commit | **Checkpoint** (`oak commit`) — local only. Inline ✓ on a file checkpoints just that file. |
| Stage + discard | **Discard Changes** (`oak restore`); on a new file this deletes it. **Discard All** runs `oak reset`. |
| — | **Branch Changes (vs main)** — everything the branch changed since its fork point (`oak status` branch changes). |
| Merge conflicts | **Merge Conflicts** group with **Accept Ours / Accept Theirs** (`oak conflict take`) and **Continue / Abort** for the pull or merge in progress. |

## Features

- **Changes, quick diff and decorations** — gutter change markers against
  HEAD, M/A/D/R badges in the Explorer and tabs, click a change for a side-by-side
  diff, **Open All Changes** in the multi-file diff editor.
- **Pushing** — a Push button in the Source Control title bar, and a
  `$(repo-push) N` status-bar item whenever the branch has commits the server
  doesn't (read locally from `oak agent state`, refreshed from the server after
  push/pull/fetch). After each checkpoint a notification offers **Push**,
  **Always Push** (sets `oak.postCommitCommand` to `push`) or **Don't Ask
  Again**. The Changes group has an inline **Checkpoint & Push**, and pushing
  with uncommitted files asks whether to checkpoint them first.
- **Status bar** — current branch (click to switch), a sync button (pull then
  push; becomes *Publish* for repos not yet on oak.space), the push indicator
  and the CI result for the branch head.
- **Remote** — Push, Pull, Fetch, Sync, Publish to `ORG/REPO`, force
  variants behind a confirmation.
- **Merge** — server-side squash merge into `main`; **Merge (wait for CI)**
  rides out a running CI (`oak merge --wait`). A CI-gate refusal offers to wait.
- **Finish** — `oak finish`: save the description, checkpoint, publish.
- **Branches** — switch (including remote-only branches by typing a name),
  create (keeping or discarding dirty files), rename, close.
- **Oak History** view — commits with their changed files; open a file's
  change, open the whole commit in a multi-diff, compare with the working tree,
  restore a file from a commit, check out a commit detached, copy the hash, open
  the commit on oak.space. **View File History** (editor tab, Explorer, or SCM
  context menu) filters the view to one file.
- **CI** — status for the branch head in the status bar (polled while running),
  recent runs, logs in an editor, re-run.
- **oak.space links** — open the repo/branch review, a file, or a commit; copy
  the review URL.
- **Clone** and **Initialize** from the Source Control welcome view.

Everything is in the **⋯** menu of the Source Control view and the Command
Palette under *Oak:*. The **Oak** output channel logs every CLI call.

## Settings

| Setting | Default | |
|---|---|---|
| `oak.path` | `null` | Path to the `oak` executable. |
| `oak.autoRefresh` | `true` | Refresh when files change. |
| `oak.autofetch` / `oak.autofetchPeriod` | `false` / `180` | Periodically `oak fetch`. |
| `oak.postCommitCommand` | `none` | `push` to publish after every checkpoint. |
| `oak.promptToPushAfterCommit` | `true` | Offer to push after a checkpoint that wasn't pushed. |
| `oak.promptToSaveFilesBeforeCommit` | `always` | Save dirty editors before checkpointing. |
| `oak.confirmSync` | `true` | Confirm before Sync. |
| `oak.showBranchChanges` | `true` | Show the Branch Changes group. |
| `oak.ci.enabled` / `oak.ci.pollInterval` | `true` / `20` | CI status in the status bar. |
| `oak.decorations.enabled` | `true` | Explorer badges. |
| `oak.countBadge` | `all` | Activity-bar count. |
| `oak.repositoryScanMaxDepth` | `1` | Subfolder depth scanned for repositories. |
| `oak.historyPageSize` | `50` | Commits per page in Oak History. |

## Not covered

- **Git-backed repos** (`oak init --git` / `oak clone --git`) have a real
  `.git` and no `.oak/`; the built-in Git extension already handles them.
- **Blame** — the CLI has no blame command yet.
- `oak mount` / `oak space` management and `oak split` remain CLI-only.

## Development

```bash
cd editors/vscode
npm install
npm run compile
npm run test:unit
VSCODE_EXECUTABLE="/Applications/Visual Studio Code.app/Contents/MacOS/Code" npm run test:integration
npm run package        # builds oak-vcs-<version>.vsix
```

`test:integration` launches VS Code against a throwaway `oak init` repository
(set `OAK_BIN` to test a specific CLI build). Without `VSCODE_EXECUTABLE` it
downloads a VS Code build first. To try the extension interactively, open this
folder in VS Code and run **Debug: Start Debugging** with an *Extension
Development Host* configuration, or install the `.vsix`.
