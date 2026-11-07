#!/usr/bin/env bash
# Stable `oak` releases and nightly builds on Oak CI: the steps of
# .oak/workflows/release.yml and .oak/workflows/nightly.yml.
#
# The whole release is ONE job on ONE machine: the host-mode macOS (Apple
# Silicon) runner bound to the `production` environment builds every
# artifact, signs it, and publishes. An environment workflow has exactly one
# job and Oak CI has no artifact passing, so nothing is split across machines:
#   darwin   built natively, Developer ID signed (make sign-release-macos)
#   linux    built in a Linux container on the runner's Docker (colima):
#            fuser's libfuse-free build needs a Linux host
#            (scripts/release-linux.Dockerfile)
#   windows  cross-compiled with cargo-xwin (make build-release-windows-xwin)
#   app      the OakMount app: xcodebuild, signed, notarized, stapled
#
# Publishing has the mode table and fail-closed order of
# .github/workflows/release-staging.yml, with the repo's own Oak release
# (`oak release`, the tag vX.Y.Z on oak/oak) in place of a GitHub release:
# draft release -> stage on oak.space -> byte-verify both channels -> promote
# -> confirm -> publish and finalize the release. The tag points at the Oak
# commit being released. Release writes use OAK_API_KEY (an API key with write
# access to the repo), since the job's own task token can't write tags.
#
# Key hygiene on a persistent host (from release-macos.yml, which this
# replaces): every cargo build runs first, with no secret anywhere on the
# machine. The P12 is imported only afterwards, for the codesign pass and the
# Xcode build (neither runs cargo), into a keychain that is destroyed in an
# always-run step straight after. Each secret is mapped into the one step that
# uses it. CARGO_HOME is per job and every build is --locked.
#
# Usage: scripts/release-oak-ci.sh <phase>
#   prepare          toolchain + targets; prints KEY=VALUE lines for $GITHUB_ENV
#   preflight        tools, secrets, version, server capability, release mode
#                    (state in target/release-ci/)
#   build-macos      darwin-arm64 + darwin-x86_64, ad-hoc signed, plus the
#                    engine header and fixtures tarball
#   build-linux      linux-x86_64 + linux-arm64 (container)
#   build-windows    windows-x86_64 (cargo-xwin)
#                    (each build phase builds the CLI and the engine library)
#   sign-macos       signing keychain from the P12; Developer ID codesign
#   build-app        provisioning profiles; signed OakMount app (xcodebuild)
#   notarize         notarize + staple the app; OakMount.zip
#   remove-keychain  delete the keychain and our profiles (always runs)
#   verify           architectures, version, signatures of every artifact
#   sign             minisign everything, check key agreement, SHA256SUMS
#   deliver          copy the signed bundle to the runner's outbox
#   publish          draft -> stage -> verify -> promote -> confirm -> flip
#
# Build and signing phases do nothing outside fresh mode: a resumed or
# already-published release is never rebuilt (minisign signatures are
# timestamped, so a rebuild never byte-matches the promoted sidecars).
#
# Nightly (RELEASE_CHANNEL=nightly, from .oak/workflows/nightly.yml): the same
# build, signing and verification phases, with three differences.
#   * Every binary is stamped OAK_BUILD_CHANNEL=nightly and
#     OAK_BUILD_ID=YYYYMMDD.<first 8 hex of the Oak commit>, so `oak --version`
#     reads `oak 0.106.0 (nightly 20261027.b8d19ad7)`.
#   * minisign signs with the trusted comment
#     `oak-nightly version=vX.Y.Z build=<id> commit=<Oak commit>`, which is
#     how `oak upgrade --channel nightly` learns what's published without
#     downloading a binary (cli/src/commands/update_channel.rs).
#   * Publishing stages everything on oak.space as the fixed version
#     `nightly`, replacing last night's, and never promotes it: no outbox, and
#     /api/releases/latest never moves. The preflight skips the build when
#     every platform already serves this commit (NIGHTLY_FORCE=true rebuilds
#     anyway).
#   * Each nightly is also an Oak prerelease, nightly-<id> on oak/oak, so the
#     Releases page and GitHub (through the mirror's release sync) carry it.
#     Only the newest NIGHTLY_KEEP stay; older ones are deleted. oak.space's
#     `nightly` version stays what `oak upgrade` reads, so a failure here can't
#     reach clients: it fails the run after the channel has updated.
#
# Publishing happens only for a deploy of `main` with RELEASE_DRY_RUN unset.
# Anything else (a dry run, a deploy from another branch the environment
# allows, a local run) builds and signs, then stops. A dry run may also lack
# the signing secrets: it then builds ad-hoc-signed binaries and skips the app.
#
# Environment: OAK_CI_* (injected by Oak CI), RELEASE_CHANNEL (stable, the
# default, or nightly), NIGHTLY_FORCE, NIGHTLY_KEEP, RELEASE_DRY_RUN,
# RELEASE_VERSION (optional vX.Y.Z check), OAK_URL, OAK_REPO, OAK_BIN,
# RELEASE_OUTBOX, SIGNING_KEYCHAIN (from prepare), and per-step secrets:
# OAK_API_KEY, OAK_ADMIN_API_KEY, MINISIGN_SECKEY, MINISIGN_PASSWORD,
# MACOS_CERT_P12_BASE64, MACOS_CERT_PASSWORD, MACOS_KEYCHAIN_PASSWORD,
# MACOS_SIGN_IDENTITY, MACOS_HOST_PROFILE_BASE64, MACOS_EXT_PROFILE_BASE64,
# MACOS_NOTARY_KEY_BASE64, MACOS_NOTARY_KEY_ID, MACOS_NOTARY_ISSUER_ID.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

STATE_DIR="$PWD/target/release-ci"
OAK_URL="${OAK_URL:-https://oak.space}"
# The repo whose Oak release (tag) each stable version publishes, and the
# `oak` CLI that writes it: the runner's own install, not this run's build.
OAK_REPO="${OAK_REPO:-oak/oak}"
OAK_BIN="${OAK_BIN:-$(command -v oak || echo "$HOME/.local/bin/oak")}"
APP="macos/OakFS/build/Build/Products/Release/Oak Mount.app"
# Profiles are installed under Xcode's own name for them, <UUID>.provisionprofile.
# A running Xcode signed into the team syncs these directories and deleted
# copies under any other name mid-build (run 1839). Cleanup removes only the
# files this run created (recorded in $STATE_DIR/profiles_installed), so a
# copy Xcode or the machine's owner already had stays. Xcode matches profiles
# by embedded Name.
PROFILE_DIRS=(
  "$HOME/Library/MobileDevice/Provisioning Profiles"
  "$HOME/Library/Developer/Xcode/UserData/Provisioning Profiles"
)
# Names earlier versions of this script installed under; cleanup still
# removes them.
LEGACY_PROFILE_FILES=(oak-release-mount.provisionprofile oak-release-oakfsext.provisionprofile)

# The editor extensions' library, header and conformance fixtures
# (docs/engine-contract.md). They ship on the Oak release only: nothing stages
# them on oak.space, and no client installs them, so they're outside SHA256SUMS
# and minisign. An extension's oak-engine.lock pins each by content hash, which
# `oak release download` verifies.
ENGINE_ASSETS=(
  oak-engine-darwin-arm64.dylib oak-engine-darwin-x86_64.dylib
  oak-engine-linux-x86_64.so oak-engine-linux-arm64.so
  oak-engine-windows-x86_64.dll
  oak_engine.h oak-engine-fixtures.tar.gz
)

