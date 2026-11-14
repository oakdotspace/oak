# Oak 0.111.0

Oak 0.111.0 is a minor release of the `oak` CLI and the `oakvcs-core` library.
It tracks a project's own CI workflows, adds `oak mcp`, and ships
`oakvcs-engine`, the shared library for the game-engine extensions.

Object hashes are unchanged, and there's no SQLite migration.

Released source: oak/oak main at the version-bump commit. The previous release,
0.110.0, was built from oak/oak main `274889b0`.

## Project workflows

`oak` now commits and checks out `<project>/.oak/workflows/`, so a project's CI
can live next to its code. The rest of a nested `.oak` directory stays
untracked, and only files directly in that `workflows` directory count.

Earlier versions refuse these paths, so a full clone of a repo that has them
fails with `refusing to write`. Upgrade before committing a project workflow.

## `oak mcp`

`oak mcp` runs an MCP server over stdio, with read-only tools. `--allow-write`
adds commit, desc and push.

## `oakvcs-engine`

`oakvcs-engine` is the shared library the game-engine extensions build on, with
a C interface. Each release attaches `oak-engine-<target>` libraries,
`oak_engine.h` and `oak-engine-fixtures.tar.gz`. See
[engine-contract.md](engine-contract.md).

## Removed in 0.110.0

0.110.0 removed Oak spaces, including `oak space`, after its notes were written.
