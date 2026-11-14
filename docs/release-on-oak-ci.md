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
| `*.minisig`, `SHA256SUMS` | `make sign-release`, checked against the CLI's `RELEASE_PUBKEY` |

Publishing goes to oak.space's CLI distribution (`/api/releases`): the
binaries are staged, the bytes oak.space serves are checked against this
run's build, the version is promoted, and the promotion is confirmed.
`oak upgrade`, its daily update check and both installers read it. The release
notes are `docs/release-X.Y.Z.md` in this repo, and the README links each
platform's latest download (`https://oak.space/api/releases/latest/<platform>`).
Nothing else is published: there's no release page and no GitHub release. All
of this lives in the Makefile and `scripts/release-oak-ci.sh`, so it needs no
change to `release.yml` or `nightly.yml` and no re-pin. crates.io publishing
([`.oak/workflows/publish-crates.yml`](../.oak/workflows/publish-crates.yml))
and the nightly channel (below) run on the same Mac.

CLIs at 0.106.0 and older check GitHub's latest release for updates, which no
longer moves, so they need the installer once to reach a current version.

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

Users opt in with `oak upgrade --channel nightly`. The client fetches
`/api/releases/nightly/<platform>/minisig`, reads the version and build from
its trusted comment, and installs the binary only after it verifies against
that signature. A nightly binary's first `oak mount` on a Mac installs the
nightly OakMount app.

`nightly.yml` publishes itself on a daily timer at 10:17 UTC (one-time
setup step 4). Timers are in UTC, so this doesn't move with daylight saving. The
timer picks the newest commit on `main` that passed Verify and skips the day when
that commit has already been published, so a red `main` means an older nightly
or none. When every platform already serves a nightly of the commit, the run
stops after the preflight. To publish one now:

```sh
oak ci publish nightly --yes
```

Set the variable `NIGHTLY_FORCE=true` on nightly.yml's settings and publish by hand to
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
`curl -fsSL https://oak.space/install | sh`), registers a host-mode runner labeled `macos` + `arm64`, starts it
from a LaunchAgent at login, and prints its uuid. Keep the Mac awake on power:
a sleeping runner is offline, and a deploy can't be approved without an
online runner.

The runner runs jobs as your user, directly on the Mac. That's acceptable
because it is bound to the release workflows (next steps): a bound runner takes no
ordinary CI, so it only ever runs this reviewed workflow from `main`, at a
commit someone approved.

### 2. Publish settings for `release.yml`

Each publish workflow has its own settings: secrets, variables, reviewers and
runners belong to the file. In oak.space → `oak/oak` → **Settings → CI** (an
org owner or admin), open `release.yml`'s settings (named `production`, the
name its channel had), or **Set up publishing** for it:

