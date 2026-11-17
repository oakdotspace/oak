# Releasing the CLI

A stable `oak` release builds, signs and publishes on one Apple Silicon Mac,
the release Mac. It follows oak/oak's `cli-released` mark: moving the mark to
a commit on `main` releases that commit. The follower
([`server/ops/marks/cli-release-follow.sh`](../server/ops/marks/cli-release-follow.sh),
run by `oak mark follow cli-released`) runs every phase of
[`scripts/release-oak-ci.sh`](../scripts/release-oak-ci.sh) at the move's
commit, then reports `live` or `failed` on it with the end of the log. One
release produces everything it ships, for five platforms (darwin-arm64,
darwin-x86_64, linux-x86_64, linux-arm64, windows-x86_64):

| Artifact | Built by |
|---|---|
| `oak-darwin-arm64`, `oak-darwin-x86_64` | `make build-release-macos`, then Developer ID codesign |
| `oak-linux-x86_64`, `oak-linux-arm64` | `make build-release-linux` in a Linux container on the Mac's colima ([`scripts/release-linux.Dockerfile`](../scripts/release-linux.Dockerfile)) |
| `oak-windows-x86_64.exe` | `make build-release-windows-xwin` (cargo-xwin cross-compile) |
| `*.minisig`, `SHA256SUMS` | `make sign-release`, checked against the CLI's `RELEASE_PUBKEY` |

Publishing goes to oak.space's CLI distribution (`/api/releases`): the
binaries are staged, the bytes oak.space serves are checked against this
build, the version is promoted, and the promotion is confirmed. `oak upgrade`,
its daily update check and both installers read it. The release notes are
`docs/release-X.Y.Z.md` in this repo, and the README links each platform's
latest download (`https://oak.space/api/releases/latest/<platform>`). Nothing
else is published: there's no release page and no GitHub release.

The script keeps its old name. It reads `OAK_COMMIT`, `OAK_MOVE_ID` and
`OAK_LINE`, which the follower sets for the move, and falls back to the
deprecated `OAK_CI_COMMIT`, `OAK_CI_RUN_ID` and `OAK_CI_BRANCH` that older
followers set.

CLIs at 0.106.0 and older check GitHub's latest release for updates, which no
longer moves, so they need the installer once to reach a current version.

## The marks

oak/oak's CLI marks (runbook 17 creates them):

- `cli-good`: commits whose CLI tests passed (whoever ran them moves it).
- `cli-approved`: requires `cli-good`; only its approvers move it.
- `cli-released`: requires `cli-approved`; owned by the `release` group. The
  release Mac follows it.

A move to a commit the marks it requires haven't reached is refused, and the
refusal names them.

## Nightly builds

A launchd timer on the release Mac
([`server/ops/marks/cli-nightly.sh`](../server/ops/marks/cli-nightly.sh),
`space.oak.cli-nightly.plist`) builds what `cli-good` points at (main's head
when there is no such mark) once a day with the same script
(`RELEASE_CHANNEL=nightly`). It ships every target, stamped
`OAK_BUILD_CHANNEL=nightly` and `OAK_BUILD_ID=YYYYMMDD.<first 8 hex of the Oak
commit>`, and minisigned with the release key under the trusted comment
`oak-nightly version=vX.Y.Z build=<id> commit=<Oak commit>`. It stages them on
oak.space as the release version `nightly`, replacing the previous night's,
and never promotes it, so `/api/releases/latest`, `install.sh` and stable
`oak upgrade` never see it. When every platform already serves a nightly of
the commit, the build stops after the preflight. To build one now:
`launchctl kickstart gui/$(id -u)/space.oak.cli-nightly` on the Mac.
`NIGHTLY_FORCE=true` in the plist's environment rebuilds an unchanged commit.

Users opt in with `oak upgrade --channel nightly`. The client fetches
`/api/releases/nightly/<platform>/minisig`, reads the version and build from
its trusted comment, and installs the binary only after it verifies against
that signature.

## One-time setup

1. **The toolchain.** On the Mac, as the release user (Xcode Command Line
   Tools, Homebrew, rustup): `scripts/release-mac-setup.sh`. It installs
   colima + docker, llvm, cargo-xwin, minisign, jq and the darwin and
   windows-msvc rust targets, and checks for the `oak` CLI. Keep the Mac awake
   on power: a release waits until it wakes.
2. **The follower and the timer:** runbook 17, step 6
   (`server/ops/runbooks/17-marks-cutover.md`, internal). The secrets are files
   in `~/.config/oak-release/secrets/`, each going only to the phases that use
   it:

   | Secret | Value |
   |---|---|
   | `OAK_ADMIN_API_KEY` | oak.space admin key (release staging/promote) |
   | `MINISIGN_SECKEY`, `MINISIGN_PASSWORD` | The release minisign key file's contents and passphrase |
   | `MACOS_CERT_P12_BASE64`, `MACOS_CERT_PASSWORD` | `base64 -i cert.p12`, and its export password |
   | `MACOS_KEYCHAIN_PASSWORD` | Any random string (the ephemeral keychain's password) |
   | `MACOS_SIGN_IDENTITY` | Optional: the cert's SHA-1, as a cross-check |

   `RELEASE_DRY_RUN=true` in the follower's plist environment builds and signs
   without publishing.

## Each release

1. **Bump** `[workspace.package].version` in `Cargo.toml`, add the release
   notes as `docs/release-X.Y.Z.md` (they become the release's notes), and
   merge to `main`.
2. **Move the marks** once the CLI tests pass at that commit:

   ```sh
   oak mark move cli-good <commit> --repo oak/oak --note "cli tests pass"
   oak mark move cli-approved <commit> --repo oak/oak    # an approver
   oak mark move cli-released <commit> --repo oak/oak
   ```

   The release Mac picks up the `cli-released` move, reports `deploying`, and
   then `live` or `failed`. `oak mark log cli-released --repo oak/oak` shows
   the move with its reports; the follower's log is in `~/oak-runner/logs/`.
3. **Verify distribution:** `curl -fsSL https://oak.space/install | sh` on a
   clean machine, `oak upgrade` from the previous version, and
   `oak --version`, and each README download link
   (`https://oak.space/api/releases/latest/<platform>`) serving the new version.
4. **Publish the crates** by hand, from a checkout at the release commit with
   a crates.io token: `cargo publish -p oakvcs-core`, wait for crates.io's
   index to show it, then `cargo publish -p oakvcs-cli`
   (`scripts/release-proof.sh` checks the versions line up first).

## When a release fails

Move `cli-released` to the same commit again: each move is a fresh run. The
preflight reads oak.space's promotion state before building anything and
picks the mode (`release_decide_mode` in `scripts/release-state.sh`):

| oak.space | Mode |
|---|---|
| unpromoted | **fresh**: build, sign, stage, verify, promote |
| promoted | **already published**: confirm the promotion; nothing is rebuilt |

A failure before promotion leaves an unpromoted staging release on oak.space.
Nothing is public, and a fresh run replaces it.

The follower removes the signing keychain when it exits. A run killed outright
can leave its keychain behind; the next run's signing step
(`scripts/macos-signing-keychain.sh create`) deletes it first.