# Every asset of a stable release, and the client-installed subset that
# SHA256SUMS covers (the CLI and app sets release-staging.yml uses).
EXPECTED=(
  oak-darwin-arm64 oak-darwin-x86_64 oak-linux-x86_64 oak-linux-arm64
  oak-windows-x86_64.exe
  oak-darwin-arm64.minisig oak-darwin-x86_64.minisig
  oak-linux-x86_64.minisig oak-linux-arm64.minisig
  oak-windows-x86_64.exe.minisig
  OakMount.zip OakMount.zip.minisig
  SHA256SUMS
  "${ENGINE_ASSETS[@]}"
)
SUMMED=(
  oak-darwin-arm64 oak-darwin-x86_64 oak-linux-x86_64 oak-linux-arm64
  oak-windows-x86_64.exe OakMount.zip
)
PLATFORMS=(darwin-arm64 darwin-x86_64 linux-x86_64 linux-arm64 windows-x86_64 darwin-mounter)

die() { echo "error: $*" >&2; exit 1; }
warn() { echo "warning: $*" >&2; }

CHANNEL="${RELEASE_CHANNEL:-stable}"
case "$CHANNEL" in
  stable|nightly) ;;
  *) die "RELEASE_CHANNEL must be stable or nightly, not '$CHANNEL'." ;;
esac
# The oak.space release version the nightly occupies. Never promoted.
NIGHTLY_VERSION=nightly
# Nightly prereleases kept on the release repo, counting the newest.
NIGHTLY_KEEP="${NIGHTLY_KEEP:-7}"
case "$NIGHTLY_KEEP" in
  ''|*[!0-9]*|0) die "NIGHTLY_KEEP must be a positive integer, not '$NIGHTLY_KEEP'." ;;
esac

is_nightly() { [ "$CHANNEL" = nightly ]; }

is_dry_run() {
  [ "${RELEASE_DRY_RUN:-false}" = "true" ] || [ "${OAK_CI_BRANCH:-}" != "main" ]
}

state_put() { mkdir -p "$STATE_DIR"; printf '%s\n' "$2" > "$STATE_DIR/$1"; }
state_get() {
  [ -f "$STATE_DIR/$1" ] || die "no $1 recorded in $STATE_DIR (did the preflight step run?)"
  cat "$STATE_DIR/$1"
}
state_is() { [ "$(cat "$STATE_DIR/$1" 2>/dev/null)" = "$2" ]; }

# Build and signing phases run only in fresh mode.
fresh_only() {
  local mode
  mode="$(state_get mode)"
  if [ "$mode" != "fresh" ]; then
    echo "Mode $mode: nothing to build (this run's artifacts are already published)."
    exit 0
  fi
}

# Export the nightly stamp for a build phase (cargo bakes it in through
# option_env!); a stable build carries none.
stamp_build() {
  if is_nightly; then
    OAK_BUILD_CHANNEL=nightly
    OAK_BUILD_ID="$(state_get build)"
    export OAK_BUILD_CHANNEL OAK_BUILD_ID
  else
    unset OAK_BUILD_CHANNEL OAK_BUILD_ID
  fi
}

# What `oak --version` must print for this run's binaries.
expected_version_line() {
  local version
  version="$(state_get version)"
  if is_nightly; then
    echo "oak ${version#v} (nightly $(state_get build))"
  else
    echo "oak ${version#v}"
  fi
}

# The trusted comment every nightly artifact is signed with.
nightly_comment() {
  echo "oak-nightly version=$(state_get version) build=$(state_get build) commit=$(state_get commit)"
}

# The Oak commit oak.space's nightly for platform $1 was built from, read off
# its signature's trusted comment; empty when no nightly is served. Any other
# failure fails, so an unreadable server never reads as "nothing served".
served_nightly_commit() {
  local platform="$1" out="$STATE_DIR/served-$1.minisig" code
  code=$(curl -sS -o "$out" -w '%{http_code}' "$OAK_URL/api/releases/$NIGHTLY_VERSION/$platform/minisig") || return 1
  case "$code" in
    200) sed -nE 's/^trusted comment: oak-nightly .*commit=([0-9a-f]+).*/\1/p' "$out" ;;
    404) ;;
    *)
      echo "GET $OAK_URL/api/releases/$NIGHTLY_VERSION/$platform/minisig returned HTTP $code. Body:" >&2
      cat "$out" >&2 || true
      return 1
      ;;
  esac
}

platform_file() {
  case "$1" in
    darwin-mounter) echo "OakMount.zip" ;;
    windows-x86_64) echo "oak-windows-x86_64.exe" ;;
    *) echo "oak-$1" ;;
  esac
}

cargo_version() {
  echo "v$(awk '/^\[workspace.package\]/{f=1} f&&/^version *=/{gsub(/[" ]/,"");split($0,a,"=");print a[2];exit}' Cargo.toml)"
}

# GET /api/releases with the admin key into $1; fails unless HTTP 200.
fetch_release_listing() {
  local out="$1" code
  code=$(curl -sS -o "$out" -w '%{http_code}' -H "Authorization: Bearer $OAK_ADMIN_API_KEY" "$OAK_URL/api/releases")
  if [ "$code" != "200" ]; then
    echo "GET $OAK_URL/api/releases returned HTTP $code (expected 200). Body:" >&2
    cat "$out" >&2 || true
    return 1
  fi
}

# `oak release ...` against the release repo on $OAK_URL, authenticated with
# OAK_API_KEY from the step's environment.
oak_release() {
  "$OAK_BIN" release "$@" --repo "$OAK_REPO" --remote "$OAK_URL"
}

# Prints absent | draft | published for the Oak release $1. A CLI or API
# error is never read as "absent" (that would route a live release into
# fresh mode).
oak_release_state() {
  local version="$1" out="$STATE_DIR/tags-$1.json"
  oak_release list --limit 1000 --json > "$out" || {
    echo "oak release list failed (details above)." >&2
    return 1
  }
  release_tag_state "$version" "$out"
}

# ---------------------------------------------------------------------------

phase_prepare() {
  local scratch="${RUNNER_TEMP:-}"
  [ -n "$scratch" ] || scratch="$(mktemp -d)"
  # rust-toolchain.toml names the channel; install it (a no-op when present)
  # plus every release target. Logs go to stderr: stdout is $GITHUB_ENV.
  {
    rustup toolchain install
    rustup target add aarch64-apple-darwin x86_64-apple-darwin x86_64-pc-windows-msvc
    rustc --version
  } >&2
  echo "CARGO_HOME=$scratch/cargo-home"
  echo "SIGNING_KEYCHAIN=$scratch/oak-signing-${OAK_CI_RUN_ID:-local}.keychain-db"
}