- **Runners:** bind the uuid the setup script printed.
- **Pin:** optional. If you pin the file, pin it again after every change.
- **Secrets**:

  | Secret | Value |
  |---|---|
  | `OAK_ADMIN_API_KEY` | oak.space admin key (release staging/promote) |
  | `MINISIGN_SECKEY`, `MINISIGN_PASSWORD` | The release minisign key file's contents and passphrase |
  | `MACOS_CERT_P12_BASE64`, `MACOS_CERT_PASSWORD` | `base64 -i cert.p12`, and its export password |
  | `MACOS_KEYCHAIN_PASSWORD` | Any random string (the ephemeral keychain's password) |
  | `MACOS_SIGN_IDENTITY` | Optional: the cert's SHA-1, as a cross-check |
  | `MACOS_HOST_PROFILE_BASE64`, `MACOS_EXT_PROFILE_BASE64` | `base64 -i` of the "Mount Profile" and "OakFSExtension" provisioning profiles |
  | `MACOS_NOTARY_KEY_BASE64`, `MACOS_NOTARY_KEY_ID`, `MACOS_NOTARY_ISSUER_ID` | App Store Connect API key (`base64 -i AuthKey_….p8`), its key id and issuer id |

- **Variables** (all optional): `RELEASE_DRY_RUN=true` to build and sign
  without publishing; `OAK_URL` and `RELEASE_OUTBOX`.

`publish-crates.yml` gets its own settings the same way, with only
`CARGO_REGISTRY_TOKEN` (crates.io → Account Settings → API Tokens, with
`publish-update` on `oakvcs-core` and `oakvcs-cli`) and the optional
`CRATES_DRY_RUN=true`. When the channels were split into workflows it
received a copy of every `production` secret; delete the ones it doesn't use,
and delete `CARGO_REGISTRY_TOKEN` from `release.yml`'s settings.

None of these may also be a repository secret used by every Verify workflow:
those reach every workflow on every branch.

### 3. Publish settings for `nightly.yml`

In the same place, open (or set up) `nightly.yml`:

- **Required reviewers:** keep it on and add whoever may publish a nightly by
  hand.
- **Runners:** bind the same runner uuid. A runner can serve several publish
  workflows; it still runs one job at a time.
- **Secrets:** the same values as `release.yml`: `OAK_ADMIN_API_KEY`,
  `MINISIGN_SECKEY`, `MINISIGN_PASSWORD`, and every `MACOS_*` secret. Instead
  of two copies, they can be repository secrets whose **Used by** is
  `release.yml, nightly.yml`.
- **Variables** (optional): `NIGHTLY_FORCE`, `RELEASE_DRY_RUN`, `OAK_URL`.

### 4. The nightly schedule

In **Settings → CI**, open `nightly.yml`'s settings, pick **Daily timer**, and
set the time in UTC. The timer publishes as whoever saves this, so
that person must keep write access to the repo. **Pause auto-publish** in the
same place stops it without changing anything else.

A release Mac set up before the timer may still have the
`space.oak.nightly-deploy` LaunchAgent. Remove it with
`scripts/nightly-schedule-setup.sh --remove`.

## Each release

1. **Bump** `[workspace.package].version` in `Cargo.toml`, add the release
   notes as `docs/release-X.Y.Z.md` (they become the release's notes), and
   merge to `main`.
2. **Optionally rehearse:** set `RELEASE_DRY_RUN=true` on `release.yml`'s settings,
   deploy (next step), and check the signed bundle in the runner's outbox
   (`~/oak-runner/releases/<version>/`). Then delete the variable.
3. **Deploy:**

   ```sh
   oak ci publish production --wait 150
   ```

   (`oak ci publish release` names the same workflow.) It previews the head
   of `main`, the workflow and its settings, asks for confirmation, and then
   waits for the run. **Publish** on the repo's Run tab does the same from
   the browser. Follow the run with `oak ci logs <run-id>`.
4. **Verify distribution:** `curl -fsSL https://oak.space/install | sh` on a
   clean machine, `oak upgrade` from the previous version, and
   `oak --version`, and each README download link
   (`https://oak.space/api/releases/latest/<platform>`) serving the new version.
5. **Publish the crates:**

   ```sh
   oak ci publish publish-crates --wait 60
   ```

   It uploads `oakvcs-core`, waits for crates.io's index to show it, then
   uploads `oakvcs-cli`. A rerun skips a core version that's already up.

## When a run fails

Deploy again. The preflight step reads oak.space's promotion state before
building anything and picks the mode (`release_decide_mode` in
`scripts/release-state.sh`):

| oak.space | Mode |
|---|---|
| unpromoted | **fresh**: build, sign, stage, verify, promote |
| promoted | **already published**: confirm the promotion; nothing is rebuilt |

A failure before promotion leaves an unpromoted staging release on oak.space.
Nothing is public, and a fresh run replaces it.

The signing keychain and provisioning profiles are removed in an always-run
step. A job killed outright can leave its keychain behind; the next run's
signing step (`scripts/macos-signing-keychain.sh create`) deletes it first.
