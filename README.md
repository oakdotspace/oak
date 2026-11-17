# Oak

This repository is [Oak](https://oak.space), version control built for you
and your agents. The root is a Cargo workspace: a reusable VCS library and the
`oak` command-line client. The server, web app and public site live in
[`server/`](server/), a workspace of its own.

Oak doesn't run agents. Bring your own (Claude Code, Codex, Cursor, …) and it
works through the `oak` CLI. A branch is one unit of work, usually one agent
session. The branch description takes the place of per-commit messages, and
shallow and sparse clones let you start on a big repo without pulling all of it.

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
oakvcs-core = { version = "0.114.0", default-features = false }
```

The crate is published as `oakvcs-core` but imported as `oak_core`.

Add the default `local-repo` feature when you also want the on-disk
`Repository` (SQLite) backend.

## Installing the CLI

Oak is in **public beta** (v0.114.0). The quickest way in is the prebuilt
`oak` binary:

```bash
curl -fsSL oak.space/install | sh
```

The `sh` installer supports **macOS (Apple Silicon and Intel)** and **Linux
(x86_64)** — it picks the native binary for the machine it runs on. After
install, `oak upgrade` updates the binary in place.

To run nightly builds of the tip of `main` instead of tagged releases, switch
your update channel once:

```bash
oak upgrade --channel nightly   # remembered: oak upgrade and the update notice now follow nightly
oak upgrade --channel stable    # back to tagged releases
```

Nightlies are built on our release Mac and served from oak.space at most once a day
(only when something landed). They're signed with the same release key as
stable and identify themselves in `oak --version`
(`0.106.0 (nightly 20261027.b8d19ad7)`).

Linux ARM64 binaries are published too, but the installer doesn't select them
yet; download the Linux ARM64 build below for now.

### Downloads

Each link serves the latest stable `oak` for that platform:

| Platform | Download |
|---|---|
| macOS, Apple Silicon | [oak-darwin-arm64](https://oak.space/api/releases/latest/darwin-arm64) |
| macOS, Intel | [oak-darwin-x86_64](https://oak.space/api/releases/latest/darwin-x86_64) |
| Linux x86_64 | [oak-linux-x86_64](https://oak.space/api/releases/latest/linux-x86_64) |
| Linux ARM64 | [oak-linux-arm64](https://oak.space/api/releases/latest/linux-arm64) |
| Windows x86_64 | [oak-windows-x86_64.exe](https://oak.space/api/releases/latest/windows-x86_64) |

Add `/sha256` or `/minisig` to a link for its checksum or minisign signature,
and swap `latest` for a version (`v0.114.0`) to pin one. Make the file
executable, rename it `oak` (`oak.exe` on Windows) and put it on your `PATH`.
What changed in each release is in [`docs/`](docs/) as `release-X.Y.Z.md`.

### Windows (x86_64)

The `curl … | sh` installer is Unix-only; Windows has a PowerShell
counterpart:

```powershell
irm https://oak.space/install.ps1 | iex
```

It installs `oak.exe` to `%USERPROFILE%\.local\bin` and adds that directory to
your user `PATH`. You can also take `oak-windows-x86_64.exe` from
[Downloads](#downloads), or build from crates.io with
`cargo install oakvcs-cli`. `oak upgrade` then updates it in place.

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
branches off main, messageless commits, branch descriptions, marks) so it
stops reaching for `git status`:

```bash
oak skill install            # into this repo's .claude/skills/ — commit it
oak skill install --global   # into ~/.claude/skills/ for every project
```

The files are baked into the binary, so the installed skill always documents
the CLI version that wrote it; re-run after `oak upgrade` to refresh it. The
source of truth is
[`cli/src/commands/skill/oak-vcs/`](cli/src/commands/skill/oak-vcs/).

A task starts and ends with two commands:

```bash
oak start "fix the login redirect"   # branch from main, described, pushed
oak finish --desc-file desc.md       # describe, checkpoint, publish
```

## Using Oak from an MCP client

`oak mcp` is an [MCP](https://modelcontextprotocol.io) server over stdio, for
agents that reach tools through MCP rather than a shell. Start it in the repo.
With Claude Code:

```bash
claude mcp add oak -- oak mcp
```

The tools are read-only: status, diffs, history, branch triage and review,
and marks. `oak mcp --allow-write` adds commit, set description and push.
Merging and closing a branch are never
offered, so they stay a deliberate step.

## Marks

Oak runs none of your code on the server: test locally, and merge once you're
up to date with main. A **mark** records what has passed what: a named pointer
to a commit on `main` that people and bots move forward (`server-good`,
`server-prod`, `cli-released`). A mark can require others to be at or past a
commit before it moves there, and a deploy is a mark a machine follows.

```bash
oak mark list                                   # each mark and how far behind main it is
oak mark move server-good main --note "tests pass"
oak mark follow server-prod --run scripts/deploy.sh   # on the deploy machine
oak report main --name tests --body "412 passed"      # say how it went
```

A report is free text about a commit under a name; its first line shows on
History. See [the marks docs](https://oak.space/docs/marks) and
[reports](https://oak.space/docs/reports).

## Working with large monorepos

Two ways to avoid pulling a whole monorepo, and they combine:

- **Shallow clones** — `oak clone <org>/<repo> --shallow` takes only the newest
  commit, with the full working tree and none of the history.
- **Sparse (partial) clones** — Perforce-style, when you want a plain on-disk
  checkout scoped to a subtree:

  ```bash
  oak clone acme/monorepo --path services/api --path libs/shared
  oak sparse add libs/proto   # widen the cone
  oak sparse disable          # back to a full checkout
  ```

  Add `--json` to a named Oak clone for one machine-readable acquisition
  receipt. It records the requested history and cone, the resulting exact
  head/manifest, observed object and unique chunk counts, materialization
  counts, and explicitly unavailable transport/disk measurements. Legacy
  fallback phases remain visible rather than being folded into a false
  single-pass claim.

  Only files under the cone are downloaded and written; the rest of the tree
  is listed but its content is withheld, and commits carry the out-of-cone
  paths forward untouched (narrowing never deletes them). The same
  withhold-content mechanism powers server-side **path permissions** —
  directory-level access set on the server as path rules owned by
  groups (see `oak access`, `oak owners` and `oak group`).
  `OAK_ALLOW_PARTIAL_CLONE=1` is a separate recovery flag that skips, rather
  than errors on, blobs a broken server failed to ship.

Every clone first negotiates a bounded metadata integrity profile; merely
being logged in does not trigger object-store probes. If the proof runs out
of its history/tree/path or wall-time budget, the result is *inconclusive*
(exit 8), not a content failure: Oak never narrows or waives on its own, and
recommends the exact `--shallow` retry first. When the server issued a
snapshot pin for that inconclusive proof, it also offers the explicit
`--allow-unverified-integrity` retry, which proceeds without proving
completeness while the pull still verifies transferred hashes. `--allow-legacy-scope` is the separate, explicit compatibility waiver
for an accessible older server that cannot prove branch/depth/sparse scope.
Use `oak doctor --repo ORG/REPO --verify metadata --json` for a cheap bounded
diagnostic and the platform-admin `oak doctor blob HASH --repo ORG/REPO --json`
for bounded target byte evidence (`--depth N --branch NAME` selects a wider
explicit history scope).

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

Apache-2.0. See [LICENSE](LICENSE). The server in [`server/`](server/) is
AGPL-3.0-only; see [server/LICENSE](server/LICENSE).

## AI

This repo was written almost entirely using AI with human oversight. If you see anything that needs fixed or would like to contribute, please email zach@oak.space or reach out on [Discord](https://discord.gg/UUPfUaeDnS).
