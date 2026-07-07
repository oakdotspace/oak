# Oak CLI

`oak` is the command-line client for [Oak](https://oak.space) — version
control at the speed of agents. It is shaped around how agents work:
branch-per-session, local checkpoints, explicit publishing,
content-addressed storage, large-file support, and lazy mounts.

This repository contains the **client** side of Oak: the `oak` binary and
the local crates it builds on. The hosted server and web UI are not part
of this repository.

## Structure

This repository **is** the `oakvcs-cli` package — the repo root is the crate
that defines the `oak` binary. It has a single dependency of note:

| Crate                        | Description                                                                 |
| ---------------------------- | --------------------------------------------------------------------------- |
| `oakvcs-cli` (this repo's root) | The `oak` command-line application (clap-based). Defines the `oak` binary.  |
| [`oakvcs-core`](https://crates.io/crates/oakvcs-core) (external) | Core VCS logic **and** the client-side local repository: BLAKE3 hashing, content-defined chunking, diff/merge, the `Blob`/`Manifest`/`Commit`/`Tree` data model, and the `Repository` trait with its local SQLite + read-only git backends. Published on crates.io (imported as `oak_core`) and shared with the Oak server. |

`oakvcs-core` is consumed from crates.io. The storage it provides is
**client-only** — it depends on neither `sqlx` nor any server data model.
Oak's server-side async/PostgreSQL storage lives in a separate, private tree.

## Installing

Oak is in **public beta** (v0.101.0). Install the prebuilt `oak` binary:

```bash
curl -fsSL oak.space/install | sh
```

Supported platforms: **macOS (Apple Silicon)** and **Linux (x86_64)**. After
install, run `oak upgrade` to update in place.

### From source

Requires a Rust toolchain (see [`rust-toolchain.toml`](rust-toolchain.toml)).

```bash
cargo install --path .             # build and install the `oak` binary to ~/.cargo/bin
make install                       # same, via the workspace Makefile
```

Or just build it without installing:

```bash
cargo build --release              # release binary at target/release/oak
cargo test                         # run the test suite
```

### Lazy mounts

`oak mount` exposes a remote repository as a virtual filesystem, hydrating
files on demand. It is **always built in** — there is no feature flag — and
selects a platform-native backend at compile time:

- **macOS** — [FSKit](https://developer.apple.com/documentation/fskit). **No
  kernel extension** — no macFUSE, no `libfuse`. Requires macOS 26+ and the
  signed **Oak Mount** app (which carries the `OakFS` file-system extension)
  installed and enabled once in System Settings → General → Login Items &
  Extensions → File System Extensions. Build it with `make macos-app` (see
  `macos/OakFS/README.md`).
- **Linux** — FUSE via the `fusermount3` helper from the `fuse3` package
  (no `libfuse` link). Needs `/dev/fuse`. The release binaries are built on a
  native Linux host (the libfuse-free `fuser` build does not cross-compile).

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
