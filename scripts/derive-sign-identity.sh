#!/usr/bin/env bash
# Derive the Developer ID Application codesigning identity from
# `security find-identity` output — the fb-112 signing-identity preflight.
#
# Incident this exists for: the Developer ID certificate rotated, the
# MACOS_SIGN_IDENTITY secret was unset, and the Makefile's stale hardcoded
# SIGN_IDENTITY default silently took the ad-hoc warning path — a full
# multi-platform release run then failed LATE on the post-build signature
# assertion. The imported P12 is the single source of truth for the signing
# identity, so the identity is DERIVED from the keychain after import and
# validated BEFORE any compilation; a configured MACOS_SIGN_IDENTITY (if any)
# is only a cross-check that must agree.
#
# Usage (CI — keychain mode):
#   derive-sign-identity.sh --keychain <path> [--expect <sha1>]
# Usage (fixture mode — pre-captured listing files):
#   derive-sign-identity.sh <valid-identities-file> [<all-identities-file>] [--expect <sha1>]
#
#   --keychain <path>        run `security find-identity` HERE, scoped to
#                            EXACTLY this keychain (both the -v valid listing
#                            and the all listing). The scoping is the point:
#                            an unscoped find-identity searches the whole
#                            keychain search list, so a pre-existing Developer
#                            ID on the build host could satisfy the validation
#                            even if the imported P12 contained none — scoped,
#                            the derived identity provably came from the
#                            imported P12's ephemeral keychain. The
#                            SECURITY_CMD env var overrides the `security`
#                            binary (test hook — fixtures stub it). Mutually
#                            exclusive with the positional listing files.
#   <valid-identities-file>  output of: security find-identity -v -p codesigning <keychain>
#                            (VALID identities only — expired/untrusted excluded)
#   <all-identities-file>    output of: security find-identity -p codesigning <keychain>
#                            (optional; all matching identities, including
#                            expired/untrusted ones — used only to sharpen the
#                            zero-valid diagnostic: "imported but expired" vs
#                            "not imported at all")
#   --expect <sha1>          optional cross-check: fail unless the derived
#                            identity equals this SHA-1 (e.g. the
#                            MACOS_SIGN_IDENTITY secret). Catches a stale
#                            secret explicitly instead of signing with
#                            something other than what the operator believes.
#
# Success: EXACTLY ONE valid "Developer ID Application" identity is present —
# its 40-hex SHA-1 is printed to stdout (all diagnostics go to stderr).
# Failure (exit 1): zero valid identities (missing or expired/untrusted cert),
# more than one (ambiguous — refusing to guess which cert signs the release),
# or an --expect mismatch.
#
# Testable without macOS: fixture mode takes plain files, and keychain mode's
# SECURITY_CMD hook lets a stub stand in for `security`. Regression tests:
# scripts/tests/derive-sign-identity-test.sh (run via `make test-release-scripts`).
set -euo pipefail

usage() {
  # Print the header comment block (everything between the shebang and the
  # first non-comment line) as the usage text.
  awk 'NR == 1 { next } !/^#/ { exit } { sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}" >&2
  exit 2
}

VALID_FILE=""
ALL_FILE=""
EXPECT=""
KEYCHAIN=""
while [ $# -gt 0 ]; do
  case "$1" in
    --expect)
      [ $# -ge 2 ] || { echo "Error: --expect needs a value." >&2; usage; }
      EXPECT="$2"; shift 2 ;;
    --keychain)
      [ $# -ge 2 ] || { echo "Error: --keychain needs a value." >&2; usage; }
      KEYCHAIN="$2"; shift 2 ;;
    -h|--help) usage ;;
    -*) echo "Error: unknown option '$1'." >&2; usage ;;
    *)
      if [ -z "$VALID_FILE" ]; then VALID_FILE="$1"
      elif [ -z "$ALL_FILE" ]; then ALL_FILE="$1"
      else echo "Error: too many positional arguments." >&2; usage; fi
      shift ;;
  esac
done