phase_preflight() {
  rm -rf "$STATE_DIR"
  mkdir -p "$STATE_DIR"

  # Everything the later phases call, checked before a long build.
  local missing="" tool tools
  tools="cargo rustup make jq curl shasum minisign docker cargo-xwin xcodegen xcodebuild codesign security ditto lipo xcrun file"
  for tool in $tools; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
  done
  is_nightly || [ -x "$OAK_BIN" ] || missing="$missing oak"
  if ! command -v brew >/dev/null 2>&1 || [ ! -x "$(brew --prefix llvm 2>/dev/null)/bin/llvm-lib" ]; then
    missing="$missing llvm(brew)"
  fi
  [ -z "$missing" ] || die "the runner is missing:$missing. Run scripts/release-runner-setup.sh on it."
  docker info >/dev/null 2>&1 || die "docker can't reach a daemon. Is colima running? (brew services start colima)"

  local version
  version="$(cargo_version)"
  if [ -n "${RELEASE_VERSION:-}" ] && [ "$RELEASE_VERSION" != "$version" ]; then
    die "RELEASE_VERSION '$RELEASE_VERSION' != Cargo.toml version '$version'. Bump Cargo.toml or fix the variable."
  fi
  state_put version "$version"
  if is_nightly; then
    preflight_nightly "$version"
    return 0
  fi
  echo "Release $version from Oak commit ${OAK_CI_COMMIT:-(local)} on branch ${OAK_CI_BRANCH:-(local)}, run ${OAK_CI_RUN_ID:-(local)}"

  if is_dry_run; then
    state_put mode fresh
    echo "Dry run (RELEASE_DRY_RUN=${RELEASE_DRY_RUN:-unset}, branch ${OAK_CI_BRANCH:-unset}): build and sign only; nothing is published."
    return 0
  fi

  # Every secret a later step hard-requires. The workflow passes presence
  # flags (HAS_*), not values, for the ones this step doesn't use.
  # MINISIGN_PASSWORD is optional: an unencrypted key legitimately has none.
  local name
  missing=""
  [ -n "${OAK_API_KEY:-}" ] || missing="$missing OAK_RELEASE_API_KEY"
  [ -n "${OAK_ADMIN_API_KEY:-}" ] || missing="$missing OAK_ADMIN_API_KEY"
  for name in MINISIGN_SECKEY MACOS_CERT_P12_BASE64 MACOS_CERT_PASSWORD MACOS_KEYCHAIN_PASSWORD \
              MACOS_HOST_PROFILE_BASE64 MACOS_EXT_PROFILE_BASE64 \
              MACOS_NOTARY_KEY_BASE64 MACOS_NOTARY_KEY_ID MACOS_NOTARY_ISSUER_ID; do
    local flag="HAS_$name"
    [ "${!flag:-false}" = "true" ] || missing="$missing $name"
  done
  [ -z "$missing" ] || die "a stable release needs these environment secrets:$missing. Add them in Settings -> Environments -> production, or set RELEASE_DRY_RUN=true."

  # Server capability probes: release-staging.yml's advisory fast-fail. The
  # staging and promote endpoints themselves are the real guard.
  # shellcheck source=scripts/release-state.sh
  source scripts/release-state.sh
  local cap code
  cap=$(release_listing_probe "$OAK_URL" "$OAK_ADMIN_API_KEY" "$STATE_DIR/probe-list.json") \
    || die "release listing probe failed (details above)."
  [ "$cap" != "pre-b2" ] || die "$OAK_URL is not staging-capable (pre-B2 listing returned 401)."
  code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$OAK_URL/api/releases/v0.0.0-capability-probe/promote" -H 'Content-Type: application/json' -d '{}')
  case "$code" in
    400|401) ;;
    *) die "promote endpoint probe got HTTP $code (expected 400/401): the server can't promote releases." ;;
  esac
  code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$OAK_URL/api/releases/stage" -H 'Content-Type: application/json' -d '{}')
  case "$code" in
    400|401) ;;
    *) die "stage endpoint probe got HTTP $code (expected 400/401): the server can't stage releases." ;;
  esac
  echo "$OAK_URL can stage and promote releases."

  # The release tag points at exactly the Oak commit this run builds. The
  # server takes only a full hash, and only a commit on main.
  [[ "${OAK_CI_COMMIT:-}" =~ ^[0-9a-f]{64}$ ]] \
    || die "OAK_CI_COMMIT is '${OAK_CI_COMMIT:-unset}', not a full Oak commit hash; is this running under Oak CI?"
  state_put tag_commit "$OAK_CI_COMMIT"
  echo "Release tag: $version on $OAK_REPO at ${OAK_CI_COMMIT:0:12} ($("$OAK_BIN" --version))"

  # Release mode from the observed state of both channels, before any build.
  local tag_state promo_state mode
  tag_state=$(oak_release_state "$version") \
    || die "can't read $OAK_REPO's releases on $OAK_URL (details above). Check OAK_RELEASE_API_KEY has write access to $OAK_REPO and releases are enabled for it."
  fetch_release_listing "$STATE_DIR/mode-releases.json" || die "can't read $OAK_URL's release listing."
  promo_state=$(release_promotion_state "$version" "$STATE_DIR/mode-releases.json") \
    || die "$version's promotion state on $OAK_URL is invalid or ambiguous (details above). Operator action required."
  echo "Observed state: release=$tag_state oak.space=$promo_state"
  mode=$(release_decide_mode "$tag_state" "$promo_state") \
    || die "inconsistent release state for $version (details above). Operator action required; don't deploy again until it's resolved."
  state_put mode "$mode"
  echo "Release mode: $mode"
}

# The nightly half of the preflight: the build id, the secrets a published
# nightly needs, and whether oak.space already serves a nightly of this commit.
preflight_nightly() {
  local version="$1" commit build
  commit="${OAK_CI_COMMIT:-local}"
  build="$(date -u +%Y%m%d).${commit:0:8}"
  state_put commit "$commit"
  state_put build "$build"
  echo "Nightly $build ($version) from Oak commit $commit on branch ${OAK_CI_BRANCH:-(local)}, run ${OAK_CI_RUN_ID:-(local)}"

  if is_dry_run; then
    state_put mode fresh
    echo "Dry run (RELEASE_DRY_RUN=${RELEASE_DRY_RUN:-unset}, branch ${OAK_CI_BRANCH:-unset}): build and sign only; nothing is published."
    return 0
  fi

  local missing="" name
  [ -n "${OAK_ADMIN_API_KEY:-}" ] || missing="$missing OAK_ADMIN_API_KEY"
  for name in MINISIGN_SECKEY MACOS_CERT_P12_BASE64 MACOS_CERT_PASSWORD MACOS_KEYCHAIN_PASSWORD \
              MACOS_HOST_PROFILE_BASE64 MACOS_EXT_PROFILE_BASE64 \
              MACOS_NOTARY_KEY_BASE64 MACOS_NOTARY_KEY_ID MACOS_NOTARY_ISSUER_ID; do
    local flag="HAS_$name"
    [ "${!flag:-false}" = "true" ] || missing="$missing $name"
  done
  [ -z "$missing" ] || die "a nightly needs these environment secrets:$missing. Add them in Settings -> Environments -> nightly, or set RELEASE_DRY_RUN=true."
  # Not checked as a hard requirement here: the update channel shouldn't stop
  # over a prerelease, so a build still ships, and publish then fails the run.
  # shellcheck source=scripts/release-state.sh
  source scripts/release-state.sh
  local blocker
  blocker=$(nightly_prerelease_blocker "${HAS_OAK_RELEASE_API_KEY:-false}" "$OAK_BIN") \
    || warn "missing $blocker: a nightly built by this run updates the channel, then fails because it can't create the Oak prerelease."

  local code
  code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$OAK_URL/api/releases/stage" -H 'Content-Type: application/json' -d '{}')
  case "$code" in
    400|401) ;;
    *) die "stage endpoint probe got HTTP $code (expected 400/401): the server can't stage releases." ;;
  esac

  local platform served mode=current
  for platform in "${PLATFORMS[@]}"; do
    served=$(served_nightly_commit "$platform") || die "can't read the nightly oak.space serves for $platform (details above)."
    echo "Served nightly for $platform: ${served:-none}"
    [ "$served" = "$commit" ] || mode=fresh
  done
  if [ "$mode" = current ] && [ "${NIGHTLY_FORCE:-false}" = true ]; then
    echo "NIGHTLY_FORCE=true: rebuilding although every platform already serves this commit."
    mode=fresh
  fi
  state_put mode "$mode"
  if [ "$mode" = fresh ]; then
    echo "Building nightly $build."
  else
    echo "Every platform already serves a nightly of ${commit:0:12}; nothing to do (NIGHTLY_FORCE=true rebuilds)."
  fi
}

