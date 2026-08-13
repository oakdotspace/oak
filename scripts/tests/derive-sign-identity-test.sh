#!/usr/bin/env bash
# Regression tests for scripts/derive-sign-identity.sh — the fb-112
# signing-identity preflight. Runs entirely on fixture files shaped like
# `security find-identity` output (no macOS, no keychain, no secrets needed).
#
# Covered matrix:
#   * exactly one valid Developer ID Application identity → derived (stdout
#     is exactly the SHA-1)
#   * zero identities at all → fail
#   * cert present but EXPIRED (in the all-identities listing, absent from
#     the valid listing — the rotation incident shape) → fail, expired hint
#   * two valid Developer ID Application identities → fail (ambiguous)
#   * other cert types present alongside exactly one Developer ID
#     Application → still derived (they must not confuse the count)
#   * duplicate listing of the SAME identity (two keychains) → derived
#   * --expect matching (case-insensitively) → derived
#   * --expect mismatching (stale MACOS_SIGN_IDENTITY) → fail
#   * garbage input → fail
#   * missing input file → usage error (exit 2)
#
# Run via `make test-release-scripts`. Exits nonzero on any failure.
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="$repo_root/scripts/derive-sign-identity.sh"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0
FAIL=0
ok()  { PASS=$((PASS + 1)); echo "PASS $1"; }
bad() { FAIL=$((FAIL + 1)); echo "FAIL $1" >&2; }

# The rotated-in (current) and rotated-out (stale) hashes mirror the incident:
# the imported P12 yields NEW_HASH while the stale Makefile default / secret
# held OLD_HASH.
NEW_HASH="9E883A8F0123456789ABCDEF0123456789ABCDEF"
OLD_HASH="981A6F2BB57517E4CA6A4F80CF3D1693F0F5191F"
SECOND_HASH="1111111111222222222233333333334444444444"

valid_one() {
  cat <<EOF
  1) $NEW_HASH "Developer ID Application: Oak Space Inc (452XFR864N)"
     1 valid identities found
EOF
}
valid_none() {
  cat <<'EOF'
     0 valid identities found
EOF
}
all_expired() {
  cat <<EOF
  1) $OLD_HASH "Developer ID Application: Oak Space Inc (452XFR864N)" (CSSMERR_TP_CERT_EXPIRED)
     1 identities found
EOF
}
valid_two() {
  cat <<EOF
  1) $NEW_HASH "Developer ID Application: Oak Space Inc (452XFR864N)"
  2) $SECOND_HASH "Developer ID Application: Oak Space Inc (452XFR864N)"
     2 valid identities found
EOF
}
valid_mixed_types() {
  cat <<EOF
  1) $SECOND_HASH "Apple Development: dev@oak.space (ABCDEF1234)"
  2) $NEW_HASH "Developer ID Application: Oak Space Inc (452XFR864N)"
  3) 5555555555666666666677777777778888888888 "Developer ID Installer: Oak Space Inc (452XFR864N)"
     3 valid identities found
EOF
}
valid_duplicated_same() {
  cat <<EOF
  1) $NEW_HASH "Developer ID Application: Oak Space Inc (452XFR864N)"
  2) $NEW_HASH "Developer ID Application: Oak Space Inc (452XFR864N)"
     2 valid identities found
EOF
}

# expect_derive NAME EXPECTED_STDOUT EXPECTED_RC [args to the script...]
expect_derive() {
  local name="$1" want_out="$2" want_rc="$3"; shift 3
  local out rc
  out=$("$SCRIPT" "$@" 2>"$TMP/err.log"); rc=$?
  if [ "$rc" -eq "$want_rc" ] && [ "$out" = "$want_out" ]; then
    ok "derive: $name"
  else
    bad "derive: $name (got out='$out' rc=$rc; want out='$want_out' rc=$want_rc)"
    sed 's/^/    /' "$TMP/err.log" >&2
  fi
}

# expect_stderr NAME PATTERN — grep the stderr of the LAST expect_derive run
expect_stderr() {
  local name="$1" pattern="$2"
  if grep -q "$pattern" "$TMP/err.log"; then
    ok "diagnostic: $name"
  else
    bad "diagnostic: $name (stderr does not match '$pattern'):"
    sed 's/^/    /' "$TMP/err.log" >&2
  fi
}

valid_one            > "$TMP/valid-one.txt"
valid_none           > "$TMP/valid-none.txt"
all_expired          > "$TMP/all-expired.txt"
valid_two            > "$TMP/valid-two.txt"
valid_mixed_types    > "$TMP/valid-mixed.txt"
valid_duplicated_same > "$TMP/valid-dup.txt"
: > "$TMP/all-empty.txt"
printf 'complete garbage\nnot an identity listing\n' > "$TMP/garbage.txt"

# --- the happy path: exactly one valid identity
expect_derive "one valid identity"            "$NEW_HASH" 0 "$TMP/valid-one.txt"
expect_derive "one valid (all-file too)"      "$NEW_HASH" 0 "$TMP/valid-one.txt" "$TMP/valid-one.txt"
expect_derive "other cert types don't count"  "$NEW_HASH" 0 "$TMP/valid-mixed.txt"
expect_derive "same identity listed twice (two keychains)" "$NEW_HASH" 0 "$TMP/valid-dup.txt"

# --- zero identities
expect_derive "zero identities"               "" 1 "$TMP/valid-none.txt" "$TMP/all-empty.txt"
expect_stderr "zero → not-imported hint"      "No Developer ID Application certificate is in the keychain at all"
expect_derive "zero identities (no all-file)" "" 1 "$TMP/valid-none.txt"

