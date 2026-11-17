# Oak 0.117.0

Oak 0.117.0 is a minor release of the `oak` CLI and the `oakvcs-core` library.
It removes stacked branches and git-backed repositories, moves the inspection
commands under `oak doctor`, and adds `oak report`.

Object hashes are unchanged, and there's no SQLite migration.

Released source: oak/oak main at the version-bump commit. The previous release,
0.116.0, was built from oak/oak main `b7b8064b`.

## Branches

- Stacked branches are gone. Every branch parents onto `main`.
  `oak switch -c --stack` and `--parent`, `oak merge --into-parent` and
  `oak branch train` are removed, `oak pull` no longer restacks, and
  `oak status` no longer shows a stack. To break up a large branch, use
  `oak split`.
- A branch stacked with an older Oak now reads as based on `main`: it syncs
  from and merges into `main`, and its next push stores `main` as its parent.
- JSON: `stack` is gone from `oak status --json`, `restacked_from` from the
  pull receipt, and `restacked_branches` from the merge and close responses.

## Git

- Git-backed repositories are gone: `oak init --git` and `oak clone --git`
  are removed, and Oak commands no longer work on a `.git` directory.
- Importing from git stays. `oak clone <git-url>` imports the history into
  Oak, and `oak init` in a directory with a `.git` offers to import it.
  `oak init --import-git` imports without asking (required when there's no
  terminal), and `--no-import-git` starts an empty repository beside the
  `.git`.
- `oak export` writes history into a new git repository with git
  fast-import. Commits keep their author, message and timestamp, merges
  within the branch keep both parents, and a history imported from git
  exports back to the same commit ids.

## Commands

- The inspection commands are now subcommands of `oak doctor`:
  `oak doctor file`, `oak doctor refs`, `oak doctor tree`, `oak doctor blob`
  and `oak doctor compact` (formerly `oak maintenance`). `oak doctor --repo`
  checks remote content integrity as before.
- `oak checkout`, `oak rev-parse`, `oak info`, `oak archive` and
  `oak branch close` are removed. Each exits with code 2 and names its
  replacement: `oak status --json` or `oak agent state --json` for `oak info`,
  `oak hash` for `oak rev-parse`, `oak switch --detach COMMIT` for
  `oak checkout`, `oak export --tree-only --at COMMIT DIR` for `oak archive`,
  and `oak close` for `oak branch close`.
- `oak skill status` and `oak skill show` report and print the installed
  agent skill, and the bundled skill is shorter.
- The MCP server (`oak mcp`) has more tools. It is read-only unless started
  with `--allow-write`.

## Marks

- `oak report COMMIT` replaces `oak mark report`: with `--name` and `--body`
  (or `--body-file`) it files a report, and without a body it lists the
  commit's reports. `oak mark report` still works.
- `oak mark follow` runs the script with `OAK_COMMIT`, `OAK_MARK`,
  `OAK_MOVE_ID`, `OAK_REPO_ID` and the move's signed grant in `OAK_GRANT` and
  `OAK_GRANT_SIGNATURE`. The `OAK_CI_*` names are still set as deprecated
  aliases, so existing follower scripts keep working.

## Compatibility

This release needs the current oak.space server. The server now refuses to
create or push an open branch whose parent is another branch, so older CLIs
that pass `--stack` or `--parent` get an error; upgrade with `oak upgrade`.
Scripts that call `oak info`, `oak rev-parse`, `oak checkout` or
`oak archive` need the replacements above.
