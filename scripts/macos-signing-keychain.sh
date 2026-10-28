#!/usr/bin/env bash
# Create or destroy the ephemeral Developer ID signing keychain on a
# PERSISTENT macOS runner — the one .oak/workflows/release.yml uses.
#
# GitHub's hosted macOS runners are thrown away after every job, so
# .github/workflows/_build-cli.yml can import the P12 and walk away. Oak's
# native release runner is a long-lived Mac: a keychain left on disk, or left
# in the user's keychain search list, outlives the job, and the next job on
# that machine (or anything else running as that user) could sign with it.
# This script makes the keychain's lifetime the job's, and nothing more.
#
# Usage:
#   macos-signing-keychain.sh create <keychain>
#       env: MACOS_CERT_P12_BASE64, MACOS_CERT_PASSWORD, MACOS_KEYCHAIN_PASSWORD
#   macos-signing-keychain.sh destroy <keychain>
#
#   <keychain>  path whose basename is oak-signing-*.keychain-db. The name is
#               enforced so destroy can never be pointed at login.keychain-db.
#
# create: deletes any signing keychain a previous job left behind (killed
#   before its destroy step), creates and unlocks the new keychain, imports
#   the P12 with its private key NON-EXTRACTABLE (`security import -x`: a job
#   that can reach the keychain can sign with it but cannot export the key),
#   grants codesign non-interactive access, and prepends the keychain to the
#   user search list so `security find-identity` and codesign see it.
# destroy: deletes the keychain and prunes every signing keychain (and every
#   entry whose file no longer exists) from the user search list. Idempotent;
#   safe to run when create never ran or failed halfway.
#
# Needs macOS `security`; SECURITY_CMD overrides it (test hook).
set -euo pipefail

SECURITY_CMD="${SECURITY_CMD:-security}"

usage() {
  awk 'NR == 1 { next } !/^#/ { exit } { sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}" >&2
  exit 2
}

[ $# -eq 2 ] || usage
ACTION="$1"
KEYCHAIN="$2"
case "$(basename "$KEYCHAIN")" in
  oak-signing-*.keychain-db) ;;
  *) echo "Error: keychain basename must match oak-signing-*.keychain-db (got '$KEYCHAIN')." >&2; exit 2 ;;
esac

is_signing_keychain() {
  case "$(basename "$1")" in oak-signing-*.keychain-db) return 0 ;; esac
  return 1
}

# The user keychain search list, one path per line, quotes and indent stripped.
search_list() {
  "$SECURITY_CMD" list-keychains -d user | sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//'
}

# Rewrite the search list without signing keychains or vanished files,
# optionally prepending one keychain. Deletes any stale signing keychain still
# on disk so its key material does not linger. No arrays: /bin/bash is 3.2,
# where expanding an empty array under `set -u` is an error.
rewrite_search_list() {
  local prepend="${1:-}" entry kept_file
  kept_file="$(mktemp)"
  search_list | while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    if is_signing_keychain "$entry"; then
      if [ "$entry" != "$prepend" ] && [ -f "$entry" ]; then
        echo "Removing stale signing keychain $entry" >&2
        "$SECURITY_CMD" delete-keychain "$entry" || rm -f "$entry"
      fi
      continue
    fi
    [ -e "$entry" ] || continue
    printf '%s\n' "$entry"
  done > "$kept_file"
  {
    [ -z "$prepend" ] || printf '%s\n' "$prepend"
    cat "$kept_file"
  } | tr '\n' '\0' | xargs -0 "$SECURITY_CMD" list-keychains -d user -s
  rm -f "$kept_file"
}

case "$ACTION" in
  create)
    : "${MACOS_CERT_P12_BASE64:?MACOS_CERT_P12_BASE64 is not set}"
    : "${MACOS_CERT_PASSWORD:?MACOS_CERT_PASSWORD is not set}"
    : "${MACOS_KEYCHAIN_PASSWORD:?MACOS_KEYCHAIN_PASSWORD is not set}"
    rewrite_search_list
    if [ -e "$KEYCHAIN" ]; then
      "$SECURITY_CMD" delete-keychain "$KEYCHAIN" 2>/dev/null || rm -f "$KEYCHAIN"
    fi
    CERT_DIR="$(mktemp -d)"
    trap 'rm -rf "$CERT_DIR"' EXIT
    printf '%s' "$MACOS_CERT_P12_BASE64" | base64 --decode > "$CERT_DIR/cert.p12"
    "$SECURITY_CMD" create-keychain -p "$MACOS_KEYCHAIN_PASSWORD" "$KEYCHAIN"
    # Lock after an hour idle and on sleep; the signing pass takes seconds.
    "$SECURITY_CMD" set-keychain-settings -lut 3600 "$KEYCHAIN"
    "$SECURITY_CMD" unlock-keychain -p "$MACOS_KEYCHAIN_PASSWORD" "$KEYCHAIN"
    "$SECURITY_CMD" import "$CERT_DIR/cert.p12" -k "$KEYCHAIN" -P "$MACOS_CERT_PASSWORD" -x -T /usr/bin/codesign
    "$SECURITY_CMD" set-key-partition-list -S apple-tool:,apple: -s -k "$MACOS_KEYCHAIN_PASSWORD" "$KEYCHAIN" > /dev/null
    rewrite_search_list "$KEYCHAIN"
    echo "Created signing keychain $KEYCHAIN" >&2
    ;;
  destroy)
    if [ -e "$KEYCHAIN" ]; then
      "$SECURITY_CMD" delete-keychain "$KEYCHAIN" || rm -f "$KEYCHAIN"
      echo "Deleted signing keychain $KEYCHAIN" >&2
    fi
    rewrite_search_list
    ;;
  *) usage ;;
esac
