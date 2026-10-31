#!/usr/bin/env bash
# Set up an Apple Silicon Mac as Oak's release runner: the host-mode
# gitea-runner that .oak/workflows/release.yml runs on. Idempotent; re-run it
# after a macOS or Xcode update, or to re-register.
#
# Usage, on the Mac itself, as the user the runner will run as:
#   scripts/release-runner-setup.sh                      # install + check
#   OAK_RUNNER_TOKEN=<token> scripts/release-runner-setup.sh   # + register
#
# OAK_RUNNER_TOKEN is the organization's registration token (oak.space →
# the org's Settings → Runners). Without it, an existing registration in
# RUNNER_DIR is kept and nothing is registered.
#
# What it does:
#   * checks for full Xcode (the OakMount app needs xcodebuild, not just the
#     Command Line Tools) and rustup
#   * brew installs gitea-runner, colima + docker (the Linux build container),
#     llvm (llvm-lib for cargo-xwin), minisign, gh, jq, xcodegen
#   * cargo installs cargo-xwin; adds the darwin and windows-msvc rust targets
#   * configures colima (COLIMA_CPU/COLIMA_MEMORY/COLIMA_DISK) and starts it at
#     login (brew services)
#   * writes RUNNER_DIR/config.yaml: labels macos:host + arm64:host,
#     capacity 1, no cache server, PATH for brew and cargo
#   * registers the runner (with OAK_RUNNER_TOKEN) and runs `gitea-runner
#     daemon` from a LaunchAgent (space.oak.release-runner) at login
#   * prints the runner's uuid, which an org owner binds to the `production`
#     and `nightly` environments (docs/release-on-oak-ci.md)
#
# Variables: RUNNER_DIR (default ~/oak-runner), RUNNER_NAME (default
# oak-release-<hostname>), OAK_URL (default https://oak.space).
set -euo pipefail

RUNNER_DIR="${RUNNER_DIR:-$HOME/oak-runner}"
RUNNER_NAME="${RUNNER_NAME:-oak-release-$(hostname -s)}"
OAK_URL="${OAK_URL:-https://oak.space}"
COLIMA_CPU="${COLIMA_CPU:-6}"
COLIMA_MEMORY="${COLIMA_MEMORY:-12}"
COLIMA_DISK="${COLIMA_DISK:-100}"
AGENT_LABEL="space.oak.release-runner"
AGENT_PLIST="$HOME/Library/LaunchAgents/$AGENT_LABEL.plist"

die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }

[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || die "run this on an Apple Silicon Mac."
command -v brew >/dev/null || die "Homebrew is required: https://brew.sh"

step "Xcode"
xcode_dir="$(xcode-select -p 2>/dev/null || true)"
case "$xcode_dir" in
  *.app/Contents/Developer) xcodebuild -version | head -1 ;;
  *) die "full Xcode must be selected (found '${xcode_dir:-nothing}'). Install Xcode, then: sudo xcode-select -s /Applications/Xcode.app" ;;
esac
xcodebuild -checkFirstLaunchStatus || die "finish Xcode's first launch: sudo xcodebuild -runFirstLaunch"

step "Homebrew packages"
brew install gitea-runner colima docker llvm minisign gh jq xcodegen

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

step "gitea-runner config ($RUNNER_DIR)"
mkdir -p "$RUNNER_DIR/work" "$RUNNER_DIR/logs"
cd "$RUNNER_DIR"
runner_path="$(brew --prefix)/bin:$(brew --prefix)/sbin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
[ -f config.yaml ] || gitea-runner --config config.yaml config init
gitea-runner --config config.yaml config set runner.capacity 1
gitea-runner --config config.yaml config set runner.timeout 3h
gitea-runner --config config.yaml config set runner.envs.PATH "$runner_path"
gitea-runner --config config.yaml config set cache.enabled false
gitea-runner --config config.yaml config set host.workdir_parent "$RUNNER_DIR/work"
for label in macos:host arm64:host; do
  gitea-runner --config config.yaml config get runner.labels 2>/dev/null | grep -qx -- "$label" \
    || gitea-runner --config config.yaml config add runner.labels "$label"
done
cat config.yaml

step "Registration"
if [ -f .runner ]; then
  echo "Already registered ($RUNNER_DIR/.runner); keeping it. Delete .runner to register again."
elif [ -n "${OAK_RUNNER_TOKEN:-}" ]; then
  gitea-runner --config config.yaml register --no-interactive --instance "$OAK_URL" \
    --name "$RUNNER_NAME" --labels macos:host,arm64:host --token "$OAK_RUNNER_TOKEN"
else
  echo "Not registered yet. Re-run with OAK_RUNNER_TOKEN=<registration token from $OAK_URL → org Settings → Runners>."
fi

step "LaunchAgent ($AGENT_LABEL)"
if pgrep -fl 'gitea-runner .*daemon' | grep -v "$RUNNER_DIR/config.yaml" | grep -q .; then
  pgrep -fl 'gitea-runner .*daemon' >&2
  die "another gitea-runner daemon is running (above). Stop it first, so two daemons never share one registration."
fi
mkdir -p "$(dirname "$AGENT_PLIST")"
cat > "$AGENT_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$AGENT_LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$(brew --prefix)/bin/gitea-runner</string>
    <string>daemon</string>
    <string>--config</string>
    <string>$RUNNER_DIR/config.yaml</string>
  </array>
  <key>WorkingDirectory</key><string>$RUNNER_DIR</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>$runner_path</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$RUNNER_DIR/logs/runner.log</string>
  <key>StandardErrorPath</key><string>$RUNNER_DIR/logs/runner.log</string>
</dict>
</plist>
PLIST
if [ -f .runner ]; then
  launchctl bootout "gui/$(id -u)/$AGENT_LABEL" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$AGENT_PLIST"
  sleep 3
  tail -5 logs/runner.log || true
  printf '\nRunner uuid (bind it to the production and nightly environments): %s\n' "$(jq -r .uuid .runner)"
else
  echo "Wrote $AGENT_PLIST; it starts once the runner is registered (re-run with OAK_RUNNER_TOKEN)."
fi

cat <<'NEXT'

Next (docs/release-on-oak-ci.md):
  * Keep this Mac awake on power, or the runner goes offline and deploys
    can't be approved: System Settings → Energy → "Prevent automatic sleeping".
  * An org owner binds the runner uuid above to the `production` and
    `nightly` environments and adds the release secrets to both.
  * For nightly builds, install the daily deploy:
    scripts/nightly-schedule-setup.sh
NEXT