# --- expired cert: valid listing empty, all listing shows the old cert
expect_derive "expired cert (rotation incident shape)" "" 1 "$TMP/valid-none.txt" "$TMP/all-expired.txt"
expect_stderr "expired → expired/untrusted hint" "NOT valid (expired, revoked, or untrusted"
expect_stderr "expired → names the stale cert"   "$OLD_HASH"

# --- ambiguity
expect_derive "two valid identities (ambiguous)" "" 1 "$TMP/valid-two.txt"
expect_stderr "ambiguous → lists both"           "$SECOND_HASH"

# --- --expect cross-check (the MACOS_SIGN_IDENTITY secret)
expect_derive "--expect matches"                  "$NEW_HASH" 0 "$TMP/valid-one.txt" --expect "$NEW_HASH"
lower_new="$(printf '%s' "$NEW_HASH" | tr '[:upper:]' '[:lower:]')"
expect_derive "--expect matches case-insensitively" "$NEW_HASH" 0 "$TMP/valid-one.txt" --expect "$lower_new"
expect_derive "--expect stale secret (incident)"  "" 1 "$TMP/valid-one.txt" --expect "$OLD_HASH"
expect_stderr "stale secret → names both hashes"  "$OLD_HASH"
expect_stderr "stale secret → points at rotation" "certificate rotated"

# --- garbage / usage
expect_derive "garbage input"     "" 1 "$TMP/garbage.txt"
expect_derive "missing valid-file" "" 2 "$TMP/does-not-exist.txt"
expect_derive "no arguments"       "" 2

# --- --keychain scoping (fb-112 review finding): in keychain mode the script
# must run `security find-identity` ITSELF, scoped to exactly the given
# keychain for BOTH listings — an unscoped call searches the host's whole
# keychain search list, so a pre-existing Developer ID on a runner could
# satisfy the validation even when the imported P12 contained none.
# SECURITY_CMD is the test hook: this stub records its argv and serves the
# fixtures, so the assertions below prove the scoping argument is passed.
mkdir -p "$TMP/bin"
cat > "$TMP/bin/security-stub" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$STUB_LOG"
case " $* " in
  *" -v "*) cat "$STUB_VALID_FIXTURE" ;;
  *)        cat "$STUB_ALL_FIXTURE" ;;
esac
EOF
chmod +x "$TMP/bin/security-stub"
KC="$TMP/oak-signing.keychain-db"
: > "$KC"  # keychain mode requires the keychain file to exist

run_keychain() { # $1 valid fixture, $2 all fixture, then extra script args
  local valid="$1" all="$2"; shift 2
  : > "$TMP/security-args.log"
  STUB_LOG="$TMP/security-args.log" STUB_VALID_FIXTURE="$valid" STUB_ALL_FIXTURE="$all" \
    SECURITY_CMD="$TMP/bin/security-stub" "$SCRIPT" --keychain "$KC" "$@" 2>"$TMP/err.log"
}

out=$(run_keychain "$TMP/valid-one.txt" "$TMP/valid-one.txt"); rc=$?
if [ "$rc" -eq 0 ] && [ "$out" = "$NEW_HASH" ]; then
  ok "keychain: derives via the stubbed security binary"
else
  bad "keychain: derive via stub (got out='$out' rc=$rc)"; sed 's/^/    /' "$TMP/err.log" >&2
fi
if [ "$(wc -l < "$TMP/security-args.log" | tr -d ' ')" = "2" ] \
  && grep -qx "find-identity -v -p codesigning $KC" "$TMP/security-args.log" \
  && grep -qx "find-identity -p codesigning $KC" "$TMP/security-args.log"; then
  ok "keychain: BOTH find-identity listings are scoped to exactly the given keychain"
else
  bad "keychain: scoping args (recorded argv below)"
  sed 's/^/    /' "$TMP/security-args.log" >&2
fi

out=$(run_keychain "$TMP/valid-none.txt" "$TMP/all-expired.txt"); rc=$?
if [ "$rc" -eq 1 ] && [ -z "$out" ] && grep -q "NOT valid (expired, revoked, or untrusted" "$TMP/err.log"; then
  ok "keychain: expired-cert diagnostics flow through keychain mode"
else
  bad "keychain: expired via stub (got out='$out' rc=$rc)"; sed 's/^/    /' "$TMP/err.log" >&2
fi

out=$(run_keychain "$TMP/valid-one.txt" "$TMP/valid-one.txt" --expect "$OLD_HASH"); rc=$?
if [ "$rc" -eq 1 ] && grep -q "certificate rotated" "$TMP/err.log"; then
  ok "keychain: --expect cross-check works in keychain mode"
else
  bad "keychain: --expect via stub (rc=$rc)"; sed 's/^/    /' "$TMP/err.log" >&2
fi

out=$("$SCRIPT" --keychain "$TMP/no-such.keychain-db" 2>"$TMP/err.log"); rc=$?
if [ "$rc" -eq 2 ] && grep -q "does not exist" "$TMP/err.log"; then
  ok "keychain: missing keychain file is a usage error"
else
  bad "keychain: missing keychain (rc=$rc)"; sed 's/^/    /' "$TMP/err.log" >&2
fi

out=$("$SCRIPT" --keychain "$KC" "$TMP/valid-one.txt" 2>"$TMP/err.log"); rc=$?
if [ "$rc" -eq 2 ] && grep -q "mutually exclusive" "$TMP/err.log"; then
  ok "keychain: --keychain + positional files are mutually exclusive"
else
  bad "keychain: mutual exclusion (rc=$rc)"; sed 's/^/    /' "$TMP/err.log" >&2
fi

echo
echo "derive-sign-identity tests: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
