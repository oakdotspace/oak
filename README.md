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
oakvcs-core = { version = "0.102.1", default-features = false }
```

The crate is published as `oakvcs-core` but imported as `oak_core`.

Add the default `local-repo` feature when you also want the on-disk
`Repository` (SQLite + read-only git) backends.

## Installing the CLI

Oak is in **public beta** (v0.102.1). The quickest way in is the prebuilt
`oak` binary:

```bash
curl -fsSL oak.space/install | sh
```

The `sh` installer supports **macOS (Apple Silicon and Intel)** and **Linux
(x86_64)** — it picks the native binary for the machine it runs on. After
install, `oak upgrade` updates the binary in place.

Linux ARM64 binaries are published too, but the installer doesn't select them
yet; grab `oak-linux-arm64` from the [latest GitHub
release](https://github.com/oakdotspace/oak/releases/latest) for now.

### Windows (x86_64)

The `curl … | sh` installer is Unix-only; Windows has a PowerShell
counterpart:

```powershell
irm https://oak.space/install.ps1 | iex
```

It installs `oak.exe` to `%USERPROFILE%\.local\bin` and adds that directory to
your user `PATH`. You can also grab the prebuilt `oak-windows-x86_64.exe` from
the [latest GitHub
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

Shell completion is generated on demand — `bash`, `zsh`, `fish`, `elvish`, and
`powershell` are supported. For the current session:

```bash
source <(oak completions bash)
```

Or write the script wherever your shell loads completions from, e.g.
`oak completions bash > ~/.local/share/bash-completion/completions/oak`.

## Teaching your agent to drive Oak

Oak bundles an agent skill in the open [Agent
Skills](https://code.claude.com/docs/en/skills) format — a `SKILL.md` plus
reference files that tell a coding agent how Oak differs from git (flat
branches, messageless commits, branch descriptions, mounts, the CI gate) so it
stops reaching for `git status`:

```bash
oak skill install            # into this repo's .claude/skills/ — commit it
oak skill install --global   # into ~/.claude/skills/ for every project
```

The files are baked into the binary, so the installed skill always documents
the CLI version that wrote it; re-run after `oak upgrade` to refresh it. The
source of truth is
[`cli/src/commands/skill/oak-vcs/`](cli/src/commands/skill/oak-vcs/).

## CI and the merge gate

An Oak server runs CI natively from workflow files at `.oak/workflows/*.yml`,
and merges onto `main` are gated on it: the server refuses a squash-merge
(HTTP 412) while the branch head's CI is red or still in flight. The CLI is the
visibility and recovery surface for that gate:

```bash
oak ci status              # CI for the current branch head — exit 0 pass, 1 fail, 3 running
oak ci runs [--limit N]    # recent runs: id, workflow, branch, commit, status, duration
oak ci logs <run-id>       # step-by-step logs
oak ci rerun <run-id>      # re-dispatch at the same commit, for infra flakes

oak merge --wait           # ride the gate out instead of polling
oak merge --force          # override it, after reading the failure
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
  withhold-content mechanism powers server-side **path permissions** —
  directory-level read access, declared in the repo as a CODEOWNERS-shaped
  `.oak/PERMISSIONS` file rather than configured in the platform.
  `OAK_ALLOW_PARTIAL_CLONE=1` is a separate recovery flag that skips, rather
  than errors on, blobs a broken server failed to ship.

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

## Feedback

`oak feedback` files a feature request or bug report against Oak from the
terminal and prints back a tracking reference (`fb-N`). It takes `-m`, a file,
or stdin, opens `$EDITOR` with a template when given nothing on a TTY, and
exits rather than blocking when there's no terminal — so an agent can file one
mid-task without stalling:

```bash
oak feedback -m "oak diff --print panics on a closed pipe" --json
```

## License

Apache-2.0. See [LICENSE](LICENSE).

## AI

This repo was written almost entirely using AI with human oversight. If you see anything that needs fixed or would like to contribute, please email zach@oak.space or reach out on [Discord](https://discord.gg/UUPfUaeDnS).