if [ -n "$KEYCHAIN" ]; then
  if [ -n "$VALID_FILE" ]; then
    echo "Error: --keychain and positional listing files are mutually exclusive (keychain mode runs 'security find-identity' itself)." >&2
    exit 2
  fi
  [ -f "$KEYCHAIN" ] || { echo "Error: keychain '$KEYCHAIN' does not exist — was the ephemeral signing keychain created and the P12 imported?" >&2; exit 2; }
  SECURITY_CMD="${SECURITY_CMD:-security}"
  LISTING_TMP="$(mktemp -d)"
  trap 'rm -rf "$LISTING_TMP"' EXIT
  VALID_FILE="$LISTING_TMP/identities-valid.txt"
  ALL_FILE="$LISTING_TMP/identities-all.txt"
  # Scoped to EXACTLY the given keychain: identities elsewhere in the host's
  # keychain search list must never satisfy (or pollute) this validation.
  "$SECURITY_CMD" find-identity -v -p codesigning "$KEYCHAIN" > "$VALID_FILE" || true
  "$SECURITY_CMD" find-identity -p codesigning "$KEYCHAIN" > "$ALL_FILE" || true
else
  [ -n "$VALID_FILE" ] || usage
  [ -f "$VALID_FILE" ] || { echo "Error: '$VALID_FILE' is not a readable file." >&2; exit 2; }
  if [ -n "$ALL_FILE" ] && [ ! -f "$ALL_FILE" ]; then
    echo "Error: '$ALL_FILE' is not a readable file." >&2; exit 2
  fi
fi

# Extract "HASH<TAB>quoted-name" pairs for Developer ID Application identities
# from `security find-identity` listing lines, which look like:
#   1) 9E883A8F0123456789ABCDEF0123456789ABCDEF "Developer ID Application: Oak Space Inc (452XFR864N)"
# (the all-identities listing may append a status suffix such as
# (CSSMERR_TP_CERT_EXPIRED) after the closing quote — tolerated by the regex).
extract_dev_id() {
  sed -nE 's/^[[:space:]]*[0-9]+\) ([0-9A-F]{40}) "(Developer ID Application[^"]*)".*$/\1\t\2/p' "$1" | sort -u
}

MATCHES="$(extract_dev_id "$VALID_FILE")"
COUNT=0
if [ -n "$MATCHES" ]; then
  COUNT="$(printf '%s\n' "$MATCHES" | wc -l | tr -d ' ')"
fi

if [ "$COUNT" -eq 0 ]; then
  echo "Error: no VALID 'Developer ID Application' identity found in the keychain." >&2
  if [ -n "$ALL_FILE" ]; then
    ALL_MATCHES="$(extract_dev_id "$ALL_FILE")"
    if [ -n "$ALL_MATCHES" ]; then
      echo "A Developer ID Application identity IS present but is NOT valid (expired, revoked, or untrusted — e.g. a rotated-out certificate):" >&2
      printf '%s\n' "$ALL_MATCHES" | sed 's/^/    /' >&2
      echo "Import the current Developer ID Application P12 (rotate the MACOS_CERT_P12_BASE64 secret in CI)." >&2
    else
      echo "No Developer ID Application certificate is in the keychain at all — the P12 import failed or the P12 holds the wrong certificate type." >&2
    fi
  fi
  echo "Keychain listing (valid identities) was:" >&2
  sed 's/^/    /' "$VALID_FILE" >&2
  exit 1
fi

if [ "$COUNT" -gt 1 ]; then
  echo "Error: $COUNT valid 'Developer ID Application' identities found — ambiguous; refusing to guess which certificate signs the release:" >&2
  printf '%s\n' "$MATCHES" | sed 's/^/    /' >&2
  echo "Keep exactly one Developer ID Application certificate in the signing keychain (the release P12 must contain only the current cert)." >&2
  exit 1
fi

HASH="$(printf '%s\n' "$MATCHES" | cut -f1)"
NAME="$(printf '%s\n' "$MATCHES" | cut -f2)"

if [ -n "$EXPECT" ]; then
  EXPECT_UC="$(printf '%s' "$EXPECT" | tr '[:lower:]' '[:upper:]')"
  if [ "$EXPECT_UC" != "$HASH" ]; then
    echo "Error: the configured expected identity does not match the certificate actually imported from the P12:" >&2
    echo "    expected (MACOS_SIGN_IDENTITY): $EXPECT" >&2
    echo "    derived from imported P12:      $HASH  \"$NAME\"" >&2
    echo "The secret is stale (certificate rotated?). Update or UNSET MACOS_SIGN_IDENTITY — the P12 is the source of truth and the derived identity is what signing uses." >&2
    exit 1
  fi
fi

echo "Derived signing identity: $HASH \"$NAME\"" >&2
printf '%s\n' "$HASH"
