# Oak

**Version control for you and your agents.**

This repository is the open-source heart of [Oak](https://oakvcs.com): the
reusable version-control library and the `oak` command-line client, developed
together as a Cargo workspace.

| Crate | Path | crates.io | What it is |
|-------|------|-----------|------------|
| `oakvcs-core` | [`core/`](core/) | [`oakvcs-core`](https://crates.io/crates/oakvcs-core) | The VCS foundation: BLAKE3 content hashing, content-defined chunking, diff/merge, the Blob/Manifest/Commit/Tree data model, and an optional client-side local repository (SQLite + git backends). |
| `oakvcs-cli` | [`cli/`](cli/) | [`oakvcs-cli`](https://crates.io/crates/oakvcs-cli) | The `oak` binary that builds on `oak-core`. |

## Using the library in your own project

`oak-core` is usable on its own — e.g. to build an Oak integration into another
tool or engine. Pull in just the content-addressed data model and hashing
(no SQLite/git) with default features off:

```toml
[dependencies]
oak-core = { package = "oakvcs-core", version = "0.94.0", default-features = false }
```

Add the default `local-repo` feature when you also want the on-disk
`Repository` (SQLite + read-only git) backends.

## Installing the CLI

```bash
cargo install oakvcs-cli   # installs the `oak` binary
```

## Building from source

```bash
cargo build --workspace        # builds oak-core + the oak binary
cargo test  -p oakvcs-cli      # CLI tests (incl. wiremock HTTP tests)
make build                     # release build + the CLI release tooling
```

The CLI depends on `oak-core` via an in-workspace path, so a plain
`cargo build` works against the local `core/` checkout with no extra setup.

## License

Apache-2.0. See [LICENSE](LICENSE).
