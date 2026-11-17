# Oak 0.108.1

Oak 0.108.1 is a patch release of the `oakvcs-core` library and the `oak`
CLI. It moves rusqlite back from 0.41 to 0.39 so the Oak server can depend on
`oakvcs-core` alongside sqlx 0.9. Nothing else changes.

Released source: oak/oak main at the version-bump commit. The previous
release, 0.108.0, was built from oak/oak main `8a21d468`.

## rusqlite 0.39

sqlx 0.9 accepts `libsqlite3-sys` below 0.38, and rusqlite 0.41 needs 0.39.
Only one crate in a dependency graph may link the native sqlite library, and
Cargo enforces that across optional dependencies too. So any crate that
depends on both `oakvcs-core` 0.108.0 and sqlx 0.9 fails to resolve, even
when neither crate turns its sqlite feature on. rusqlite 0.39 uses
`libsqlite3-sys` 0.37, which both accept.

- The `local-repo` storage layer's API is unchanged: `rusqlite::MAIN_DB` and
  the `fallible_uint` feature are both in 0.39.
- Move back to rusqlite 0.41 once sqlx accepts `libsqlite3-sys` 0.39.
- Hashes and on-disk formats are unchanged.