phase_build_macos() {
  fresh_only
  stamp_build
  make build-release-macos SIGN_IDENTITY= CARGO="cargo --locked"
}

phase_build_linux() {
  fresh_only
  stamp_build
  local image="oak-release-linux-toolchain" cid
  # The toolchain image (cached between releases); its context is just the
  # Dockerfile and rust-toolchain.toml, without macOS extended attributes
  # (the daemon can't set com.apple.provenance on the Linux side).
  COPYFILE_DISABLE=1 tar -c --no-xattrs --no-mac-metadata rust-toolchain.toml scripts/release-linux.Dockerfile \
    | docker build -f scripts/release-linux.Dockerfile -t "$image" -
  # The build runs in a throwaway container, removed with everything it built
  # once the two binaries are out. Zig's linker opens many files at once.
  # -e NAME passes the stamp through when it's set, and nothing when it isn't.
  cid="$(docker create --ulimit nofile=65536:65536 -e OAK_BUILD_CHANNEL -e OAK_BUILD_ID "$image" sh -c '
    make build-release-linux CARGO="cargo --locked" &&
    for b in target/releases/oak-linux-x86_64 target/releases/oak-linux-arm64; do
      if readelf -d "$b" | grep -i "NEEDED.*fuse"; then
        echo "error: $b links libfuse; it must exec fusermount3 at runtime instead" >&2
        exit 1
      fi
    done')"
  # shellcheck disable=SC2064 # expand $cid now
  trap "docker rm -f $cid >/dev/null 2>&1 || true" EXIT
  # The source tree: no target/, no .oak/, no app build output, and no macOS
  # extended attributes (docker cp can't set com.apple.* on Linux files).
  COPYFILE_DISABLE=1 tar -c --no-xattrs --no-mac-metadata \
      --exclude=./target --exclude=./.oak --exclude=./.git --exclude=./macos/OakFS/build . \
    | docker cp - "$cid:/src"
  docker start --attach "$cid" || true
  [ "$(docker inspect --format '{{.State.ExitCode}}' "$cid")" = 0 ] || die "the Linux build failed (log above)."
  mkdir -p target/releases
  docker cp "$cid:/src/target/releases/oak-linux-x86_64" target/releases/oak-linux-x86_64
  docker cp "$cid:/src/target/releases/oak-linux-arm64" target/releases/oak-linux-arm64
  docker cp "$cid:/src/target/releases/oak-engine-linux-x86_64.so" target/releases/oak-engine-linux-x86_64.so
  docker cp "$cid:/src/target/releases/oak-engine-linux-arm64.so" target/releases/oak-engine-linux-arm64.so
}

phase_build_windows() {
  fresh_only
  stamp_build
  make build-release-windows-xwin
}

phase_sign_macos() {
  fresh_only
  if [ -z "${MACOS_CERT_P12_BASE64:-}" ]; then
    is_dry_run || die "MACOS_CERT_P12_BASE64 is not set."
    warn "dry run without the signing certificate: the darwin binaries stay ad-hoc signed and the app is skipped."
    return 0
  fi
  : "${SIGNING_KEYCHAIN:?SIGNING_KEYCHAIN is not set (the prepare step writes it)}"
  bash scripts/macos-signing-keychain.sh create "$SIGNING_KEYCHAIN"
  local expect="${MACOS_SIGN_IDENTITY:-}" identity
  unset MACOS_CERT_P12_BASE64 MACOS_CERT_PASSWORD MACOS_KEYCHAIN_PASSWORD MACOS_SIGN_IDENTITY
  # Derived from the imported P12 alone (scoped to this keychain), so the
  # identity can't drift from the certificate; see _build-cli.yml.
  identity="$(bash scripts/derive-sign-identity.sh --keychain "$SIGNING_KEYCHAIN" ${expect:+--expect "$expect"})"
  make sign-release-macos SIGN_IDENTITY="$identity"
  state_put sign_identity "$identity"
}

phase_build_app() {
  fresh_only
  if [ ! -f "$STATE_DIR/sign_identity" ]; then
    is_dry_run || die "the darwin binaries weren't signed, so there's no keychain to build the app with."
    warn "dry run without signing: skipping the OakMount app."
    return 0
  fi
  [ -n "${MACOS_HOST_PROFILE_BASE64:-}" ] && [ -n "${MACOS_EXT_PROFILE_BASE64:-}" ] \
    || die "MACOS_HOST_PROFILE_BASE64 / MACOS_EXT_PROFILE_BASE64 are required: the OakFS extension can't be signed without both profiles."
  # A wrong profile decodes fine and fails late in xcodebuild: check the
  # Names project.yml asks for, for the right team.
  local want_team found="" b64 src plist pname team uuid dir dest spec
  want_team=$(grep -m1 -E '^[[:space:]]*DEVELOPMENT_TEAM[[:space:]]*:' macos/OakFS/project.yml | sed -E 's/.*"([^"]+)".*/\1/')
  mkdir -p "$STATE_DIR"
  : > "$STATE_DIR/profiles_installed"
  for b64 in "$MACOS_HOST_PROFILE_BASE64" "$MACOS_EXT_PROFILE_BASE64"; do
    src="$STATE_DIR/profile.provisionprofile"
    printf '%s' "$b64" | base64 --decode > "$src"
    plist=$(security cms -D -i "$src")
    pname=$(printf '%s' "$plist" | plutil -extract Name raw -)
    team=$(printf '%s' "$plist" | plutil -extract TeamIdentifier.0 raw -)
    uuid=$(printf '%s' "$plist" | plutil -extract UUID raw -)
    for dir in "${PROFILE_DIRS[@]}"; do
      mkdir -p "$dir"
      dest="$dir/$uuid.provisionprofile"
      if [ -f "$dest" ] && cmp -s "$src" "$dest"; then
        continue
      fi
      cp "$src" "$dest"
      printf '%s\n' "$dest" >> "$STATE_DIR/profiles_installed"
    done
    rm -f "$src"
    echo "Installed $uuid.provisionprofile: Name='$pname' Team=$team Expires=$(printf '%s' "$plist" | plutil -extract ExpirationDate raw -)"
    found="$found|$team/$pname"
  done
  for spec in "$want_team/Mount Profile" "$want_team/OakFSExtension"; do
    case "$found" in
      *"$spec"*) ;;
      *) die "no installed profile matches project.yml's '$spec' (found:$found). Re-export the right profile and update its secret." ;;
    esac
  done

  make macos-app
  codesign --verify --strict --deep --verbose=2 "$APP"
  codesign --display --entitlements - "$APP/Contents/Extensions/OakFSExtension.appex"
  state_put app_built true
}

