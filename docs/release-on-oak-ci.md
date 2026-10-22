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
the same version at once. crates.io publishing (`publish-crates.yml`) and the
canary channel (`canary.yml`) are unchanged and still run on GitHub.

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
  `.oak/workflows/release.yml` (re-pin after every change to the file).
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

- **Variables** (all optional): `RELEASE_DRY_RUN=true` to build and sign
  without publishing; `GH_MIRROR_BRANCH` if the mirror isn't the GitHub repo's
  default branch; `OAK_URL`, `GH_REPO`, `RELEASE_OUTBOX`.

None of these may also be an ordinary CI secret: those reach every workflow
on every branch.

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
6. **Publish the crates** with GitHub's `publish-crates.yml`, as before.

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
