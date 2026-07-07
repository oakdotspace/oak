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
oakvcs-core = { version = "0.101.0", default-features = false }
```

The crate is published as `oakvcs-core` but imported as `oak_core`.

Add the default `local-repo` feature when you also want the on-disk
`Repository` (SQLite + read-only git) backends.

## Installing the CLI

Oak is in **public beta** (v0.101.0). The quickest way in is the prebuilt
`oak` binary:

```bash
curl -fsSL oak.space/install | sh
```

The installer supports **macOS (Apple Silicon)** and **Linux (x86_64)**.
After install, `oak upgrade` updates the binary in place.

### Windows (x86_64)

The `curl … | sh` installer is Unix-only. On Windows, grab the prebuilt
`oak-windows-x86_64.exe` from the [latest GitHub
release](https://github.com/oakdotspace/oak/releases/latest) (rename it to
`oak.exe` and put it on your `PATH`), or build from crates.io with
`cargo install oakvcs-cli`. `oak upgrade` then updates it in place.

`oak mount` on Windows uses the **Projected File System (ProjFS)**, an optional
Windows feature. Enable it once per machine from an elevated PowerShell:

```powershell
Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart
```

(or Settings → Apps → Optional features → "Windows Projected File System").
Everything else — clone, push, pull, commit — works without it.

Prefer to build from crates.io? Install with Cargo instead (works on macOS,
Linux, and Windows — the TLS stack uses rustls + `ring`, so no C/NASM build
toolchain is required):

```bash
cargo install oakvcs-cli   # builds and installs the `oak` binary
```

## Working with large monorepos

Two ways to avoid pulling a whole monorepo:

- **Lazy mounts** — `oak mount <org>/<repo>` puts a working tree on top of the
  remote and hydrates files on demand (FSKit on macOS, FUSE on Linux, ProjFS on
  Windows). Best default for very large repos.
- **Sparse (partial) clones** — Perforce-style, when you want a plain on-disk
  checkout scoped to a subtree:

  ```bash
  oak clone acme/monorepo --path services/api --path libs/shared
  oak sparse add libs/proto   # widen the cone
  oak sparse disable          # back to a full checkout
  ```

  Only files under the cone are downloaded and written; the rest of the tree
  is listed but its content is withheld, and commits carry the out-of-cone
  paths forward untouched (narrowing never deletes them). The same
  withhold-content mechanism powers server-side **path permissions**
  (directory-level read access set by repo admins). `OAK_ALLOW_PARTIAL_CLONE=1`
  is a separate recovery flag that skips, rather than errors on, blobs a broken
  server failed to ship.

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