phase_notarize() {
  fresh_only
  if ! state_is app_built true; then
    is_dry_run || die "the OakMount app wasn't built."
    echo "No app in this dry run; nothing to notarize."
    return 0
  fi
  if [ -z "${MACOS_NOTARY_KEY_BASE64:-}" ] || [ -z "${MACOS_NOTARY_KEY_ID:-}" ] || [ -z "${MACOS_NOTARY_ISSUER_ID:-}" ]; then
    is_dry_run || die "the MACOS_NOTARY_* secrets are required: a stable release can't ship an un-notarized app."
    warn "dry run without notary credentials: the app is signed but NOT notarized."
  else
    local key="$STATE_DIR/notary-key.p8" submit="$STATE_DIR/OakMount-notarize.zip" json rc status id
    ( umask 077; printf '%s' "$MACOS_NOTARY_KEY_BASE64" | base64 --decode > "$key" )
    trap 'rm -f "$STATE_DIR/notary-key.p8" "$STATE_DIR/OakMount-notarize.zip"' EXIT
    ditto -c -k --keepParent "$APP" "$submit"
    local creds=(--key "$key" --key-id "$MACOS_NOTARY_KEY_ID" --issuer "$MACOS_NOTARY_ISSUER_ID")
    echo "Submitting to Apple's notary service (a few minutes)..."
    json=$(xcrun notarytool submit "$submit" "${creds[@]}" --wait --output-format json) && rc=0 || rc=$?
    echo "$json"
    id=$(printf '%s' "$json" | plutil -extract id raw - 2>/dev/null || true)
    status=$(printf '%s' "$json" | plutil -extract status raw - 2>/dev/null || true)
    if [ "$status" != "Accepted" ]; then
      [ -n "$id" ] && xcrun notarytool log "$id" "${creds[@]}" || true
      die "notarization failed (status '$status', exit $rc)."
    fi
    xcrun stapler staple "$APP"
    xcrun stapler validate "$APP"
    state_put app_notarized true
    echo "Notarized and stapled."
  fi
  mkdir -p target/releases
  rm -f target/releases/OakMount.zip
  ditto -c -k --keepParent "$APP" target/releases/OakMount.zip
}

phase_remove_keychain() {
  if [ -n "${SIGNING_KEYCHAIN:-}" ]; then
    bash scripts/macos-signing-keychain.sh destroy "$SIGNING_KEYCHAIN"
  fi
  local dir name path
  if [ -f "$STATE_DIR/profiles_installed" ]; then
    while IFS= read -r path; do
      [ -n "$path" ] && rm -f "$path"
    done < "$STATE_DIR/profiles_installed"
    rm -f "$STATE_DIR/profiles_installed"
  fi
  for dir in "${PROFILE_DIRS[@]}"; do
    for name in "${LEGACY_PROFILE_FILES[@]}"; do
      rm -f "$dir/$name"
    done
  done
  echo "Signing keychain and provisioning profiles removed."
}

# Checks the bytes about to be signed and shipped, independently of the
# Makefile and the build phases.
phase_verify() {
  fresh_only
  local want b info
  want="$(expected_version_line)"
  cd target/releases
  [ "$(lipo -archs oak-darwin-arm64)" = arm64 ] || die "oak-darwin-arm64 is not an arm64 binary."
  [ "$(lipo -archs oak-darwin-x86_64)" = x86_64 ] || die "oak-darwin-x86_64 is not an x86_64 binary."
  # The tag `oak upgrade` compares is the binary's CARGO_PKG_VERSION; a
  # nightly's build id is what the nightly channel compares.
  [ "$(./oak-darwin-arm64 --version)" = "$want" ] || die "oak-darwin-arm64 reports '$(./oak-darwin-arm64 --version)', expected '$want'."
  if is_nightly; then
    # The binaries this Mac can't run carry the id as a string.
    for b in oak-darwin-x86_64 oak-linux-x86_64 oak-linux-arm64 oak-windows-x86_64.exe; do
      LC_ALL=C grep -qaF -- "$(state_get build)" "$b" || die "$b doesn't carry the nightly build id $(state_get build)."
    done
  fi
  file oak-linux-x86_64 | grep -q 'ELF 64-bit LSB.*x86-64' || die "oak-linux-x86_64 is not an x86-64 ELF."
  file oak-linux-arm64 | grep -q 'ELF 64-bit LSB.*ARM aarch64' || die "oak-linux-arm64 is not an aarch64 ELF."
  file oak-windows-x86_64.exe | grep -q 'PE32+ executable.*x86-64' || die "oak-windows-x86_64.exe is not an x86-64 PE."
  [ "$(lipo -archs oak-engine-darwin-arm64.dylib)" = arm64 ] || die "oak-engine-darwin-arm64.dylib is not arm64."
  [ "$(lipo -archs oak-engine-darwin-x86_64.dylib)" = x86_64 ] || die "oak-engine-darwin-x86_64.dylib is not x86_64."
  file oak-engine-linux-x86_64.so | grep -q 'ELF 64-bit LSB.*x86-64' || die "oak-engine-linux-x86_64.so is not an x86-64 ELF."
  file oak-engine-linux-arm64.so | grep -q 'ELF 64-bit LSB.*ARM aarch64' || die "oak-engine-linux-arm64.so is not an aarch64 ELF."
  file oak-engine-windows-x86_64.dll | grep -q 'PE32+ executable.*x86-64' || die "oak-engine-windows-x86_64.dll is not an x86-64 PE."
  grep -q 'oak_engine_call' oak_engine.h || die "oak_engine.h doesn't declare oak_engine_call."
  tar -tzf oak-engine-fixtures.tar.gz | grep -q '^conformance/cli_parse.json$' || die "oak-engine-fixtures.tar.gz has no cli_parse.json."
  if [ ! -f "$STATE_DIR/sign_identity" ]; then
    echo "Unsigned dry run: architectures and version check out."
    return 0
  fi
  for b in oak-darwin-arm64 oak-darwin-x86_64 oak-engine-darwin-arm64.dylib oak-engine-darwin-x86_64.dylib; do
    codesign --verify --strict --verbose=2 "$b"
    info="$(codesign --display --verbose=4 "$b" 2>&1)"
    grep -q '^Authority=Developer ID Application' <<<"$info" || { echo "$info"; die "$b is not Developer ID signed."; }
    grep -q '^Authority=Apple Root CA$' <<<"$info" || { echo "$info"; die "$b's signature doesn't chain to Apple Root CA."; }
    grep -Eq '^CodeDirectory .*flags=0x[0-9a-f]+\([^)]*runtime' <<<"$info" || { echo "$info"; die "$b lacks the hardened runtime."; }
    grep -q '^Timestamp=' <<<"$info" || { echo "$info"; die "$b has no secure timestamp."; }
  done
  echo "Darwin binaries: Developer ID, Apple Root CA, hardened runtime, secure timestamp."
  if state_is app_built true; then
    [ -f OakMount.zip ] || die "OakMount.zip is missing."
    local unpacked="$STATE_DIR/app-verify"
    rm -rf "$unpacked" && mkdir -p "$unpacked"
    ditto -x -k OakMount.zip "$unpacked"
    codesign --verify --strict --deep "$unpacked/Oak Mount.app"
    if state_is app_notarized true; then
      xcrun stapler validate "$unpacked/Oak Mount.app"
    fi
    echo "OakMount.zip: signed$(state_is app_notarized true && echo ', notarization ticket stapled')."
  fi
}

