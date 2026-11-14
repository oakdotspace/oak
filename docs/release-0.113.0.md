# Oak 0.113.0

Oak 0.113.0 is a minor release of the `oak` CLI and the `oakvcs-core` library.
Teams are now groups, CI runs on `main` after a merge instead of gating it,
branches can be stacked, and `oak serve` is gone.

Object hashes are unchanged, and there's no SQLite migration.

Released source: oak/oak main at the version-bump commit. The previous release,
0.112.0, was built from oak/oak main `18eecfe0`.

## Groups

Access in an organization now comes from groups: a repo grants a group read or
write access, and the directories in it can be owned by a group.

- `oak groups` lists the groups you're in, and marks the one this checkout
  works as.
- `oak group list|show|create|add|remove|invite` manages an organization's
  groups. Sub-group members count as members of the group above.
- `oak group grant GROUP REPO --role read|write` gives a group access to a
  repo. `oak group grant public REPO` makes it public.
- `oak group switch GROUP` makes this checkout work as that group, and
  `--clear` goes back to your own access.
- `oak group agent list|add|remove` puts an organization's agents (service
  accounts) on groups. An agent reaches what its groups reach.

`oak team` still works as an alias.

## CI on main

CI runs on `main` after a merge, and branches don't wait on it.

- `oak merge` no longer waits for CI. `--wait` is accepted and does nothing.
- `oak ci status` reports each service (one workflow file) on `main`: passing,
  failing since a commit, or running. `oak ci status --head` shows the newest
  run for the current checkout head.
- `oak ci wait` retries 408, 429, 500, 502, 503 and 504 answers and dropped
  connections until its original deadline, instead of stopping at the first.
- `oak ci logs` shows how long each job and step took.

`oak queue` is removed, since there is no merge queue.

## Stacked branches

A branch can build on another open branch instead of `main`.

- `oak switch -c NAME --stack` stacks a new branch on the current one.
  `--parent BRANCH` stacks it on another open branch.
- `oak pull` on a stacked branch merges in its parent. When the parent lands
  or closes, the branch moves onto the parent's parent first.
- Stacks land bottom-up: `oak merge` on a stacked branch names the branch to
  land first, and `oak push` waits until the parent is on the server.
- `oak status` and `oak info` show the stack, and their JSON has a `stack`
  array when the branch is stacked.

## Roots, Pages and retention

- `oak root list|show|create|archive|pull|push` works with roots: separate
  timelines inside one repo. `oak root create NAME --from main` starts one at
  a commit on `main` (a release line). A root's files live in `roots/NAME/`,
  which the checkout ignores.
- `oak site` manages Oak Pages sites at `NAME.oak.page`: `create`, `show`,
  `rename`, `set`, `rm`, `deploys`, `redeploy`, `rollback` and `check`.
  `oak site domain add|list|verify|primary|rm` serves a site on your own
  domain, and `oak open --site NAME` opens one.
- `oak retention list|set|remove` keeps only the newest versions of files
  under a directory. `oak pull` and `oak clone` accept history whose older
  versions were removed this way.

## Cloning and pushing

- `oak clone` of a repo with directories you can't read clones the rest as a
  partial checkout. Before, it stopped with an error unless you named
  directories with `--path`.
- `oak clone --detached --expected-head FULL` checks out `main` at exactly
  that commit, or refuses.
- A clone whose admission token is too long for a request URL fails early with
  `clone_admission_token_too_large` and suggests a shallow clone.
- Pushes of more than 128 new files or 64 MiB of new content to oak.space
  now publish (the new content is uploaded first). `oak push --json` reports
  the chunks it needed and uploaded, and a long upload prints its progress.
- A push that fails leaves the checkout's remote and repository as they were.
- `oak commit --push`, `oak finish` and mount finish always need a credential.

## Safer working-tree updates

- An update of the working tree that's interrupted (a killed `oak pull`,
  `oak switch` or `oak start`) is recorded before it starts. `oak status` and
  `oak agent state` report it, `oak commit` refuses it, and running the
  command again finishes it.
- Every update checks the whole tree before it writes anything: an untracked
  file in the way, a file or directory where the other is needed, or a file
  whose name differs only in case is refused up front.
- A file renamed between Unicode normalization forms (`café` written two
  ways) is kept on `oak pull`, `oak switch -c` and `oak start`. When the
  filesystem keeps the old spelling, the command's JSON lists it in
  `kept_spellings`.
- Merges keep a change to only the executable bit, keep symlinks and binary
  files whole in a conflict, combine several best merge bases, and remove the
  files `main` deleted when `oak pull` stops on a conflict.
- A filename with a literal backslash is refused on macOS and Linux, and
  255-byte filenames work.

## Other changes

- `oak start` and `oak switch -c` ask the server for `main`'s head every time,
  and `oak start --json` says where the base came from.
- Every failing `--json` command prints one document with an `error` whose
  `code` is from a fixed list.
- `oak status --json` reads the tree once, straight histories are compared
  without walking all of history, and the log viewer scrolls large diffs
  quickly.
- `oak status` in a mount whose filesystem is gone says so, with the command to
  remount, and a full disk is reported as `disk_full`.
- `oak help` prints the same overview as `oak`, regrouped by what you're doing.
- `oak login`'s browser pages match oak.space's sign-in pages.
- A command run outside a repository never treats `~/.oak` as one.

## Removed

- `oak serve`, `OAK_SERVE_TOKEN` and the self-hosting guide.
- `oak queue` and `oak findings`.
- The old `oak site enable|disable|show|list`, replaced by the commands above.

## Compatibility

This release goes with the current oak.space server (groups, roots and CI on
`main`). Older CLIs keep working against it, but they stop with an error when
cloning a repo that has directories they can't read.
