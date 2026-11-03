# Oak 0.107.0

Oak 0.107.0 is a minor release of the `oak` CLI. It adds:

- a nightly update channel: `oak upgrade --channel nightly|stable` and
  `oak upgrade --canary`
- `oak ci approve`: release a CI run held because its author can't read every
  path under `.oak/PERMISSIONS`
- `oak branch close`, an alias for `oak close`

It also hardens sparse checkouts and server-withheld (restricted) content,
fixes cold reads through `oak mount`, tracks `.oak/PERMISSIONS` like
`.oak/attributes`, and hardens path handling. JSON changes are additive within
`schema_version` 1.

Released source: oak/oak main at the version-bump commit (notes cover main
through `a868cb0a`). The previous stable release, v0.106.0, was built from
oak/oak main `b8d19ad7`.

**`oak upgrade` now reads oak.space only.** Clients from 0.107.0 on, along
with their daily update check, take every channel from oak.space
(`OAK_URL`). Clients up to 0.106.0 still read GitHub's `/releases/latest`, so
this release is published on GitHub too, and later releases must be until
those clients have upgraded.

## Nightly channel

- **`oak upgrade --channel nightly`** switches to the daily build of the tip
  of `main` and remembers it for later upgrades and the update notice;
  `--channel stable` switches back. **`--canary`** installs the current
  nightly once without changing the channel (`nightly-builds`).
- Nightlies are signed with the release key. The client installs one only
  after it verifies against the signature whose trusted comment names its
  version and build.
- A nightly binary identifies itself in `oak --version`
  (`0.107.0 (nightly YYYYMMDD.<commit>)`), and `oak environment --json`
  reports the channel and build id.

## Sparse checkouts and restricted content

- **Sparse cone changes** (`oak sparse set|add|disable`) refuse, changing
  nothing, while uncommitted or untracked files sit in the old or new cone.
  They also refuse when content entering the cone can't be downloaded
  (`sparse-restricted-hardening`).
- **Out-of-cone paths:** `oak status` and `oak commit` warn about out-of-cone
  files on disk, and `oak commit <path>` outside the cone is an error. A
  conflict on an out-of-cone path stops with a hint to widen the cone.
- **Restricted (server-withheld) files** keep their names and hashes. Clone,
  pull and switch skip them with an access message, status and commit never
  treat them as deleted, and commits carry them forward unchanged. Once access
  is granted, a plain `oak pull` downloads them.
- **`oak ci approve RUN_ID --commit HASH`** releases a run the server held
  because its author can't read every path. `oak ci status`, `oak merge
  --wait` and `oak ci wait` say when a run is held.

## Fixes

- **Cold-mount reads** through `oak mount` could return a transient EIO or a
  silently empty file, and status could report an added file as modified
  (`fix-mount-cold-reads`).
- **Commit hooks and `oak split`:** two bugs that new tests exposed are fixed
  (`test-coverage-gaps`).
- **`.oak/PERMISSIONS`** is now tracked like `.oak/attributes`. Before, the
  CLI ignored it, so it could only be changed in the web editor
  (`zach-3c51c8`).
- **Path handling** in the CLI is hardened (`security-hardening-2026-10`).
- **Dependencies** are bumped to clear security advisories
  (`bump-deps-security-advisories`).
- **Performance:** a benchmark of oak against git
  (`scripts/bench-vs-git`), and fixes for what made oak slower
  (`perf-vs-git`).

## New commands

- **`oak branch close [NAME]`** is an alias for `oak close`, with the same
  arguments, output and exit codes (`branch-close-alias`).

## Compatibility and behaviour changes

- `oak upgrade` and the update check no longer read GitHub (see above).
- No protocol change. Every client change works against the currently
  deployed oak.space.

## Also in this range

These don't change the shipped CLI:

- The nightly pipeline on Oak CI: `.oak/workflows/nightly.yml`, the
  `nightly` environment and the schedule LaunchAgent (`nightly-builds`,
  `zach-655a84`).
- Release pipeline fixes: provisioning profiles installed under Xcode's own
  UUID names (`release-profiles-xcode-sync`), macOS xattrs stripped from the
  Linux toolchain image's build context (`release-linux-image-no-xattrs`).
- A test fix for the native-CI budget test (`ci-cache-test-actions-cache`).

## Release proof and publication

Run `make release-proof` from a complete source checkout of the release commit
(see [release readiness](release-readiness.md)). Then deploy the `production`
environment on Oak CI (`oak ci deploy -e production --wait 150`, or
Settings → Environments → Deploy). The run builds, signs, notarizes, stages,
verifies, promotes, and flips the GitHub draft live. This release ships
through GitHub Releases, oak.space and `oak upgrade`.
