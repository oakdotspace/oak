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

Publishing follows [`release-staging.yml`](../.github/workflows/release-staging.yml)
exactly: a draft GitHub release, staging on oak.space, byte-level verification
of both channels, promotion, confirmation, then the draft goes live. It
supersedes the GitHub workflow, which stays as a fallback; don't run both for
the same version at once. crates.io publishing
([`.oak/workflows/publish-crates.yml`](../.oak/workflows/publish-crates.yml))
and the nightly channel (below) run on the same Mac.

`oak upgrade`, its daily update check and `install.sh` all read oak.space. The
GitHub release is only for clients up to 0.106.0, which still read GitHub's
`/releases/latest`, and for the README's manual-download links. Keep publishing
it until those clients have upgraded. The same goes for the GitHub canary
build (`canary.yml`): newer clients' `oak upgrade --canary` installs the
current nightly from oak.space instead.

## Nightly builds

[`.oak/workflows/nightly.yml`](../.oak/workflows/nightly.yml) builds the head
of `main` once a day on the release runner, with the same script
(`RELEASE_CHANNEL=nightly`). It ships every target and the notarized app,
stamped `OAK_BUILD_CHANNEL=nightly` and
`OAK_BUILD_ID=YYYYMMDD.<first 8 hex of the Oak commit>`, and minisigned with
the release key under the trusted comment
`oak-nightly version=vX.Y.Z build=<id> commit=<Oak commit>`. It stages them on
oak.space as the release version `nightly`, replacing the previous night's,
and never promotes it, so `/api/releases/latest`, `install.sh` and stable
`oak upgrade` never see it. Nothing touches GitHub.

Users opt in with `oak upgrade --channel nightly`. The client fetches
`/api/releases/nightly/<platform>/minisig`, reads the version and build from
its trusted comment, and installs the binary only after it verifies against
that signature. A nightly binary's first `oak mount` on a Mac installs the
nightly OakMount app.

Oak CI doesn't fire `schedule:` triggers, so a LaunchAgent on the release Mac
deploys the `nightly` environment every day at 02:17 local time (one-time
setup step 4). When every platform already serves a nightly of `main`'s head,
the run stops after the preflight. To publish one now:

```sh
oak ci deploy -e nightly --workflow nightly --yes
```

Set the environment variable `NIGHTLY_FORCE=true` to rebuild an unchanged
`main`, or `RELEASE_DRY_RUN=true` to build and sign without publishing.

## One-time setup

### 1. The runner

On the Mac (full Xcode installed and selected, Homebrew, rustup):

```sh
OAK_RUNNER_TOKEN=<token> scripts/release-runner-setup.sh
```

