#!/usr/bin/env bash
# Regression tests for scripts/macos-signing-keychain.sh — the ephemeral
# signing keychain on the persistent macOS release runner. A stub stands in
# for `security` (via SECURITY_CMD) and keeps the user keychain search list in
# a file, so this runs anywhere (no macOS, no keychain, no secrets needed).
#
# Covered matrix:
#   * a keychain not named oak-signing-*.keychain-db (login.keychain-db) →
#     usage error, `security` never called
#   * create → keychain prepended to the search list, P12 imported with a
#     non-extractable key, codesign partition list set
#   * create after a killed job → the stale signing keychain is deleted and
#     dropped from the search list, entries whose file vanished are pruned
#   * create with a secret missing → fails, no keychain created
#   * destroy → keychain deleted, search list back to the non-signing entries
#   * destroy twice, or with no keychain at all → succeeds (idempotent)
#   * search-list paths containing spaces survive a rewrite intact
#
# Run via `make test-release-scripts`. Exits nonzero on any failure.
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="$repo_root/scripts/macos-signing-keychain.sh"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0
FAIL=0
ok()  { PASS=$((PASS + 1)); echo "PASS $1"; }
bad() { FAIL=$((FAIL + 1)); echo "FAIL $1" >&2; }

STATE="$TMP/search-list"
CALLS="$TMP/calls.log"
STUB="$TMP/security"
cat > "$STUB" <<'EOF'
#!/usr/bin/env bash
# Minimal `security` stand-in: the user search list lives in $STUB_STATE, one
# path per line; every call is appended to $STUB_CALLS.
set -euo pipefail
printf '%s\n' "$*" >> "$STUB_CALLS"
case "$1" in
  list-keychains) # -d user [-s PATH...]
    if [ "${4:-}" != "-s" ]; then
      while IFS= read -r p; do printf '    "%s"\n' "$p"; done < "$STUB_STATE"
    else
      shift 4
      : > "$STUB_STATE"
      for p in "$@"; do printf '%s\n' "$p" >> "$STUB_STATE"; done
    fi ;;
  create-keychain) touch "$4" ;; # -p PASSWORD PATH
  delete-keychain)
    rm -f "$2"
    grep -vxF "$2" "$STUB_STATE" > "$STUB_STATE.new" || true
    mv "$STUB_STATE.new" "$STUB_STATE" ;;
  set-keychain-settings|unlock-keychain|import|set-key-partition-list) ;;
  *) echo "stub security: unexpected command $1" >&2; exit 1 ;;
esac
EOF
chmod +x "$STUB"
export SECURITY_CMD="$STUB" STUB_STATE="$STATE" STUB_CALLS="$CALLS"

LOGIN="$TMP/Library Keychains/login.keychain-db"
mkdir -p "$(dirname "$LOGIN")"
touch "$LOGIN"
KC="$TMP/runner-temp/oak-signing-42.keychain-db"
STALE="$TMP/old-job/oak-signing-41.keychain-db"
mkdir -p "$(dirname "$KC")" "$(dirname "$STALE")"

secrets() {
  MACOS_CERT_P12_BASE64="$(printf 'fake-p12' | base64)" \
  MACOS_CERT_PASSWORD=p12-test-password \
  MACOS_KEYCHAIN_PASSWORD=keychain-test-password \
  "$@"
}
list_is() { # expected entries, one per argument
  local expected
  expected="$(printf '%s\n' "$@")"
  [ "$(cat "$STATE")" = "$expected" ]
}

# --- guard -------------------------------------------------------------------
printf '%s\n' "$LOGIN" > "$STATE"; : > "$CALLS"
"$SCRIPT" destroy "$LOGIN" 2>"$TMP/err.log"; rc=$?
if [ "$rc" -eq 2 ] && [ ! -s "$CALLS" ] && [ -f "$LOGIN" ]; then
  ok "guard: refuses a keychain not named oak-signing-*.keychain-db"
else
  bad "guard (rc=$rc)"; sed 's/^/    /' "$TMP/err.log" "$CALLS" >&2
fi

# --- create after a killed job ------------------------------------------------
touch "$STALE"
printf '%s\n' "$STALE" "$LOGIN" "$TMP/vanished/other.keychain-db" > "$STATE"; : > "$CALLS"
secrets "$SCRIPT" create "$KC" 2>"$TMP/err.log"; rc=$?
if [ "$rc" -eq 0 ] && list_is "$KC" "$LOGIN" && [ -f "$KC" ]; then
  ok "create: keychain prepended, vanished entry pruned"
else
  bad "create (rc=$rc)"; sed 's/^/    /' "$TMP/err.log" "$STATE" >&2
fi
if [ ! -e "$STALE" ] && grep -qxF "delete-keychain $STALE" "$CALLS"; then
  ok "create: stale signing keychain from a killed job deleted"
else
  bad "create: stale keychain"; sed 's/^/    /' "$CALLS" >&2
fi
if grep -q "^import .* -k $KC .* -x -T /usr/bin/codesign\$" "$CALLS" \
  && grep -q "^set-key-partition-list -S apple-tool:,apple: -s -k .* $KC\$" "$CALLS"; then
  ok "create: P12 imported non-extractable, codesign granted access"
else
  bad "create: import flags"; sed 's/^/    /' "$CALLS" >&2
fi

# --- destroy ----------------------------------------------------------------
: > "$CALLS"
"$SCRIPT" destroy "$KC" 2>"$TMP/err.log"; rc=$?
if [ "$rc" -eq 0 ] && list_is "$LOGIN" && [ ! -e "$KC" ]; then
  ok "destroy: keychain deleted, search list restored"
else
  bad "destroy (rc=$rc)"; sed 's/^/    /' "$TMP/err.log" "$STATE" >&2
fi

"$SCRIPT" destroy "$KC" 2>"$TMP/err.log"; rc=$?
if [ "$rc" -eq 0 ] && list_is "$LOGIN"; then
  ok "destroy: idempotent"
else
  bad "destroy twice (rc=$rc)"; sed 's/^/    /' "$TMP/err.log" "$STATE" >&2
fi

if [ -f "$LOGIN" ] && list_is "$LOGIN"; then
  ok "rewrite: a search-list path containing spaces survives intact"
else
  bad "rewrite: path with spaces"; sed 's/^/    /' "$STATE" >&2
fi

# --- create with a secret missing -----------------------------------------------
: > "$CALLS"
MACOS_CERT_P12_BASE64="$(printf 'fake-p12' | base64)" MACOS_CERT_PASSWORD=x \
  "$SCRIPT" create "$KC" 2>"$TMP/err.log"; rc=$?
if [ "$rc" -ne 0 ] && [ ! -e "$KC" ] && ! grep -q '^create-keychain' "$CALLS"; then
  ok "create: a missing secret fails before any keychain exists"
else
  bad "create without MACOS_KEYCHAIN_PASSWORD (rc=$rc)"; sed 's/^/    /' "$TMP/err.log" "$CALLS" >&2
fi

echo
echo "macos-signing-keychain tests: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