phase_sign() {
  fresh_only
  if [ -z "${MINISIGN_SECKEY:-}" ]; then
    is_dry_run || die "MINISIGN_SECKEY is not set: oak upgrade and first-run oak mount refuse unsigned artifacts."
    warn "dry run without MINISIGN_SECKEY: artifacts are not minisigned."
  else
    local key="$STATE_DIR/release.key" comment=""
    is_nightly && comment="$(nightly_comment)"
    ( umask 077; printf '%s\n' "$MINISIGN_SECKEY" > "$key" )
    trap 'rm -f "$STATE_DIR/release.key"' EXIT
    make sign-release MINISIGN_SECKEY="$key" MINISIGN_PASSWORD="${MINISIGN_PASSWORD:-}" MINISIGN_TRUSTED_COMMENT="$comment"
    rm -f "$key"
    # Key agreement: every signature must verify against the RELEASE_PUBKEY
    # baked into the CLI, or clients would refuse the release.
    local pubkey f
    pubkey=$(sed -nE 's/.*RELEASE_PUBKEY: &str = "([^"]+)".*/\1/p' cli/src/commands/upgrade.rs)
    [ -n "$pubkey" ] || die "couldn't read RELEASE_PUBKEY from cli/src/commands/upgrade.rs."
    for f in "${SUMMED[@]}"; do
      [ -f "target/releases/$f" ] || continue
      minisign -Vq -P "$pubkey" -m "target/releases/$f" -x "target/releases/$f.minisig" \
        || die "target/releases/$f.minisig doesn't verify against the CLI's RELEASE_PUBKEY: wrong signing key."
      if is_nightly; then
        grep -qxF "trusted comment: $comment" "target/releases/$f.minisig" \
          || die "target/releases/$f.minisig doesn't carry the nightly comment '$comment'."
      fi
    done
    echo "Every signature verifies against the CLI's RELEASE_PUBKEY."
    if is_nightly; then echo "Every signature carries the trusted comment: $comment"; fi
  fi
  local present=() f
  for f in "${SUMMED[@]}"; do
    [ -f "target/releases/$f" ] && present+=("$f")
  done
  (cd target/releases && shasum -a 256 "${present[@]}" > SHA256SUMS)
  cat target/releases/SHA256SUMS
}