The token comes from oak.space → the `oak` org's **Settings → Runners**. The
script installs the toolchain (gitea-runner, colima + docker, llvm,
cargo-xwin, minisign, gh, jq, xcodegen, the darwin and windows-msvc rust
targets), registers a host-mode runner labeled `macos` + `arm64`, starts it
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
- **Secrets**, the same values as the GitHub repo's release secrets:

  | Secret | Value |
  |---|---|
  | `GH_RELEASE_TOKEN` | Fine-grained GitHub token for `oakdotspace/oak`: Contents read and write |
  | `OAK_ADMIN_API_KEY` | oak.space admin key (release staging/promote) |
  | `MINISIGN_SECKEY`, `MINISIGN_PASSWORD` | The release minisign key file's contents and passphrase |
  | `MACOS_CERT_P12_BASE64`, `MACOS_CERT_PASSWORD` | `base64 -i cert.p12`, and its export password |
  | `MACOS_KEYCHAIN_PASSWORD` | Any random string (the ephemeral keychain's password) |
  | `MACOS_SIGN_IDENTITY` | Optional: the cert's SHA-1, as a cross-check |
  | `MACOS_HOST_PROFILE_BASE64`, `MACOS_EXT_PROFILE_BASE64` | `base64 -i` of the "Mount Profile" and "OakFSExtension" provisioning profiles |
  | `MACOS_NOTARY_KEY_BASE64`, `MACOS_NOTARY_KEY_ID`, `MACOS_NOTARY_ISSUER_ID` | App Store Connect API key (`base64 -i AuthKey_….p8`), its key id and issuer id |
  | `CARGO_REGISTRY_TOKEN` | crates.io API token (crates.io → Account Settings → API Tokens) with `publish-update` on `oakvcs-core` and `oakvcs-cli` |

- **Variables** (all optional): `RELEASE_DRY_RUN=true` to build and sign
  without publishing; `GH_MIRROR_BRANCH` if the mirror isn't the GitHub repo's
  default branch; `OAK_URL`, `GH_REPO`, `RELEASE_OUTBOX`; `CRATES_DRY_RUN=true`
  to verify and package the crates without uploading them.

None of these may also be an ordinary CI secret: those reach every workflow
on every branch.

### 3. The `nightly` environment

In the same place, create `nightly`:

- **Branches:** `main`.
- **Required reviewers:** keep it on and add the account the release Mac's
  `oak` is logged in as (step 4 deploys as that account).
- **Runners:** bind the same runner uuid. A runner can serve several
  environments; it still runs one job at a time.
- **Secrets:** the same values as `production`, except `GH_RELEASE_TOKEN`,
  which the nightly doesn't use: `OAK_ADMIN_API_KEY`, `MINISIGN_SECKEY`,
  `MINISIGN_PASSWORD`, and every `MACOS_*` secret.
- **Variables** (optional): `NIGHTLY_FORCE`, `RELEASE_DRY_RUN`, `OAK_URL`.

### 4. The nightly schedule

On the release Mac, logged in to oak.space as the reviewer from step 3:

```sh
scripts/nightly-schedule-setup.sh ~/oak-nightly   # an oak clone of oak/oak
```

It previews tonight's deploy and installs the `space.oak.nightly-deploy`
LaunchAgent, which runs `oak ci deploy -e nightly --workflow nightly --yes`
daily (`NIGHTLY_HOUR` / `NIGHTLY_MINUTE` change the time) and logs to
`~/oak-runner/logs/nightly-deploy.log`. `--remove` uninstalls it.

## Each release

1. **Bump** `[workspace.package].version` in `Cargo.toml`, add the release
   notes, and merge to `main`.
2. **Check the GitHub mirror** holds that commit: oak.space syncs it on merge.
   The tag must point at the mirror's snapshot of the commit being released,
   and the preflight step fails if the mirror is behind. Use **Sync now** in
   the repo's GitHub settings on oak.space if it is.
3. **Optionally rehearse:** set `RELEASE_DRY_RUN=true` on the environment,
   deploy (next step), and check the signed bundle in the runner's outbox
   (`~/oak-runner/releases/<version>/`). Then delete the variable.
4. **Deploy:**

   ```sh
   oak ci deploy -e production --wait 150
   ```

   It previews the head of `main`, the workflow and the environment's policy,
   asks for confirmation, and then waits for the run. This needs a CLI built
   from `main` (it isn't in 0.105.0); **Settings → Environments → Deploy**
   does the same from the browser. Follow the run with `oak ci logs <run-id>`.
5. **Verify distribution:** `curl -fsSL https://oak.space/install | sh` on a
   clean machine, `oak upgrade` from the previous version, and
   `oak --version`.
6. **Publish the crates:**

   ```sh
   oak ci deploy -e production --workflow publish-crates --wait 60
   ```

   It uploads `oakvcs-core`, waits for crates.io's index to show it, then
   uploads `oakvcs-cli`. A rerun skips a core version that's already up.

## When a run fails

Deploy again. The preflight step reads both channels before building
anything and picks the mode, using the same table and helpers as
`release-staging.yml` (`scripts/release-state.sh`):

| GitHub | oak.space | Mode |
|---|---|---|
| no release, or draft | unpromoted | **fresh**: build, sign, publish (a draft is replaced) |
| draft | promoted | **resume**: no rebuild; verify the draft against oak.space, then publish it |
| published | any | **already published**: verify both channels and re-mark it latest |

A failure before promotion leaves GitHub with an invisible draft and oak.space
with an unpromoted staging release. Nothing is public, and a fresh run
replaces both. After promotion only the GitHub flip remains, and the resume
mode finishes it.

The signing keychain and provisioning profiles are removed in an always-run
step. A job killed outright can leave its keychain behind; the next run's
signing step (`scripts/macos-signing-keychain.sh create`) deletes it first.
