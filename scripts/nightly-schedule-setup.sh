#!/usr/bin/env bash
# Deploy the nightly build once a day from the release Mac. Oak CI accepts a
# `schedule:` trigger but never fires it, so a LaunchAgent
# (space.oak.nightly-deploy) runs
#   oak ci deploy --environment nightly --workflow nightly --branch main --yes --json
# every day at NIGHTLY_HOUR:NIGHTLY_MINUTE local time; launchd runs a missed
# one when the Mac wakes. .oak/workflows/nightly.yml then builds, signs and
# publishes main's head, or does nothing when oak.space already serves it.
#
# The deploy approves main's head as whoever `oak` is logged in as on this
# Mac, so that account must be allowed to deploy `nightly`: a reviewer on the
# environment, or a writer when it doesn't require reviewers.
#
# Usage, on the release Mac, as the user the runner runs as:
#   scripts/nightly-schedule-setup.sh [CHECKOUT]   # install or update
#   scripts/nightly-schedule-setup.sh --remove     # uninstall
#
# CHECKOUT is an `oak` checkout of oak/oak, which `oak ci deploy` reads the
# repository from (default: NIGHTLY_CHECKOUT, a detached clone of its own,
# made here when it's missing). The deploy runs from there rather than from a
# checkout someone works in, so whatever branch, repository or half-finished
# change that checkout is on never reaches the schedule. Nothing in it is
# changed; the build itself fetches main's head on the runner.
#
# Variables: NIGHTLY_HOUR (default 2), NIGHTLY_MINUTE (default 17), OAK_BIN
# (default: the `oak` on PATH), LOG_DIR (default ~/oak-runner/logs),
# NIGHTLY_CHECKOUT (default ~/oak-runner/nightly-checkout).
set -euo pipefail

AGENT_LABEL="space.oak.nightly-deploy"
AGENT_PLIST="$HOME/Library/LaunchAgents/$AGENT_LABEL.plist"
NIGHTLY_HOUR="${NIGHTLY_HOUR:-2}"
NIGHTLY_MINUTE="${NIGHTLY_MINUTE:-17}"
LOG_DIR="${LOG_DIR:-$HOME/oak-runner/logs}"
NIGHTLY_CHECKOUT="${NIGHTLY_CHECKOUT:-$HOME/oak-runner/nightly-checkout}"

die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }

[ "$(uname -s)" = Darwin ] || die "run this on the release Mac."

if [ "${1:-}" = "--remove" ]; then
  launchctl bootout "gui/$(id -u)/$AGENT_LABEL" 2>/dev/null || true
  rm -f "$AGENT_PLIST"
  echo "Removed $AGENT_LABEL; nightly builds now only run when someone deploys them."
  exit 0
fi

oak_bin="${OAK_BIN:-$(command -v oak || true)}"
[ -x "$oak_bin" ] || die "no oak binary found; install it or set OAK_BIN."
if [ -z "${1:-}" ] && [ ! -e "$NIGHTLY_CHECKOUT" ]; then
  step "Dedicated checkout ($NIGHTLY_CHECKOUT)"
  mkdir -p "$(dirname "$NIGHTLY_CHECKOUT")"
  "$oak_bin" clone oak/oak "$NIGHTLY_CHECKOUT" --shallow --detached
fi
checkout="$(cd "${1:-$NIGHTLY_CHECKOUT}" && pwd)"
case "$NIGHTLY_HOUR:$NIGHTLY_MINUTE" in
  *[!0-9:]*) die "NIGHTLY_HOUR and NIGHTLY_MINUTE must be numbers." ;;
esac
[ "$NIGHTLY_HOUR" -le 23 ] && [ "$NIGHTLY_MINUTE" -le 59 ] || die "NIGHTLY_HOUR must be 0-23 and NIGHTLY_MINUTE 0-59."

step "Checkout ($checkout)"
(cd "$checkout" && "$oak_bin" status --json --compact >/dev/null) \
  || die "$checkout isn't an oak checkout. Pass one of oak/oak as the first argument."
echo "oak: $oak_bin ($("$oak_bin" --version))"

# Advisory: what tonight's deploy would see. It may well be blocked while the
# environment is still being set up; the schedule is installed regardless.
step "Deploy preview"
(cd "$checkout" && "$oak_bin" ci deploy --environment nightly --workflow nightly --branch main --dry-run) \
  || echo "The preview above isn't ready yet. Finish the nightly environment (docs/release-on-oak-ci.md) before the first scheduled run."

step "LaunchAgent ($AGENT_LABEL, daily at $(printf '%02d:%02d' "$NIGHTLY_HOUR" "$NIGHTLY_MINUTE"))"
mkdir -p "$LOG_DIR" "$(dirname "$AGENT_PLIST")"
cat > "$AGENT_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$AGENT_LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>-c</string>
    <string>printf '\n== %s\n' "\$(date -u +%Y-%m-%dT%H:%M:%SZ)"; exec "\$0" ci deploy --environment nightly --workflow nightly --branch main --yes --json</string>
    <string>$oak_bin</string>
  </array>
  <key>WorkingDirectory</key><string>$checkout</string>
  <key>StartCalendarInterval</key>
  <dict>
    <key>Hour</key><integer>$NIGHTLY_HOUR</integer>
    <key>Minute</key><integer>$NIGHTLY_MINUTE</integer>
  </dict>
  <key>StandardOutPath</key><string>$LOG_DIR/nightly-deploy.log</string>
  <key>StandardErrorPath</key><string>$LOG_DIR/nightly-deploy.log</string>
</dict>
</plist>
PLIST
plutil -lint "$AGENT_PLIST"
launchctl bootout "gui/$(id -u)/$AGENT_LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$AGENT_PLIST"

cat <<NEXT

Installed. Deploy logs: $LOG_DIR/nightly-deploy.log
To deploy one now:       launchctl kickstart gui/$(id -u)/$AGENT_LABEL
To stop the schedule:    $0 --remove
NEXT
