# Releasing on Oak CI

A stable `oak` release builds, signs and publishes on Oak's own CI, on one
Apple Silicon Mac: [`.oak/workflows/release.yml`](../.oak/workflows/release.yml),
which runs the phases of [`scripts/release-oak-ci.sh`](../scripts/release-oak-ci.sh).
One run produces everything a release ships:

| Artifact | Built by |
|---|---|
| `oak-darwin-arm64`, `oak-darwin-x86_64` | `make build-release-macos`, then Developer ID codesign |
| `oak-linux-x86_64`, `oak-linux-arm64` | `make build-release-linux` in a Linux container on the Mac's colima ([`scripts/release-linux.Dockerfile`](../scripts/release-linux.Dockerfile)); fuser needs a Linux host |
| `oak-windows-x86_64.exe` | `make build-release-windows-xwin` (cargo-xwin cross-compile) |
| `OakMount.zip` | `make macos-app`, notarized and stapled |
| `oak-engine-<target>.{dylib,so,dll}` | The same build targets, with `-p oakvcs-engine`; the dylibs are codesigned with the CLI |
| `oak_engine.h`, `oak-engine-fixtures.tar.gz` | `make package-release-engine`, during `build-macos` |
| `*.minisig`, `SHA256SUMS` | `make sign-release`, checked against the CLI's `RELEASE_PUBKEY` |

Publishing has the same order and checks as
[`release-staging.yml`](../.github/workflows/release-staging.yml), with the
repo's own Oak release in place of a GitHub release: a draft release `vX.Y.Z`
on `oak/oak` (tagged at the commit being released, every asset attached),
staging on oak.space, byte-level verification of both channels, promotion,
confirmation, then the release is published and its asset list finalized. The
notes are `docs/release-X.Y.Z.md` when it exists. The release appears at
`https://oak.space/oak/oak/releases`, and `oak release download vX.Y.Z` fetches
its assets. The engine assets ([docs/engine-contract.md](engine-contract.md)) are
on the Oak release only: they aren't staged on oak.space, and they're outside
minisign and `SHA256SUMS` because extensions pin them by content hash. All of
this lives in the Makefile and `scripts/release-oak-ci.sh`, so it needs no
change to `release.yml` or `nightly.yml` and no re-pin. The GitHub workflow
stays as a fallback, without these assets; don't run both for the
same version at once. crates.io publishing
([`.oak/workflows/publish-crates.yml`](../.oak/workflows/publish-crates.yml))
and the nightly channel (below) run on the same Mac.

`oak upgrade`, its daily update check and `install.sh` all read oak.space.
This workflow doesn't publish to GitHub itself. oak.space's GitHub mirror does
that: with **Publish releases to GitHub** on under the repo's Settings →
Export, every published Oak release, nightlies included, appears on
`oakdotspace/oak` with its assets, and a tag deleted here is deleted there.
The sync runs on the server after publishing and never fails this workflow, so
check its status on that settings page. Stable releases become GitHub's Latest,
which keeps clients up to 0.106.0 updating, since they read GitHub's
`/releases/latest`. It leaves releases it didn't create alone, including
`v0.109.1` and older and the `canary` release from `canary.yml`: newer clients'
`oak upgrade --canary` installs the current nightly from oak.space instead.

## Nightly builds

[`.oak/workflows/nightly.yml`](../.oak/workflows/nightly.yml) builds the
newest verified commit on `main` once a day on the release runner, with the same script
(`RELEASE_CHANNEL=nightly`). It ships every target and the notarized app,
stamped `OAK_BUILD_CHANNEL=nightly` and
`OAK_BUILD_ID=YYYYMMDD.<first 8 hex of the Oak commit>`, and minisigned with
the release key under the trusted comment
`oak-nightly version=vX.Y.Z build=<id> commit=<Oak commit>`. It stages them on
oak.space as the release version `nightly`, replacing the previous night's,
and never promotes it, so `/api/releases/latest`, `install.sh` and stable
`oak upgrade` never see it.

