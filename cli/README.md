# Oak CLI

`oak` is the command-line client for [Oak](https://oak.space), version
control built for you and your agents. It has branch-per-session work, local
checkpoints, explicit publishing, content-addressed storage, and large-file
support.

This folder is the `oakvcs-cli` package, the crate that defines the `oak`
binary. It sits in the workspace at the repository root next to the library
it builds on; the server and web UI are in `../server/`.

## Structure

| Crate                        | Description                                                                 |
| ---------------------------- | --------------------------------------------------------------------------- |
| `oakvcs-cli` (this folder) | The `oak` command-line application (clap-based). Defines the `oak` binary.  |
| [`oakvcs-core`](../core/) | Core VCS logic **and** the client-side local repository: BLAKE3 hashing, content-defined chunking, diff/merge, the `Blob`/`Manifest`/`Commit`/`Tree` data model, and the `Repository` trait with its local SQLite backend. Published on crates.io (imported as `oak_core`) and shared with the Oak server. |

`oakvcs-core` is the sibling `../core` crate, linked by path inside the
workspace (and by version on crates.io). The storage it provides is
**client-only** — it depends on neither `sqlx` nor any server data model.
Oak's server-side PostgreSQL storage lives in `../server/crates/oak-storage`.

## Installing

Oak is in **public beta** (v0.114.0). Install the prebuilt `oak` binary:

```bash
curl -fsSL oak.space/install | sh
```

The installer supports **macOS (Apple Silicon and Intel)** and **Linux
(x86_64)**; Windows has `irm https://oak.space/install.ps1 | iex`. After
install, run `oak upgrade` to update in place.

### From source

Requires a Rust toolchain (see [`rust-toolchain.toml`](../rust-toolchain.toml)).

```bash
cargo install --path .             # build and install the `oak` binary to ~/.cargo/bin
make install                       # same, via the workspace Makefile
```

Or just build it without installing:

```bash
cargo build --release              # release binary at target/release/oak
cargo test                         # run the test suite
```

## Default server

`oak` talks to [oak.space](https://oak.space) by default. Point it at a
different server with `oak login -r <url>` or the `-r/--remote` flag on the
sync commands.

## Getting started

```bash
oak init                 # initialize a repository in the current directory
oak commit               # create a local checkpoint
oak push --repo <org>/<name>   # link/create the repo and publish your branch
```

Plain `oak commit` never publishes. For headless agents or CI,
`oak commit --json --quiet [--push]` reports checkpoint/publish state, and
`oak push --repo <org>/<name>` is the non-interactive first-publish path. It
links the local repo to that org/name and creates the server repo on first
push if it does not already exist.

Run `oak --help` for the full command list, or `oak <command> --help` for
details on any command. Docs live at <https://oak.space/docs>.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
