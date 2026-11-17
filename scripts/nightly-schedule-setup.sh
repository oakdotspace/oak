#!/usr/bin/env bash
# Removes the old nightly LaunchAgent (space.oak.nightly-deploy) from the
# release Mac. It ran `oak ci deploy ... nightly` every day; Oak CI is gone,
# and the nightly build now comes from server/ops/marks/cli-nightly.sh on its
# own launchd timer (runbook 17).
#
# Usage, on the release Mac, as the release user:
#   scripts/nightly-schedule-setup.sh --remove
set -euo pipefail

AGENT_LABEL="space.oak.nightly-deploy"
AGENT_PLIST="$HOME/Library/LaunchAgents/$AGENT_LABEL.plist"

die() { echo "error: $*" >&2; exit 1; }

[ "$(uname -s)" = Darwin ] || die "run this on the release Mac."
[ "${1:-}" = "--remove" ] || die "Oak CI was removed, so there is no schedule to install; the nightly build is server/ops/marks/cli-nightly.sh (runbook 17). Pass --remove to uninstall the old agent."

launchctl bootout "gui/$(id -u)/$AGENT_LABEL" 2>/dev/null || true
rm -f "$AGENT_PLIST"
echo "Removed $AGENT_LABEL."
