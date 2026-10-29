# Oak 0.106.0

Oak 0.106.0 is a minor release of the `oak` CLI. It adds:

- `oak findings`: review findings on a branch
- `oak ci deploy`: authorize a protected-environment release from the CLI
- a git storage backend, so Oak repos can live on GitHub or any git server
- the collaboration commands against a loopback `oak serve`, not just
  push, pull and clone

It also makes `oak pull`, `oak merge`, `oak switch`, `oak split` and
`oak branch --from` work on trees instead of flattened manifests, fixes a
merge that stored a squash commit whose hash couldn't be reproduced, and fixes
several conflict-handling bugs. `oak release` is removed. JSON changes are
additive within `schema_version` 1.

Released source: oak/oak main at the version-bump commit (notes cover main
through `26151ffa`). The previous stable release, v0.105.0, was built from
oak/oak main `d4d8062d`.

This is the first release built on Oak CI's macOS release runner
([release on Oak CI](release-on-oak-ci.md)) instead of GitHub Actions. Its
Developer ID certificate is new (SHA-1 `F400DBAE…`), so the darwin binaries
and OakMount.app carry a different signing certificate from 0.105.0's. The
team (452XFR864N) and the minisign key are unchanged, so `oak upgrade` and
Gatekeeper accept it as before.

## Merges work on trees

- **`oak pull` and `oak merge` merge trees** instead of flattened manifests
  (`tree-merge-cli`).
- **Hot paths walk trees** rather than flattening whole manifests
  (`manifest-tree-walks`).
- **`oak switch`, `switch -c`, `split` and `branch --from` move the working
  tree by tree diff** (`tree-diff-moves`), and their dirty-tree guards use the
  tree-hash clean check (`tree-clean-guards`).
- **`oak switch NAME` for a branch that isn't local yet** only fetches it,
  then lets the switch's own tree move write the files
  (`switch-remote-fetch-only`).

## Fixes

- **`oak merge` stored main's squash commit with fields that couldn't
  reproduce its hash.** Merges now store a commit that verifies
  (`zach-a56250`).
- **Case-only renames** (`readme.md` → `README.md`) now survive on
  case-insensitive filesystems, including through `merge --continue` and
  `pull --continue` (`case-renames-continue`).
- **`oak pull` auto-committed conflict markers** on files without a trailing
  newline. It no longer does (`pull-conflict-long-line`).
- **`oak conflict take` and the `--continue` gates** now handle diff3
  conflict blocks (`conflict-take-diff3-base`).
- **`oak clone <git-url>`** no longer refuses repos that track files under
  `.oak/` (`zach-git-clone-tracked-oak-dir`).

## New commands and backends

- **`oak findings`** records and lists review findings on a branch
  (`review-findings`).
- **`oak ci deploy -e ENV`** previews the head, workflow and environment
  policy, then authorizes a protected-environment run, with `--yes`,
  `--dry-run`, `--wait` and `--json` (`ci-deploy`).
- **Git storage backend:** host Oak repos on GitHub or any git server
  (`git-storage-backend`).
- **Loopback `oak serve`** now supports the CLI's collaboration commands,
  not just push, pull and clone (`zach-0cdcd5`).

## Compatibility and behaviour changes

- **`oak release` is removed**, along with the CLI's feature-gating
  machinery (`zach-c69205`).
- No protocol change. Every client change works against the currently
  deployed oak.space.

## Also in this range

These don't change the shipped CLI:

- The release pipeline itself: Oak CI workflows and the release runner
  (`oak-ci-release`, `zach-393c3b`, `zach-9d1c18`).
- A VS Code extension in `editors/vscode` (`vscode-extension`).
- A design doc for binary `Hash` values in `oak-core`
  (`binary-hashes-design`).
- A test-only fix that scopes the shared HTTP client pool per Tokio runtime
  in debug builds (`mrmrs-3b28f9`).

## Release proof and publication

Run `make release-proof` from a complete source checkout of the release commit
(see [release readiness](release-readiness.md)). Then deploy the `production`
environment on Oak CI (`oak ci deploy -e production --wait 150`, or
Settings → Environments → Deploy). The run builds, signs, notarizes, stages,
verifies, promotes, and flips the GitHub draft live. This release ships
through GitHub Releases, oak.space and `oak upgrade`.