Each nightly is also an Oak prerelease, `nightly-YYYYMMDD.<hash>`, with every
artifact attached, so the Releases page lists it. Only the newest seven stay
(the variable `NIGHTLY_KEEP` changes that); the run deletes older ones.
`oak upgrade` doesn't read these tags, so this step can't affect clients. It
runs after the channel has updated: if it fails the run fails, but a retry
stops at "already serves this commit", and the next commit's nightly is the
next prerelease. A run that builds also fails if it can't attempt this step:
without `OAK_RELEASE_API_KEY`, or without the `oak` CLI on the runner's account
(`OAK_BIN`), the preflight warns and publishing fails right after the channel
updates. Fix what's missing, then deploy with `NIGHTLY_FORCE=true` to publish
that build's prerelease.

Users opt in with `oak upgrade --channel nightly`. The client fetches
`/api/releases/nightly/<platform>/minisig`, reads the version and build from
its trusted comment, and installs the binary only after it verifies against
that signature. A nightly binary's first `oak mount` on a Mac installs the
nightly OakMount app.

The `nightly` channel publishes itself on a daily timer at 10:17 UTC (one-time
setup step 4). Timers are in UTC, so this doesn't move with daylight saving. The
timer picks the newest commit on `main` that passed Verify and skips the day when
that commit has already been published, so a red `main` means an older nightly
or none. When every platform already serves a nightly of the commit, the run
stops after the preflight. To publish one now:

```sh
oak ci deploy -e nightly --workflow nightly --yes
```

Set the environment variable `NIGHTLY_FORCE=true` and publish by hand to
rebuild an unchanged `main`, or `RELEASE_DRY_RUN=true` to build and sign without publishing.

## One-time setup

### 1. The runner

On the Mac (full Xcode installed and selected, Homebrew, rustup):

```sh
OAK_RUNNER_TOKEN=<token> scripts/release-runner-setup.sh
```

The token comes from oak.space → the `oak` org's **Settings → Runners**. The
script installs the toolchain (gitea-runner, colima + docker, llvm,
cargo-xwin, minisign, jq, xcodegen, the darwin and windows-msvc rust
targets), checks for the `oak` CLI (install it first with
`curl -fsSL https://oak.space/install | sh`; the release steps use it to
publish the Oak release), registers a host-mode runner labeled `macos` + `arm64`, starts it
from a LaunchAgent at login, and prints its uuid. Keep the Mac awake on power:
a sleeping runner is offline, and a deploy can't be approved without an
online runner.

The runner runs jobs as your user, directly on the Mac. That's acceptable
because it is bound to `production` (next step): a bound runner takes no
ordinary CI, so it only ever runs this reviewed workflow from `main`, at a
commit someone approved.

### 2. The `production` environment

In oak.space → `oak/oak` → **Settings → Environments → production** (an org
owner or admin):

- **Branches:** `main`.
- **Runners:** bind the uuid the setup script printed.
- **Workflow pins:** if `release-macos.yml` is pinned, replace the pin with
  `.oak/workflows/release.yml`, and pin `.oak/workflows/publish-crates.yml`
  too (re-pin after every change to either file).
