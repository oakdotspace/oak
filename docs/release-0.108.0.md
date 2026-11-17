# Oak 0.108.0

Oak 0.108.0 is a minor release of the `oak` CLI and the `oakvcs-core`
library. It moves every dependency to its latest major version, and it makes
`oak ci wait` and `oak ci status` stop pulling CI logs on every poll. JSON
changes are additive within `schema_version` 1.

Released source: oak/oak main at the version-bump commit (notes cover main
through `4d7edb20`). The previous stable release, v0.107.0, was built from
oak/oak main `fed5591f`.

## `oak ci wait` and `oak ci status` poll without logs

- `oak ci wait` used to fetch each run once a second with the full log of
  every job, though it only needs the status. Polls now ask for
  `?logs=false`, so the server stops re-reading and resending megabytes of
  logs per wait. Servers that predate the parameter ignore it and send logs
  as before.
- `oak ci status` reads its run the same way.
- The `--json` wait document still embeds each run with its step logs: once
  the wait settles, it re-reads each observed run once with logs. If that read
  fails, times out, or returns a different run, commit, or status, it keeps
  the log-free observation. `--summary` and plain-text output never needed
  the logs.

## Dependencies

Every direct dependency is on its latest version, so `oakvcs-core` and the
Oak server now share crate versions. Notable bumps: rusqlite 0.41, sha2 0.11,
gix 0.90, zip 9, zstd 0.14, fastcdc 5, diffy 0.5, and windows 0.62.

- fuser stays on 0.15 for now. 0.18 rewrites the Linux mount API, and the port
  ships separately once it's been tested on Linux.
- windows-core stays on 0.62, because 0.100 needs a newer Rust than the
  pinned 1.94.

## Library (`oakvcs-core`)

- `oakvcs-core` is published to crates.io again at 0.108.0. The previous
  published version was 0.102.1.
- Hashes and on-disk formats are unchanged.
- rusqlite's `DatabaseName::Main` is gone, so code that uses the `local-repo`
  storage layer directly should pass `rusqlite::MAIN_DB` instead.
