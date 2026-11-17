# Oak 0.116.0

Oak 0.116.0 is a minor release of the `oak` CLI and the `oakvcs-core` library.
Reports on commits become free text: `oak mark report` takes a body instead of
a state and a summary.

Object hashes are unchanged, and there's no SQLite migration.

Released source: oak/oak main at the version-bump commit. The previous release,
0.115.0, was built from oak/oak main `c5e64699`.

## Marks

- `oak mark report COMMIT --name NAME` takes `--body TEXT` or
  `--body-file FILE` (`-` reads stdin) in place of `--state`, `--summary` and
  `--log-file`. A report is free text, usually markdown. Its first line is the
  headline shown beside the commit on History, and the rest is on the
  commit's page. A body over 64 KiB keeps its start and its end.
- There are no report states. Whether a commit passed something is recorded by
  moving a mark; a report says how it went.
- `oak mark follow` reports "deploying (move #N by …)", then "live (move #N)"
  or "failed (move #N)" followed by the end of the script's output.
- `--json` output for reports carries `headline` and `body`. Lists of moves
  carry only `headline`.

## Compatibility

This release needs the current oak.space server: `oak mark report --body` is
refused by a server that still expects report states. The server accepts
reports from 0.115.0's `--state`, `--summary` and `--log-file` and folds them
into a body, so older followers keep working.