- **Secrets**, the same values as the GitHub repo's release secrets, plus
  `OAK_RELEASE_API_KEY`:

  | Secret | Value |
  |---|---|
  | `OAK_RELEASE_API_KEY` | oak.space API key with `write` scope whose account (a service account is best) can write `oak/oak`; it creates, publishes and finalizes the release |
  | `OAK_ADMIN_API_KEY` | oak.space admin key (release staging/promote) |
  | `MINISIGN_SECKEY`, `MINISIGN_PASSWORD` | The release minisign key file's contents and passphrase |
  | `MACOS_CERT_P12_BASE64`, `MACOS_CERT_PASSWORD` | `base64 -i cert.p12`, and its export password |
  | `MACOS_KEYCHAIN_PASSWORD` | Any random string (the ephemeral keychain's password) |
  | `MACOS_SIGN_IDENTITY` | Optional: the cert's SHA-1, as a cross-check |
  | `MACOS_HOST_PROFILE_BASE64`, `MACOS_EXT_PROFILE_BASE64` | `base64 -i` of the "Mount Profile" and "OakFSExtension" provisioning profiles |
  | `MACOS_NOTARY_KEY_BASE64`, `MACOS_NOTARY_KEY_ID`, `MACOS_NOTARY_ISSUER_ID` | App Store Connect API key (`base64 -i AuthKey_….p8`), its key id and issuer id |
  | `CARGO_REGISTRY_TOKEN` | crates.io API token (crates.io → Account Settings → API Tokens) with `publish-update` on `oakvcs-core` and `oakvcs-cli` |

- **Variables** (all optional): `RELEASE_DRY_RUN=true` to build and sign
  without publishing; `OAK_URL`, `OAK_REPO` (default `oak/oak`), `OAK_BIN`
  (the `oak` CLI, default the one on `PATH`), `RELEASE_OUTBOX`; `CRATES_DRY_RUN=true`
  to verify and package the crates without uploading them.

None of these may also be an ordinary CI secret: those reach every workflow
on every branch.

### 3. The `nightly` environment

In the same place, create `nightly`:

- **Branches:** `main`.
- **Required reviewers:** keep it on and add whoever may publish a nightly by
  hand.
- **Runners:** bind the same runner uuid. A runner can serve several
  environments; it still runs one job at a time.
- **Secrets:** the same values as `production`: `OAK_ADMIN_API_KEY`,
  `OAK_RELEASE_API_KEY`, `MINISIGN_SECKEY`, `MINISIGN_PASSWORD`, and every
  `MACOS_*` secret.
- **Variables** (optional): `NIGHTLY_FORCE`, `NIGHTLY_KEEP`, `RELEASE_DRY_RUN`,
  `OAK_URL`, `OAK_REPO`, `OAK_BIN`.

### 4. The nightly schedule

On the repo's **Runners** page, choose **Configure** on `nightly`, pick **Daily
timer**, and set the time in UTC. The timer publishes as whoever saves this, so
that person must keep write access to the repo. **Pause auto-publish** in the
same place stops it without changing anything else.

A release Mac set up before the timer may still have the
`space.oak.nightly-deploy` LaunchAgent. Remove it with
`scripts/nightly-schedule-setup.sh --remove`.

## Each release

1. **Bump** `[workspace.package].version` in `Cargo.toml`, add the release
   notes as `docs/release-X.Y.Z.md` (they become the release's notes), and
   merge to `main`.
2. **Optionally rehearse:** set `RELEASE_DRY_RUN=true` on the environment,
   deploy (next step), and check the signed bundle in the runner's outbox
   (`~/oak-runner/releases/<version>/`). Then delete the variable.
3. **Deploy:**

   ```sh
   oak ci deploy -e production --wait 150
   ```

   It previews the head of `main`, the workflow and the environment's policy,
   asks for confirmation, and then waits for the run. This needs a CLI built
   from `main` (it isn't in 0.105.0); **Settings → Environments → Deploy**
   does the same from the browser. Follow the run with `oak ci logs <run-id>`.
4. **Verify distribution:** `curl -fsSL https://oak.space/install | sh` on a
   clean machine, `oak upgrade` from the previous version, and
   `oak --version`, and `oak release view vX.Y.Z --repo oak/oak`.
5. **Publish the crates:**

   ```sh
   oak ci deploy -e production --workflow publish-crates --wait 60
   ```

   It uploads `oakvcs-core`, waits for crates.io's index to show it, then
   uploads `oakvcs-cli`. A rerun skips a core version that's already up.

## When a run fails

Deploy again. The preflight step reads both channels before building
anything and picks the mode, using the same table and helpers as
`release-staging.yml` (`scripts/release-state.sh`):

| Oak release | oak.space | Mode |
|---|---|---|
| no release, or draft | unpromoted | **fresh**: build, sign, publish (a draft is deleted and recreated) |
| draft | promoted | **resume**: no rebuild; verify the draft against oak.space, then publish it |
| published | any | **already published**: verify both channels, and finalize the release if it isn't |

A failure before promotion leaves `oak/oak` with a draft release (visible
only to the repo's writers) and oak.space with an unpromoted staging release.
Nothing is public, and a fresh run replaces both. After promotion only
publishing the release remains, and the resume mode finishes it.

The signing keychain and provisioning profiles are removed in an always-run
step. A job killed outright can leave its keychain behind; the next run's
signing step (`scripts/macos-signing-keychain.sh create`) deletes it first.
