#!/usr/bin/env bash
# Set up an Apple Silicon Mac as Oak's release Mac: the toolchain the
# `cli-released` follower (server/ops/marks/cli-release-follow.sh) and the
# nightly timer (server/ops/marks/cli-nightly.sh) build with. Idempotent;
# re-run it after a macOS or Command Line Tools update. Installing the
# follower itself is runbook 17, step 6 (server/ops/runbooks/).
#
# Usage, on the Mac itself, as the release user:
#   scripts/release-mac-setup.sh
#
# What it does:
#   * checks for the Xcode Command Line Tools (the macOS SDK, codesign, lipo)
#     and rustup
#   * brew installs colima + docker (the Linux build container), llvm
#     (llvm-lib for cargo-xwin), minisign, jq
#   * checks for the `oak` CLI, which follows the mark and publishes each
#     release (install it with `curl -fsSL https://oak.space/install | sh`)
#   * cargo installs cargo-xwin; adds the darwin and windows-msvc rust targets
#   * configures colima (COLIMA_CPU/COLIMA_MEMORY/COLIMA_DISK) and starts it at
#     login (brew services)
#   * says so when the old Oak CI runner (LaunchAgent space.oak.release-runner)
#     is still installed; it has nothing to do any more
#
# Variables: OAK_URL (default https://oak.space).
set -euo pipefail

OAK_URL="${OAK_URL:-https://oak.space}"
COLIMA_CPU="${COLIMA_CPU:-6}"
COLIMA_MEMORY="${COLIMA_MEMORY:-12}"
COLIMA_DISK="${COLIMA_DISK:-100}"
OLD_RUNNER_LABEL="space.oak.release-runner"
OLD_RUNNER_PLIST="$HOME/Library/LaunchAgents/$OLD_RUNNER_LABEL.plist"

die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }

[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || die "run this on an Apple Silicon Mac."
command -v brew >/dev/null || die "Homebrew is required: https://brew.sh"

step "Xcode Command Line Tools"
xcode-select -p >/dev/null 2>&1 || die "the Xcode Command Line Tools are required: xcode-select --install"
xcrun --show-sdk-path >/dev/null 2>&1 || die "no macOS SDK found. Reinstall the Command Line Tools: xcode-select --install"
for tool in codesign lipo; do
  command -v "$tool" >/dev/null || die "$tool not found. Reinstall the Command Line Tools: xcode-select --install"
done
echo "macOS SDK: $(xcrun --show-sdk-path)"

step "Homebrew packages"
brew install colima docker llvm minisign jq

step "oak CLI"
oak_bin="$(command -v oak || echo "$HOME/.local/bin/oak")"
[ -x "$oak_bin" ] || die "the oak CLI is required: curl -fsSL $OAK_URL/install | sh (then re-run this script)."
"$oak_bin" --version

step "Rust"
command -v rustup >/dev/null || die "rustup is required: https://rustup.rs (then re-run this script)."
rustup target add aarch64-apple-darwin x86_64-apple-darwin x86_64-pc-windows-msvc
command -v cargo-xwin >/dev/null || cargo install cargo-xwin --locked
cargo xwin --version

step "colima (Docker for the Linux build)"
if ! colima status >/dev/null 2>&1; then
  colima start --cpu "$COLIMA_CPU" --memory "$COLIMA_MEMORY" --disk "$COLIMA_DISK"
  # Hand it to launchd so it comes back at login; brew services restarts it
  # with the configuration colima just saved.
  colima stop
fi
brew services start colima
for _ in $(seq 1 60); do docker info >/dev/null 2>&1 && break; sleep 2; done
docker info --format 'docker {{.ServerVersion}} on {{.OperatingSystem}} ({{.Architecture}}, {{.NCPU}} CPUs)' \
  || die "docker can't reach colima. Check: colima status"

step "Old Oak CI runner"
if [ -f "$OLD_RUNNER_PLIST" ]; then
  echo "The Oak CI runner's LaunchAgent is still installed. Oak CI is gone, so remove it:"
  echo "  launchctl bootout gui/$(id -u)/$OLD_RUNNER_LABEL; rm $OLD_RUNNER_PLIST"
else
  echo "none"
fi

cat <<'NEXT'

Next (runbook 17, step 6):
  * Keep this Mac awake on power, or releases wait until it wakes:
    System Settings → Energy → "Prevent automatic sleeping".
  * Install the release secrets, the follower and the nightly timer from
    server/ops/marks/.
NEXT
