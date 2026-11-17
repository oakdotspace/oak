# Oak 0.115.0

Oak 0.115.0 is a minor release of the `oak` CLI and the `oakvcs-core` library.
Oak CI is gone and marks replace it, and `oak mount` and the Oak Mount app are
gone too.

Object hashes are unchanged, and there's no SQLite migration.

Released source: oak/oak main `c5e64699`, the first release made by moving the
`cli-released` mark. The previous release, 0.114.0, was built from oak/oak
main `46cde0ca`.

## Marks

- `oak mark` is new. A mark is a named pointer to a commit on `main`
  (`server-good`, `cli-released`) that people and bots move forward to record
  what passed. A mark can require other marks to be at or past the commit,
  name approvers, and be owned by a group. Subcommands: `list`, `show`,
  `create`, `move`, `rollback`, `log`, `delete`, `report` and `follow`.
- `oak mark follow MARK --run SCRIPT` waits for the mark to move, checks the
  commit out, and runs the script for each move. Run as one of the mark's
  followers, the script gets the move's grant and `OAK_CI_*` variables. Only
  the newest move runs, and a state file makes restarts safe.
- `oak mark report` files a report on a commit under a name.
- See https://oak.space/docs/marks.

## Removed

- Oak CI: `oak ci` and `oak workflow` now print one line pointing at
  `oak mark`. Nothing runs on the server after a merge. `oak start` begins at
  `main`'s head (its `--good` and `--tip` flags are gone).
- `oak mount` and its subcommands (`list`, `finish`, `end`, `forget`), the
  Oak Mount app for macOS, and the FUSE (Linux) and ProjFS (Windows) backends.
  `oak upgrade` no longer updates the Oak Mount app, and releases no longer
  include it. Use `oak clone --shallow` (or `--path`) for a separate checkout
  per task.
- The `oak-vcs` skill's mounts and CI references. Its merging reference
  covers marks.
- From `oakvcs-core`: `Repository::open_relaxed`.

## Compatibility

This release goes with the current oak.space server, which has marks and no
longer runs CI. A mount made by an older version keeps working until it's
ended with that version; 0.115.0 doesn't know about it. On macOS, the Oak
Mount app can be removed from Applications.
