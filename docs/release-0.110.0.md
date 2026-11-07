# Oak 0.110.0

Oak 0.110.0 is a minor release of the `oak` CLI and the `oakvcs-core`
library. It adds the CLI side of **Projects**: project discovery, view
checkouts and mounts, and maintainer approvals on merge. It also adds
`oak ci publish` and lets `oak watch` share uncommitted work, and macOS mounts
get faster and easier to start. This is the first release published to Oak
releases rather than GitHub Releases.

`oakvcs-core` has breaking API changes, listed below. Object hashes are
unchanged. Local repositories gain one SQLite migration.

Released source: oak/oak main at the version-bump commit (notes cover main
through `f5a59fb0`). The previous release, 0.109.1, was built from oak/oak
main `abc9ae82`.

## Breaking changes

In `oakvcs-core`:

- `LocalRelease::is_release()` is gone, because every tag is now a release.
- `OakError` has two new variants, `ApprovalRefused` and `PathOutsideView`.
  An exhaustive `match` on it needs new arms.

In the CLI:

- `oak release list` no longer takes `--kind`.
- `oak release` JSON no longer has the `is_release` field.
- `.oak/PERMISSIONS` is retired. Path rules now live only in `OAK_PROJECT`
  files, and servers no longer read the old file. It stays trackable, so
  history that contains one still checks out.

## Projects

A company repo holds its code as projects: directories with an `OAK_PROJECT`
file. These commands need a server with the Projects APIs.

- `oak project list|show|which|deps|search` shows the projects at HEAD, with
  their owners, dependencies and dependents. With `--org ORG`, it reads the
  server's index instead, so no checkout is needed.
- `oak project impact` lists the projects a branch touches and their
  dependents. `oak status`, `oak finish` and `oak merge` report the same in
  their `--json` output.
- `oak clone ORG/PROJECT` clones one project as a view: its directory plus its
  dependencies.
- `oak mount ORG --project A --project B ./dir` mounts one view holding several
  projects. `oak mount ORG/PROJECT` mounts one.
- `oak view show|add|remove` shows, widens or narrows a view checkout. A change
  re-acquires the checkout, so it refuses while there is uncommitted, untracked
  or unpushed work.
- `oak ls --remote`, `oak cat --remote` and `oak grep --remote` read paths
  outside your checkout. They're read-only and filtered by permission.

Work outside a view is caught before the commit, with `path_outside_view` and
the `oak view add` command that fixes it.

## Approvals

In a repo with projects, `oak merge` is refused (`approvals_required`) until a
maintainer of every touched project, who didn't work on the branch, approves
its slice of the diff. Editing that slice needs a new approval. Other pushes
keep it.

- `oak approve` approves a branch from the CLI. An approval has to come from a
  person, so it needs an API key created with the `Review` scope. `oak login`
  tokens, ordinary keys and service accounts are refused.
- A branch that has moved since the head you reviewed is refused rather than
  approved unseen.
- Only an org admin can force past project approvals, and by default only on
  the branch page. The refusal says why.

## `oak ci publish`

`oak ci publish` authorizes one publish to a channel. `oak ci deploy` remains
as an alias.

- `--commit HASH` republishes an older commit instead of the head.
- `--pause-auto-publish`, with `--commit`, also pauses the channel, so the next
  merge or timer doesn't replace it.

## `oak watch`

- `oak config set watch.share_working true` shows uncommitted text files on the
  branch page. It's off by default, because files leave your machine as they're
  saved, before any commit. Ignored files, binary files and credential-shaped
  names stay local.
- Fixed startup races and a daemon signal race.
- The agent instructions now start every task with `oak start`.

## Mounts

- `oak` commands work inside mounts with the same flags and JSON receipts as a
  checkout, including `oak commit --push`, `oak push --plan`,
  `oak pull --branch-only`, the `oak diff` modes, `oak log -S`/`-G`,
  `oak desc --append` and `oak conflict take`.
- File operations in macOS mounts are faster.
- Renames keep overlay state correct, including ignored files and whole
  directories. Symlinks can be created inside a mount.
- The Oak Mount app has a **Mount Repository…** window. It searches your repos
  and every public repo, and mounts the one you pick at
  `~/oaktree/<owner>/<repo>`. The app also has a new icon.
- On macOS, `oak upgrade` now updates an installed Oak Mount app along with the
  CLI. If the app update fails, the upgrade warns, and the next `oak upgrade`
  tries again.

## Remote review

`oak branch diff --remote` and `oak branch review --remote` now ask the
server's review API for the file list, hunks and merge verdict. They work from
any directory with `--repo OWNER/NAME`. Older servers fall back to the local
path, which needs a checkout.

## Fixes

- A shallow clone no longer fails on a tag behind head. Tags no longer have a
  foreign key to their commit (SQLite migration 0018).
- Status stays accurate when a dirty tree reconciles a remotely merged branch.
  The new branch stays on the merged tip, so the next `oak commit` can't revert
  what main gained.

## Releases

- Stable releases are published to <https://oak.space/oak/oak/releases>, and
  `oak release download vX.Y.Z` fetches their assets. oak.space's mirror
  carries them to GitHub.
- Each nightly is also a dated Oak prerelease. A daily timer publishes the
  newest commit on `main` that passed Verify, and a nightly that can't create
  its prerelease now fails.