# A read-only copy of every signed run of main, with a manifest, in
# ${RELEASE_OUTBOX:-~/oak-runner/releases}/<version>/<oak-commit>-run<id>/
# (one directory per run, created atomically). Oak CI has no artifact
# storage, so this is where a dry run's output, and the audit trail of a real
# release, live.
phase_deliver() {
  fresh_only
  local version
  version="$(state_get version)"
  if [ ! -f "$STATE_DIR/sign_identity" ]; then
    echo "Unsigned dry run: not delivered."
    return 0
  fi
  if [ "${OAK_CI_BRANCH:-}" != main ]; then
    echo "Run from branch '${OAK_CI_BRANCH:-(local)}': only main is delivered."
    return 0
  fi
  local outbox="${RELEASE_OUTBOX:-$HOME/oak-runner/releases}"
  local dest="$outbox/$version/$OAK_CI_COMMIT-run$OAK_CI_RUN_ID" incoming f files
  [ ! -e "$dest" ] || die "$dest already exists; refusing to overwrite a delivered release."
  mkdir -p "$outbox/$version"
  incoming="$(mktemp -d "$outbox/.incoming.XXXXXX")"
  trap '[ -z "${incoming:-}" ] || rm -rf "$incoming"' EXIT
  for f in "${EXPECTED[@]}"; do
    [ -f "target/releases/$f" ] && cp "target/releases/$f" "$incoming/"
  done
  files="$(cd "$incoming" && for f in *; do
    jq -n --arg name "$f" --arg sha256 "$(shasum -a 256 "$f" | cut -d' ' -f1)" \
      --argjson size "$(stat -f %z "$f")" '{name: $name, sha256: $sha256, size: $size}'
  done | jq -s .)"
  jq -n \
    --arg version "$version" \
    --arg repository "${GITHUB_REPOSITORY:-}" \
    --arg oak_commit "$OAK_CI_COMMIT" \
    --arg git_sha "${GITHUB_SHA:-}" \
    --arg branch "$OAK_CI_BRANCH" \
    --arg run_id "$OAK_CI_RUN_ID" \
    --arg signing_identity "$(state_get sign_identity)" \
    --arg dry_run "$(is_dry_run && echo true || echo false)" \
    --arg rustc "$(rustc --version)" \
    --arg built_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --argjson files "$files" \
    '{version: $version, repository: $repository, oak_commit: $oak_commit, git_sha: $git_sha,
      branch: $branch, run_id: $run_id, signing_identity: $signing_identity,
      dry_run: ($dry_run == "true"), rustc: $rustc, built_at: $built_at, files: $files}' \
    > "$incoming/manifest.json"
  chmod 0444 "$incoming"/*
  # Rename, then lock the directory: macOS refuses to move a read-only
  # directory to a new parent (its `..` entry must change).
  mv "$incoming" "$dest"
  incoming=""
  chmod 0555 "$dest"
  echo "Delivered $version to $dest"
}

phase_publish() {
  if is_dry_run; then
    echo "Dry run: nothing published."
    ls -la target/releases 2>/dev/null || true
    return 0
  fi
  if is_nightly; then
    publish_nightly
    return 0
  fi
  # shellcheck source=scripts/release-state.sh
  source scripts/release-state.sh
  local version mode
  version="$(state_get version)"
  mode="$(state_get mode)"
  echo "Publishing $version (mode: $mode)"

  if [ "$mode" = "fresh" ]; then
    publish_draft "$version" "$(state_get tag_commit)"
    publish_stage "$version"
  fi
  publish_verify "$version" "$mode"
  publish_promote "$version" "$mode"
  publish_release "$version"
  echo "Published $version on $OAK_URL ($OAK_URL/$OAK_REPO/releases/tag/$version)."
}

# Publish the draft and freeze its asset list. Promotion already made the
# version what `oak upgrade` installs; this makes the release page public.
# Both flags are one-way, so an already-published, finalized release is left
# as it is, and a live release is never demoted.
publish_release() {
  local version="$1" out="$STATE_DIR/release-final.json" args=()
  oak_release view "$version" --json > "$out" || die "can't read release $version (details above)."
  [ "$(jq -r '.tag.draft' "$out")" = true ] && args+=(--publish)
  [ "$(jq -r '.tag.finalized' "$out")" = true ] || args+=(--finalize)
  if [ "${#args[@]}" -gt 0 ]; then
    oak_release edit "$version" "${args[@]}" --json > "$out" || die "can't publish release $version (details above)."
  fi
  jq -e '.tag.draft == false and .tag.finalized == true' "$out" >/dev/null \
    || die "release $version is still a draft or not finalized after publishing: $(jq -c '.tag | {draft, finalized}' "$out")"
}

# Stage this run's build as oak.space's `nightly` version, which replaces
# last night's (re-staging an unpromoted version is allowed) and is never
# promoted, then check that every platform serves exactly these bytes and
# signatures.
publish_nightly() {
  local build platform file want got url dl="$STATE_DIR/nightly-verify"
  build="$(state_get build)"
  if [ "$(state_get mode)" != fresh ]; then
    echo "Nothing to publish: oak.space already serves this commit's nightly."
    return 0
  fi
  for platform in "${PLATFORMS[@]}"; do
    file="target/releases/$(platform_file "$platform")"
    [ -f "$file" ] && [ -f "$file.minisig" ] || die "$file or its signature is missing; a nightly ships every platform."
  done
  state_is app_notarized true || die "OakMount.zip wasn't notarized in this run."

  make upload-release VERSION="$NIGHTLY_VERSION" OAK_URL="$OAK_URL" OAK_ADMIN_API_KEY="$OAK_ADMIN_API_KEY" REQUIRE_MOUNTER=1
  rm -rf "$dl"
  mkdir -p "$dl"
  for platform in "${PLATFORMS[@]}"; do
    file="target/releases/$(platform_file "$platform")"
    want=$(shasum -a 256 "$file" | awk '{print $1}')
    url="$OAK_URL/api/releases/$NIGHTLY_VERSION/$platform"
    curl --fail-with-body -sS -o "$dl/$platform.bin" "$url" || die "$url is not being served."
    got=$(shasum -a 256 "$dl/$platform.bin" | awk '{print $1}')
    [ "$got" = "$want" ] || die "$url serves sha256 $got, expected $want."
    got=$(curl --fail-with-body -sS "$url/sha256") || die "$url/sha256 is not being served."
    [ "$(printf '%s' "$got" | tr -d '[:space:]')" = "$want" ] || die "$url/sha256 publishes '$got', expected $want."
    curl --fail-with-body -sS -o "$dl/$platform.minisig" "$url/minisig" || die "$url/minisig is not being served."
    cmp -s "$file.minisig" "$dl/$platform.minisig" || die "$url/minisig doesn't byte-match."
    echo "OK $platform"
  done
  echo "Published nightly $build on $OAK_URL (version '$NIGHTLY_VERSION'; never promoted)."
  publish_nightly_release "$build"
}

# Publish this build as the Oak prerelease nightly-<build>: a draft until every
# asset is up, so the mirror's release sync sees it whole. A rerun of one
# commit on one day reuses the name, so a leftover draft is replaced and a
# published one left alone. Then drop nightlies past NIGHTLY_KEEP.
publish_nightly_release() {
  local build="$1" tag="nightly-$1" state assets=() f name blocker
  # shellcheck source=scripts/release-state.sh
  source scripts/release-state.sh
  blocker=$(nightly_prerelease_blocker "${OAK_API_KEY:+true}" "$OAK_BIN") \
    || die "can't create the Oak prerelease $tag: missing $blocker. The channel already serves $build. Fix this (docs/release-on-oak-ci.md), then deploy with NIGHTLY_FORCE=true to publish it."
  for f in "${EXPECTED[@]}"; do
    [ -f "target/releases/$f" ] || die "target/releases/$f is missing; a nightly prerelease ships all ${#EXPECTED[@]} assets."
    assets+=("$f")
  done
  state=$(oak_release_state "$tag") || die "can't read release $tag (details above)."
  case "$state" in
    published)
      echo "Oak prerelease $tag is already published."
      ;;
    draft|absent)
      if [ "$state" = draft ]; then
        echo "Deleting the draft prerelease $tag an earlier run left."
        oak_release delete "$tag" --json >/dev/null || die "can't delete the draft prerelease $tag (details above)."
      fi
      (cd target/releases && oak_release create "$tag" --draft --prerelease --commit "$(state_get commit)" \
        --title "Nightly $build" --notes "Build of \`main\` at $(state_get commit). Rolling; deleted after $NIGHTLY_KEEP newer nightlies." \
        "${assets[@]}" --json) > "$STATE_DIR/nightly-draft.json" \
        || die "can't create the prerelease $tag (details above)."
      oak_release edit "$tag" --publish --finalize --json > "$STATE_DIR/nightly-release.json" \
        || die "can't publish the prerelease $tag (details above)."
      jq -e '.tag.draft == false and .tag.finalized == true' "$STATE_DIR/nightly-release.json" >/dev/null \
        || die "prerelease $tag is still a draft or not finalized after publishing."
      echo "Published Oak prerelease $tag ($OAK_URL/$OAK_REPO/releases/tag/$tag)."
      ;;
  esac

  oak_release list --limit 1000 --json > "$STATE_DIR/nightly-tags.json" || die "oak release list failed (details above)."
  nightly_tags_to_prune "$STATE_DIR/nightly-tags.json" "$NIGHTLY_KEEP" > "$STATE_DIR/nightly-prune" \
    || die "can't read the nightly prereleases (details above)."
  while IFS= read -r name; do
    echo "Deleting old nightly $name."
    oak_release delete "$name" --json >/dev/null || die "can't delete $name (details above)."
  done < "$STATE_DIR/nightly-prune"
}

# Create the Oak release as an invisible DRAFT tag on commit $2, with every
# asset. A draft left by an earlier run is deleted and recreated: its tag
# can't be repointed and its assets can't be replaced in place, and a draft
# is visible only to the repo's writers, so nothing public changes. Requires
# every asset, including the notarized app.
publish_draft() {
  local version="$1" commit="$2" f state notes="$STATE_DIR/release-notes.md"
  for f in "${EXPECTED[@]}"; do
    [ -f "target/releases/$f" ] || die "target/releases/$f is missing; a stable release ships all ${#EXPECTED[@]} assets."
  done
  state_is app_notarized true || die "OakMount.zip wasn't notarized in this run."
  state=$(oak_release_state "$version") || die "can't read release $version (details above)."
  case "$state" in
    absent) ;;
    draft)
      echo "Deleting the draft release $version an earlier run left."
      oak_release delete "$version" --json >/dev/null || die "can't delete the draft release $version (details above)."
      ;;
    *) die "release $version is already $state; fresh mode never changes a live release (published mid-run?). Deploy again so preflight picks the right mode." ;;
  esac
  # The version's release notes when docs/ has them, else a pointer to the repo.
  if [ -f "docs/release-${version#v}.md" ]; then
    cp "docs/release-${version#v}.md" "$notes"
  else
    printf '**Full Changelog**: %s/%s\n' "$OAK_URL" "$OAK_REPO" > "$notes"
  fi
  (cd target/releases && oak_release create "$version" --draft --commit "$commit" --title "$version" \
    --notes-file "$notes" "${EXPECTED[@]}" --json) > "$STATE_DIR/release-draft.json" \
    || die "can't create the draft release $version (details above)."
  echo "Draft release $version is on $OAK_REPO (invisible until the final step)."
}

# Stage on oak.space (content-addressed, unpromoted, not selectable yet).
# On failure, a version another actor already promoted goes to verification.
publish_stage() {
  local version="$1" rc state
  set +e
  make upload-release VERSION="$version" OAK_URL="$OAK_URL" OAK_ADMIN_API_KEY="$OAK_ADMIN_API_KEY" REQUIRE_MOUNTER=1
  rc=$?
  set -e
  if [ "$rc" -eq 0 ]; then
    echo "Staged $version on $OAK_URL (unpromoted)."
    return 0
  fi
  fetch_release_listing "$STATE_DIR/stage-releases.json" || die "staging failed (exit $rc) and the promotion check failed too."
  state=$(release_promotion_state "$version" "$STATE_DIR/stage-releases.json") \
    || die "staging failed (exit $rc) and $version's promotion state is ambiguous. Operator action required."
  [ "$state" = "promoted" ] || exit "$rc"
  state_put promoted_preexisting true
  warn "$version is already fully promoted on $OAK_URL; skipping staging and verifying against the promoted assets."
}

# Byte-level verification of both channels BEFORE promoting. Fresh mode
# compares everything to this run's build and writes the promote payload;
# resume/published modes check that the two channels agree.
publish_verify() {
  local version="$1" mode="$2" rel_dir="$STATE_DIR/release-verify" dl="$STATE_DIR/oakspace-verify"
  local published expected asset platform file want got served url
  rm -rf "$rel_dir" "$dl"
  mkdir -p "$rel_dir" "$dl"

  oak_release view "$version" --json > "$STATE_DIR/release-view.json" || die "can't read release $version (details above)."
  published=$(jq -r '.tag.assets[].name' "$STATE_DIR/release-view.json" | sort)
  expected=$(printf '%s\n' "${EXPECTED[@]}" | sort)
  if [ "$published" != "$expected" ]; then
    diff <(printf '%s\n' "$expected") <(printf '%s\n' "$published") || true
    die "release $version's assets don't exactly match the expected set (diff above: expected vs actual)."
  fi
  # `oak release download` checks each file against its content hash.
  oak_release download "$version" --dir "$rel_dir" --json > /dev/null || die "can't download release $version's assets (details above)."
  for asset in "${EXPECTED[@]}"; do
    [ -f "$rel_dir/$asset" ] || die "release asset $asset failed to download."
  done
  for asset in "${SUMMED[@]}"; do
    grep -qE " \*?$asset\$" "$rel_dir/SHA256SUMS" || die "SHA256SUMS doesn't list $asset."
  done
  (cd "$rel_dir" && shasum -a 256 -c SHA256SUMS) || die "SHA256SUMS doesn't match the release assets."

  if [ "$mode" = "fresh" ]; then
    for asset in "${EXPECTED[@]}"; do
      cmp -s "target/releases/$asset" "$rel_dir/$asset" || die "release asset $asset doesn't byte-match this run's build."
    done
    echo "Release: all ${#EXPECTED[@]} assets byte-identical to this run's build."
  fi

  # oak.space, per platform: the binary, the /sha256 install.sh reads, and
  # the minisig. The reference is this run's build in fresh mode, otherwise
  # the release's assets.
  local ref_dir="$rel_dir" payload='{"platforms":{}}' minisig_sha
  [ "$mode" = "fresh" ] && ref_dir="target/releases"
  for platform in "${PLATFORMS[@]}"; do
    file="$ref_dir/$(platform_file "$platform")"
    want=$(shasum -a 256 "$file" | awk '{print $1}')
    url="$OAK_URL/api/releases/$version/$platform"
    curl --fail-with-body -sS -o "$dl/$platform.bin" "$url" || die "$url is not being served."
    got=$(shasum -a 256 "$dl/$platform.bin" | awk '{print $1}')
    if [ "$got" != "$want" ]; then
      state_is promoted_preexisting true \
        && warn "$version was already promoted with different bytes; promoted versions are immutable (see the retry matrix in docs/release-0.102.0-runbook.md)."
      die "$url serves sha256 $got, expected $want."
    fi
    served=$(curl --fail-with-body -sS "$url/sha256") || die "$url/sha256 is not being served."
    [ "$(printf '%s' "$served" | tr -d '[:space:]')" = "$want" ] || die "$url/sha256 publishes '$served', expected $want."
    curl --fail-with-body -sS -o "$dl/$platform.minisig" "$url/minisig" || die "$url/minisig is not being served."
    cmp -s "$file.minisig" "$dl/$platform.minisig" || die "$url/minisig doesn't byte-match."
    minisig_sha=$(shasum -a 256 "$file.minisig" | awk '{print $1}')
    payload=$(jq --arg p "$platform" --arg s "$want" --arg m "$minisig_sha" \
      '.platforms[$p] = {sha256: $s, minisig_sha256: $m}' <<<"$payload")
    echo "OK $platform"
  done
  if [ "$mode" = "fresh" ]; then
    printf '%s\n' "$payload" | jq . > "$STATE_DIR/promote-payload.json"
    echo "Promote payload:"
    cat "$STATE_DIR/promote-payload.json"
  fi
  echo "Both channels verified for $version."
}

# Promote with the verified hash map (fresh), then require the re-queried
# server state to say promoted: an HTTP 200 from promote is not the
# postcondition. The resume/published modes only confirm.
publish_promote() {
  local version="$1" mode="$2" state payload="$STATE_DIR/promote-payload.json"
  if [ "$mode" = "fresh" ]; then
    [ -s "$payload" ] || die "no verified promote payload; refusing to promote unverified hashes."
    make promote-release VERSION="$version" OAK_URL="$OAK_URL" OAK_ADMIN_API_KEY="$OAK_ADMIN_API_KEY" PROMOTE_PAYLOAD="$payload"
  fi
  fetch_release_listing "$STATE_DIR/promote-confirm.json" || die "can't confirm promotion."
  state=$(release_promotion_state "$version" "$STATE_DIR/promote-confirm.json") \
    || die "$version's promotion state is ambiguous after promote. Operator action required."
  [ "$state" = "promoted" ] || die "expected $version to be promoted on $OAK_URL, but it's '$state'."
  if [ "$mode" = "fresh" ]; then
    release_spot_check_sha256 "$version" "$OAK_URL" "$payload" || die "post-promote /sha256 spot check failed."
  fi
  echo "$version is promoted on $OAK_URL."
}

case "${1:-}" in
  prepare) phase_prepare ;;
  preflight) phase_preflight ;;
  build-macos) phase_build_macos ;;
  build-linux) phase_build_linux ;;
  build-windows) phase_build_windows ;;
  sign-macos) phase_sign_macos ;;
  build-app) phase_build_app ;;
  notarize) phase_notarize ;;
  remove-keychain) phase_remove_keychain ;;
  verify) phase_verify ;;
  sign) phase_sign ;;
  deliver) phase_deliver ;;
  publish) phase_publish ;;
  *) awk 'NR == 1 { next } !/^#/ { exit } { sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}" >&2; exit 2 ;;
esac
