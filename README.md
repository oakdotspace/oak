# Oak

This repository is the open-source heart of [Oak](https://oak.space):
**version control at the speed of agents**. It's developed as a Cargo
workspace: a reusable VCS library plus the `oak` command-line client that
agents drive.

Bring your own agent (Claude Code, Codex, Cursor, …); Oak is the foundation
it reads, writes, branches, and collaborates through. The substrate is shaped
around how agents actually work — branch-per-session as the unit of work,
branch descriptions in place of per-commit messages, and content-addressed
lazy mounts that get an agent editing any repo in seconds. Because it's
content-addressed and hydrates on demand, it's also far faster than git for
agent workloads — but the speed is a consequence of the design, not the pitch.

| Crate | Path | crates.io | What it is |
|-------|------|-----------|------------|
| `oakvcs-core` | [`core/`](core/) | [`oakvcs-core`](https://crates.io/crates/oakvcs-core) | The VCS foundation: BLAKE3 content hashing, content-defined chunking, diff/merge, the Blob/Manifest/Commit/Tree data model, and an optional client-side local repository (SQLite + git backends). |
| `oakvcs-cli` | [`cli/`](cli/) | [`oakvcs-cli`](https://crates.io/crates/oakvcs-cli) | The `oak` binary that builds on `oakvcs-core`. |

## Using the library in your own project

`oakvcs-core` is usable on its own — e.g. to build an Oak integration into
another tool or engine. Pull in just the content-addressed data model and
hashing (no SQLite/git) with default features off:

```toml
[dependencies]
oakvcs-core = { version = "0.99.0", default-features = false }
```

The crate is published as `oakvcs-core` but imported as `oak_core`.

Add the default `local-repo` feature when you also want the on-disk
`Repository` (SQLite + read-only git) backends.

## Installing the CLI

Oak is in **public beta** (v0.99.0). The quickest way in is the prebuilt
`oak` binary:

```bash
curl -fsSL oak.space/install | sh
```

The installer supports **macOS (Apple Silicon)** and **Linux (x86_64)**.
After install, `oak upgrade` updates the binary in place.

Prefer to build from crates.io? Install with Cargo instead:

```bash
cargo install oakvcs-cli   # builds and installs the `oak` binary
```

## Building from source

```bash
cargo build --workspace        # builds oak-core + the oak binary
cargo test  -p oakvcs-cli      # CLI tests (incl. wiremock HTTP tests)
make build                     # release build + the CLI release tooling
make release-proof             # non-mutating launch/release readiness proof
```

The CLI depends on `oak-core` via an in-workspace path, so a plain
`cargo build` works against the local `core/` checkout with no extra setup.
See [`docs/release-readiness.md`](docs/release-readiness.md) for the release
proof and crates.io publish-order checks.

## License

Apache-2.0. See [LICENSE](LICENSE).

## AI

This repo was written almost entirely using AI with human oversight. If you see anything that needs fixed or would like to contribute, please email zach@oak.space or reach out on [Discord](https://discord.gg/UUPfUaeDnS).
