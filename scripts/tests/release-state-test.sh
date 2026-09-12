#!/usr/bin/env bash
# Regression tests for the release pipeline's shell layer:
#   1. the strict shared promotion predicate (scripts/release-state.sh),
#      including the promoted_at type/format matrix
#   2. `make upload-release` staging behavior against a stub server —
#      rollback 404/410 and the B1 `release_writes_retired` error_code
#   3. `make promote-release` CAS hash-map payload construction
#   4. the post-promotion confirmation flow (re-queried list state gates the
#      flip, not the promote response)
#
# Run via `make test-release-scripts`. Requires: bash, jq, curl, shasum,
# python3 (for the stub HTTP server). No network access; nothing real is
# staged or promoted. Exits nonzero on any failure.
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

for tool in jq curl shasum python3 make; do
  command -v "$tool" >/dev/null 2>&1 || { echo "SKIP-FAIL: '$tool' is required for release-state tests" >&2; exit 1; }
done

source scripts/release-state.sh

V=v0.102.0-test
TMP="$(mktemp -d)"
PASS=0
FAIL=0
STUB_PID=""
RELEASES_BACKUP=""

cleanup() {
  [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null && wait "$STUB_PID" 2>/dev/null
  rm -rf "$TMP" target/releases
  if [ -n "$RELEASES_BACKUP" ] && [ -d "$RELEASES_BACKUP" ]; then
    mv "$RELEASES_BACKUP" target/releases
  fi
}
trap cleanup EXIT

ok()   { PASS=$((PASS + 1)); echo "PASS $1"; }
bad()  { FAIL=$((FAIL + 1)); echo "FAIL $1" >&2; }

# ---------------------------------------------------------------- fixtures
row() { printf '{"version":"%s","platform":"%s","promoted_at":%s}' "$1" "$2" "$3"; }
P='"2026-07-27T00:00:00Z"'
full_promoted_rows() { # $1 = promoted_at JSON value used for darwin-arm64
  printf '%s,%s,%s,%s,%s,%s' \
    "$(row "$V" darwin-arm64 "$1")" "$(row "$V" darwin-x86_64 "$P")" \
    "$(row "$V" linux-x86_64 "$P")" "$(row "$V" linux-arm64 "$P")" \
    "$(row "$V" windows-x86_64 "$P")" "$(row "$V" darwin-mounter "$P")"
}
# The /api/releases contract: a TOP-LEVEL array of row objects.
mklist() { printf '[%s]' "$1"; }

# expect_state NAME FIXTURE EXPECTED_OUTPUT EXPECTED_RC
expect_state() {
  local name="$1" fixture="$2" want_out="$3" want_rc="$4" out rc
  printf '%s' "$fixture" > "$TMP/list.json"
  out=$(release_promotion_state "$V" "$TMP/list.json" 2>"$TMP/err.log"); rc=$?
  if [ "$rc" -eq "$want_rc" ] && [ "$out" = "$want_out" ]; then
    ok "predicate: $name"
  else
    bad "predicate: $name (got out='${out}' rc=$rc; want out='${want_out}' rc=$want_rc)"
    sed 's/^/    /' "$TMP/err.log" >&2
  fi
}

# ------------------------------------------- 1. predicate matrix
expect_state "fully promoted (Z zone)"           "$(mklist "$(full_promoted_rows "$P")")" promoted 0
expect_state "fully promoted (fraction+offset)"  "$(mklist "$(full_promoted_rows '"2026-07-27T00:00:00.123+02:00"')")" promoted 0
expect_state "fully promoted (no zone)"          "$(mklist "$(full_promoted_rows '"2026-07-27T00:00:00"')")" promoted 0
expect_state "version absent"                    "$(mklist "$(row v0.1.0 darwin-arm64 "$P")")" unpromoted 0
expect_state "all staged (null)"                 "$(mklist "$(row "$V" darwin-arm64 null),$(row "$V" linux-x86_64 null)")" unpromoted 0
expect_state "mixed staged/promoted"             "$(mklist "$(row "$V" darwin-arm64 "$P"),$(row "$V" linux-x86_64 null)")" "" 2
expect_state "duplicate platform"                "$(mklist "$(row "$V" darwin-arm64 "$P"),$(row "$V" darwin-arm64 "$P")")" "" 2
expect_state "malformed JSON"                    '{"releases": [broken' "" 2
expect_state "missing platform (5 of 6)"         "$(mklist "$(row "$V" darwin-arm64 "$P"),$(row "$V" darwin-x86_64 "$P"),$(row "$V" linux-x86_64 "$P"),$(row "$V" linux-arm64 "$P"),$(row "$V" windows-x86_64 "$P")")" "" 2
expect_state "extra platform (7th slug)"         "$(mklist "$(full_promoted_rows "$P"),$(row "$V" freebsd-x86_64 "$P")")" "" 2
expect_state "missing promoted_at key"           "$(jq -nc --arg v "$V" '[{version: $v, platform: "darwin-arm64"}]')" "" 2
# envelope validation: only a top-level array of objects is acceptable
expect_state "envelope: releases wrapper object" "$(jq -nc --arg v "$V" '{releases: [{version: $v, platform: "darwin-arm64", promoted_at: null}]}')" "" 2
expect_state "envelope: metadata wrapper object" "$(jq -nc --arg v "$V" '{metadata: [{version: $v, platform: "darwin-arm64", promoted_at: null}]}')" "" 2
expect_state "envelope: top-level string"        '"promoted"' "" 2
expect_state "envelope: top-level number"        '42' "" 2
expect_state "envelope: non-object element"      "$(jq -nc --arg v "$V" '[{version: $v, platform: "darwin-arm64", promoted_at: null}, "stray"]')" "" 2
# slug validation: empty-string and whitespace-padded slugs must be rejected
# explicitly (a textual sort|xargs comparison would normalize the padding away)
expect_state "slug: empty string"                "$(jq -nc --arg v "$V" '[{version: $v, platform: "", promoted_at: null}]')" "" 2
expect_state "slug: whitespace-padded (attack)"  "$(mklist "$(row "$V" ' darwin-arm64' "$P"),$(row "$V" darwin-x86_64 "$P"),$(row "$V" linux-x86_64 "$P"),$(row "$V" linux-arm64 "$P"),$(row "$V" windows-x86_64 "$P"),$(row "$V" darwin-mounter "$P")")" "" 2
# promoted_at type/format matrix — reviewer reproductions first:
expect_state "promoted_at number (1)"            "$(mklist "$(full_promoted_rows '1')")" "" 2
expect_state "promoted_at object"                "$(mklist "$(full_promoted_rows '{"bad":true}')")" "" 2
expect_state "promoted_at boolean"               "$(mklist "$(full_promoted_rows 'true')")" "" 2
expect_state "promoted_at array"                 "$(mklist "$(full_promoted_rows '[1]')")" "" 2
expect_state "promoted_at empty string"          "$(mklist "$(full_promoted_rows '""')")" "" 2
expect_state "promoted_at garbage string"        "$(mklist "$(full_promoted_rows '"yesterday"')")" "" 2
# row-level validation BEFORE version filtering: a malformed row must fail
# closed even when it would not match the target version — the old
# filter-first order read these as "no matching release" (fail-open)
expect_state "row: missing version key"          '[{"platform":"darwin-arm64","promoted_at":null}]' "" 2
expect_state "row: renamed version key"          '[{"ver":"v0.1.0","platform":"darwin-arm64","promoted_at":null}]' "" 2
expect_state "row: empty object"                 '[{}]' "" 2
expect_state "row: numeric version"              '[{"version":1,"platform":"darwin-arm64","promoted_at":null}]' "" 2
expect_state "row: other-version bad promoted_at" '[{"version":"v0.1.0","platform":"darwin-arm64","promoted_at":7}]' "" 2

# ------------------------------------------- stub server for make targets
if [ -d target/releases ]; then
  RELEASES_BACKUP="target/releases.test-backup.$$"
  mv target/releases "$RELEASES_BACKUP"
fi
mkdir -p target/releases
for p in darwin-arm64 darwin-x86_64 linux-x86_64 linux-arm64; do
  echo "bin-$p" > "target/releases/oak-$p"
  echo "sig-$p" > "target/releases/oak-$p.minisig"
done
echo win > target/releases/oak-windows-x86_64.exe
echo winsig > target/releases/oak-windows-x86_64.exe.minisig

cat > "$TMP/stub.py" <<'EOF'
import sys, json, os, re
from http.server import BaseHTTPRequestHandler, HTTPServer
MODE, PORT, LIST_FILE, RECORD_DIR = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
PAYLOAD_FILE = sys.argv[5] if len(sys.argv) > 5 else ""
SENTINEL = sys.argv[6] if len(sys.argv) > 6 else ""
AUTH = sys.argv[7] if len(sys.argv) > 7 else ""
def record(line):
    if SENTINEL:
        line += " sentinel=" + ("yes" if os.path.exists(SENTINEL) else "no")
    with open(os.path.join(RECORD_DIR, 'requests.log'), 'a') as f:
        f.write(line + "\n")
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def authed(self):
        # Auth mode: mirrors the B2 contract — every /api/releases* request
        # must carry the admin Bearer key; without it the server 401s (the
        # same shape a PRE-B2 server gives the listing even WITH the key).
        if not AUTH:
            return True
        if self.headers.get('Authorization') == 'Bearer ' + AUTH:
            return True
        self.reply(401, {"error": "Authentication required"})
        return False
    def do_GET(self):
        record("GET " + self.path)
        if not self.authed():
            return
        m = re.match(r'^/api/releases/[^/]+/([^/]+)/sha256$', self.path)
        if self.path == '/api/releases':
            body = open(LIST_FILE, 'rb').read()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif m:
            platform = m.group(1)
            if MODE == 'sha-missing':
                self.reply(404, {"error": "Not found"})
            elif MODE == 'sha-wrong':
                self.text(200, "deadbeef" * 8)
            else:
                try:
                    sha = json.load(open(PAYLOAD_FILE))['platforms'][platform]['sha256']
                    self.text(200, sha)
                except Exception:
                    self.reply(404, {"error": "no such platform in payload"})
        else:
            self.reply(404, {"error": "Not found"})
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length') or 0))
        record("POST " + self.path)
        if not self.authed():
            return
        if self.path == '/api/releases/stage':
            if MODE == 'rollback':
                self.reply(404, {"error": "Not found"})
            elif MODE == 'retired':
                self.reply(410, {"error": "gone", "error_code": "release_writes_retired"})
            elif MODE == 'stage-adopt':
                slugs = ["darwin-arm64", "darwin-x86_64", "linux-x86_64",
                         "linux-arm64", "windows-x86_64", "darwin-mounter"]
                slug = next((p for p in slugs if p.encode() in body), "unknown")
                self.reply(200, {"already_promoted": True,
                                 "sha256": "stored-bin-" + slug,
                                 "minisig_sha256": "stored-sig-" + slug})
            else:
                self.reply(200, {"staged": True})
        elif self.path.endswith('/promote'):
            with open(os.path.join(RECORD_DIR, 'promote-body.json'), 'wb') as f:
                f.write(body)
            if MODE == 'promote-404':
                self.reply(404, {"error": "Not found"})
            elif MODE == 'promote-409':
                self.reply(409, {"error": "cas_mismatch", "platform": "linux-x86_64", "expected": "aaaa", "actual": "bbbb"})
            elif MODE == 'promote-500':
                self.reply(500, {"error": "stub promote explosion"})
            else:
                self.reply(200, {"promoted": True})
        else:
            self.reply(404, {"error": "Not found"})
    def text(self, code, s):
        body = s.encode()
        self.send_response(code)
        self.send_header('Content-Type', 'text/plain')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def reply(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
HTTPServer(('127.0.0.1', PORT), H).serve_forever()
EOF

mklist "$(full_promoted_rows "$P")" > "$TMP/list-promoted.json"
mklist "$(row "$V" darwin-arm64 "$P"),$(row "$V" linux-x86_64 null)" > "$TMP/list-mixed.json"

REC="$TMP/rec"
start_stub() { # $1 mode, $2 port, $3 list file, [$4 payload file for /sha256], [$5 build sentinel], [$6 required bearer token]
  rm -rf "$REC"; mkdir -p "$REC"
  python3 "$TMP/stub.py" "$1" "$2" "$3" "$REC" "${4:-}" "${5:-}" "${6:-}" & STUB_PID=$!
  sleep 0.4
}
stop_stub() { kill "$STUB_PID" 2>/dev/null; wait "$STUB_PID" 2>/dev/null; STUB_PID=""; }
requests()  { [ -f "$REC/requests.log" ] && wc -l < "$REC/requests.log" | tr -d ' ' || echo 0; }

# ------------------------------------------- 2. staging behavior
start_stub rollback 8471 "$TMP/list-promoted.json"
out=$(make upload-release VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL=http://127.0.0.1:8471 2>&1); rc=$?
if [ "$rc" -ne 0 ] && grep -q "does not speak the staging protocol (rolled back?)" <<<"$out"; then
  ok "stage: 404 rollback message + nonzero exit"
else
  bad "stage: 404 rollback (rc=$rc)"; sed 's/^/    /' <<<"$out" >&2
fi
stop_stub

start_stub retired 8471 "$TMP/list-promoted.json"
out=$(make upload-release VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL=http://127.0.0.1:8471 2>&1); rc=$?
if [ "$rc" -ne 0 ] && grep -q "release_writes_retired" <<<"$out" && grep -q "does not speak the staging protocol" <<<"$out"; then
  ok "stage: 410 release_writes_retired recognized as rollback"
else
  bad "stage: release_writes_retired (rc=$rc)"; sed 's/^/    /' <<<"$out" >&2
fi
stop_stub

start_stub ok 8471 "$TMP/list-promoted.json"
out=$(make upload-release VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL=http://127.0.0.1:8471 2>&1); rc=$?
if [ "$rc" -eq 0 ] && grep -q "STAGED, not yet selectable" <<<"$out"; then
  ok "stage: happy path stages all platforms"
else
  bad "stage: happy path (rc=$rc)"; sed 's/^/    /' <<<"$out" >&2
fi

stop_stub

# ------------------------------------------- 3. canonical-six promotion gate
# Pre-POST failures use a dead OAK_URL: if the target ever attempted the
# request, the error would be a connection failure, not the canonical-six
# message — so a PASS here proves no POST was made.
DEAD=http://127.0.0.1:9

# 3a. computed map with the mounter missing must fail BEFORE any POST
out=$(make promote-release VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL="$DEAD" 2>&1); rc=$?
if [ "$rc" -ne 0 ] && grep -q "canonical six" <<<"$out" && grep -q "darwin-mounter" <<<"$out"; then
  ok "promote: computed map without mounter fails pre-POST naming darwin-mounter"
else
  bad "promote: computed-map five-platform gate (rc=$rc)"; sed 's/^/    /' <<<"$out" >&2
fi

# 3b. PROMOTE_PAYLOAD with five keys must fail pre-POST naming the missing slug
jq -n '{platforms: {"darwin-arm64":{sha256:"a",minisig_sha256:"b"},"darwin-x86_64":{sha256:"a",minisig_sha256:"b"},"linux-x86_64":{sha256:"a",minisig_sha256:"b"},"linux-arm64":{sha256:"a",minisig_sha256:"b"},"windows-x86_64":{sha256:"a",minisig_sha256:"b"}}}' > "$TMP/five.json"
out=$(make promote-release VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL="$DEAD" PROMOTE_PAYLOAD="$TMP/five.json" 2>&1); rc=$?
if [ "$rc" -ne 0 ] && grep -q "not exactly the canonical six" <<<"$out" && grep -q "missing: darwin-mounter" <<<"$out"; then
  ok "promote: five-key PROMOTE_PAYLOAD fails pre-POST (missing: darwin-mounter)"
else
  bad "promote: five-key payload gate (rc=$rc)"; sed 's/^/    /' <<<"$out" >&2
fi

# 3c. PROMOTE_PAYLOAD with an extra key must fail pre-POST naming the extra slug
jq '.platforms."darwin-mounter" = {sha256:"a",minisig_sha256:"b"} | .platforms."freebsd-x86_64" = {sha256:"a",minisig_sha256:"b"}' "$TMP/five.json" > "$TMP/seven.json"
out=$(make promote-release VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL="$DEAD" PROMOTE_PAYLOAD="$TMP/seven.json" 2>&1); rc=$?
if [ "$rc" -ne 0 ] && grep -q "not exactly the canonical six" <<<"$out" && grep -q "extra: freebsd-x86_64" <<<"$out"; then
  ok "promote: seven-key PROMOTE_PAYLOAD fails pre-POST (extra: freebsd-x86_64)"
else
  bad "promote: seven-key payload gate (rc=$rc)"; sed 's/^/    /' <<<"$out" >&2
fi

# 3d. with all six artifacts present, the computed CAS map has exactly the
# canonical six keys, hashes match shasum, and the POST succeeds (stub 200)
echo mounter > target/releases/OakMount.zip
echo mountersig > target/releases/OakMount.zip.minisig
start_stub ok 8471 "$TMP/list-promoted.json"
out=$(make promote-release VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL=http://127.0.0.1:8471 2>&1); rc=$?
payload=target/releases/promote-payload.json
want_sha=$(shasum -a 256 target/releases/oak-darwin-arm64 | awk '{print $1}')
want_zip_sha=$(shasum -a 256 target/releases/OakMount.zip | awk '{print $1}')
want_zip_msha=$(shasum -a 256 target/releases/OakMount.zip.minisig | awk '{print $1}')
if [ "$rc" -eq 0 ] \
  && jq -e . "$payload" >/dev/null 2>&1 \
  && [ "$(jq -r '.platforms."darwin-arm64".sha256' "$payload")" = "$want_sha" ] \
  && [ "$(jq -r '.platforms."darwin-mounter".sha256' "$payload")" = "$want_zip_sha" ] \
  && [ "$(jq -r '.platforms."darwin-mounter".minisig_sha256' "$payload")" = "$want_zip_msha" ] \
  && [ "$(jq '.platforms | length' "$payload")" = "6" ]; then
  ok "promote: computed CAS hash map is the canonical six and matches shasum"
else
  bad "promote: canonical-six CAS payload (rc=$rc)"; sed 's/^/    /' <<<"$out" >&2; jq . "$payload" 2>/dev/null | sed 's/^/    /' >&2
fi
stop_stub

# ------------------------------------------- 4. post-promotion confirmation
# The workflow's confirm_promoted gate: re-fetch the list and require the
# strict predicate to say "promoted" — a 200 from promote is NOT enough.
start_stub ok 8471 "$TMP/list-promoted.json"
curl -sS -o "$TMP/confirm.json" http://127.0.0.1:8471/api/releases
if state=$(release_promotion_state "$V" "$TMP/confirm.json" 2>/dev/null) && [ "$state" = "promoted" ]; then
  ok "confirm: re-queried fully-promoted list passes the strict predicate"
else
  bad "confirm: fully-promoted list (state='${state:-}')"
fi
stop_stub

start_stub ok 8471 "$TMP/list-mixed.json"
curl -sS -o "$TMP/confirm.json" http://127.0.0.1:8471/api/releases
if state=$(release_promotion_state "$V" "$TMP/confirm.json" 2>"$TMP/err.log"); then
  bad "confirm: mixed list must FAIL the gate even though promote returned 200 (got state='$state')"
else
  ok "confirm: promote-200-but-mixed-list fails closed (flip would be blocked)"
fi
stop_stub

# ------------------------------------------- 5. mode-decision matrix
expect_mode() { # $1 name, $2 gh, $3 promo, $4 want_out, $5 want_rc
  local out rc
  out=$(release_decide_mode "$2" "$3" 2>/dev/null); rc=$?
  if [ "$rc" -eq "$5" ] && [ "$out" = "$4" ]; then
    ok "mode: $1"
  else
    bad "mode: $1 (got out='${out}' rc=$rc; want out='$4' rc=$5)"
  fi
}
expect_mode "absent+unpromoted -> fresh"           absent    unpromoted fresh 0
expect_mode "draft+unpromoted -> fresh"            draft     unpromoted fresh 0
expect_mode "draft+promoted -> resume"             draft     promoted   resume-post-promotion 0
expect_mode "published+promoted -> published"      published promoted   already-published 0
expect_mode "published+unpromoted -> inconsistent" published unpromoted "" 3
expect_mode "absent+promoted -> inconsistent"      absent    promoted   "" 3
expect_mode "garbage state -> inconsistent"        banana    promoted   "" 3

# ------------------------------------------- 6. draft-action decision
expect_draft() { # $1 name, $2 is_draft, $3 target, $4 sha, $5 want_out, $6 want_rc
  local out rc
  out=$(release_draft_action "$2" "$3" "$4" 2>/dev/null); rc=$?
  if [ "$rc" -eq "$6" ] && [ "$out" = "$5" ]; then
    ok "draft: $1"
  else
    bad "draft: $1 (got out='${out}' rc=$rc; want out='$5' rc=$6)"
  fi
}
expect_draft "same-SHA draft -> clobber"            true  sha1 sha1 clobber 0
expect_draft "stale draft -> retarget-then-clobber" true  sha0 sha1 retarget-then-clobber 0
expect_draft "published -> refuse"                  false sha1 sha1 "" 3
expect_draft "garbage isDraft -> refuse"            banana sha1 sha1 "" 3

# ------------------------------------------- 7. post-promote /sha256 spot check
ATTESTED=target/releases/promote-payload.json  # written by test 3d (six keys)
start_stub sha-right 8471 "$TMP/list-promoted.json" "$ATTESTED"
if release_spot_check_sha256 "$V" http://127.0.0.1:8471 "$ATTESTED" >/dev/null 2>&1; then
  ok "spot check: matching /sha256 for all six platforms passes"
else
  bad "spot check: matching hashes should pass"
fi
stop_stub
start_stub sha-wrong 8471 "$TMP/list-promoted.json" "$ATTESTED"
if release_spot_check_sha256 "$V" http://127.0.0.1:8471 "$ATTESTED" >/dev/null 2>&1; then
  bad "spot check: wrong served hash must fail"
else
  ok "spot check: wrong served hash fails before the flip"
fi
stop_stub
start_stub sha-missing 8471 "$TMP/list-promoted.json" "$ATTESTED"
if release_spot_check_sha256 "$V" http://127.0.0.1:8471 "$ATTESTED" >/dev/null 2>&1; then
  bad "spot check: missing /sha256 endpoint must fail"
else
  ok "spot check: missing /sha256 endpoint fails before the flip"
fi
stop_stub

# ------------------------------------------- 8. promote error handling
cp "$ATTESTED" "$TMP/attested.json"
for mode in promote-404 promote-409 promote-500; do
  start_stub "$mode" 8471 "$TMP/list-promoted.json"
  out=$(make promote-release VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL=http://127.0.0.1:8471 PROMOTE_PAYLOAD="$TMP/attested.json" 2>&1); rc=$?
  case "$mode" in
    promote-404) want="does not speak the staging protocol" ;;
    promote-409) want="cas_mismatch" ;;
    promote-500) want="stub promote explosion" ;;
  esac
  if [ "$rc" -ne 0 ] && grep -q "$want" <<<"$out"; then
    ok "promote error: $mode fails with the expected detail"
  else
    bad "promote error: $mode (rc=$rc)"; sed 's/^/    /' <<<"$out" >&2
  fi
  stop_stub
done
out=$(make promote-release VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL=http://127.0.0.1:9 PROMOTE_PAYLOAD="$TMP/attested.json" 2>&1); rc=$?
if [ "$rc" -ne 0 ] && grep -q "HTTP 000" <<<"$out"; then
  ok "promote error: network-000 fails with HTTP 000 reported"
else
  bad "promote error: network-000 (rc=$rc)"; sed 's/^/    /' <<<"$out" >&2
fi

# ------------------------------------------- 9. supplied-payload transmission
start_stub ok 8471 "$TMP/list-promoted.json"
out=$(make promote-release VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL=http://127.0.0.1:8471 PROMOTE_PAYLOAD="$TMP/attested.json" 2>&1); rc=$?
if [ "$rc" -eq 0 ] && cmp -s "$TMP/attested.json" "$REC/promote-body.json"; then
  ok "payload transmission: stub received the PROMOTE_PAYLOAD file byte-for-byte"
else
  bad "payload transmission (rc=$rc)"; diff "$TMP/attested.json" "$REC/promote-body.json" 2>&1 | sed 's/^/    /' >&2
fi
stop_stub

# ------------------------------------------- 10. release-publish end-to-end
# 10a. preflight failure: with the mounter missing, release-publish must fail
# BEFORE any request — the stub's request log stays empty.
mv target/releases/OakMount.zip "$TMP/OakMount.zip.aside"
mv target/releases/OakMount.zip.minisig "$TMP/OakMount.zip.minisig.aside"
start_stub ok 8471 "$TMP/list-promoted.json"
out=$(make release-publish VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL=http://127.0.0.1:8471 2>&1); rc=$?
if [ "$rc" -ne 0 ] && grep -q "release preflight failed" <<<"$out" && grep -q "OakMount.zip" <<<"$out" && [ "$(requests)" = "0" ]; then
  ok "release-publish: missing mounter fails preflight with ZERO requests made"
else
  bad "release-publish preflight (rc=$rc requests=$(requests))"; sed 's/^/    /' <<<"$out" >&2
fi
stop_stub
mv "$TMP/OakMount.zip.aside" target/releases/OakMount.zip
mv "$TMP/OakMount.zip.minisig.aside" target/releases/OakMount.zip.minisig

# 10b. success: preflight passes, six staging POSTs then one promote POST.
start_stub ok 8471 "$TMP/list-promoted.json"
out=$(make release-publish VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL=http://127.0.0.1:8471 2>&1); rc=$?
stage_n=$(grep -c "POST /api/releases/stage" "$REC/requests.log" 2>/dev/null || echo 0)
promote_n=$(grep -c "/promote" "$REC/requests.log" 2>/dev/null || echo 0)
if [ "$rc" -eq 0 ] && [ "$stage_n" = "6" ] && [ "$promote_n" = "1" ]; then
  ok "release-publish: end-to-end success (6 stage requests + 1 promote)"
else
  bad "release-publish e2e (rc=$rc stage=$stage_n promote=$promote_n)"; sed 's/^/    /' <<<"$out" >&2
fi
stop_stub

# ------------------------------------------- 11. make -j cross-step ordering
# release-all's build -> preflight -> stage -> promote ordering must be
# encoded in RECIPE BODIES: recipe lines run sequentially even under -j,
# while a prerequisite list (the old shape,
# `release-all: build-release-all release-publish`) lets parallel make start
# preflight/staging while builds/signing still run — with a complete stale
# target/releases/ present, that STAGES STALE BYTES. Sandbox setup: an
# overlay Makefile in its own directory includes the real Makefile
# (absolute path) and replaces build-release-all with a slow fake that
# writes fresh artifacts + a completion sentinel only after a delay; the
# stub tags every request with whether the sentinel existed when it
# arrived. Sub-makes re-read ./Makefile in the sandbox, so the override
# holds through the whole $(MAKE) chain.
JD="$TMP/jtest"
mkdir -p "$JD/target/releases"
for p in darwin-arm64 darwin-x86_64 linux-x86_64 linux-arm64; do
  echo "stale-$p" > "$JD/target/releases/oak-$p"
  echo "stale-sig-$p" > "$JD/target/releases/oak-$p.minisig"
done
echo stale-win > "$JD/target/releases/oak-windows-x86_64.exe"
echo stale-winsig > "$JD/target/releases/oak-windows-x86_64.exe.minisig"
echo stale-mounter > "$JD/target/releases/OakMount.zip"
echo stale-mountersig > "$JD/target/releases/OakMount.zip.minisig"
echo "include $repo_root/Makefile" > "$JD/Makefile"
cat >> "$JD/Makefile" <<'EOF'

# Slow fake build: fresh artifacts + the sentinel appear only after a delay.
build-release-all:
	@sleep 2
	@for p in darwin-arm64 darwin-x86_64 linux-x86_64 linux-arm64; do \
		echo "fresh-$$p" > "target/releases/oak-$$p"; \
		echo "fresh-sig-$$p" > "target/releases/oak-$$p.minisig"; \
	done
	@echo fresh-win > target/releases/oak-windows-x86_64.exe
	@echo fresh-winsig > target/releases/oak-windows-x86_64.exe.minisig
	@echo fresh-mounter > target/releases/OakMount.zip
	@echo fresh-mountersig > target/releases/OakMount.zip.minisig
	@touch target/releases/.build-complete

# The OLD prerequisite-ordered release-all shape, kept ONLY to demonstrate
# the regression this suite detects: -j8 runs the two prerequisites
# concurrently, so staging hits the server while the fake build sleeps.
release-all-oldshape: build-release-all release-publish
EOF
SENTINEL="$JD/target/releases/.build-complete"

# 11a. reproduction: the old shape stages before the build completes.
rm -f "$SENTINEL"
start_stub ok 8471 "$TMP/list-promoted.json" "" "$SENTINEL"
( cd "$JD" && make -j8 release-all-oldshape VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL=http://127.0.0.1:8471 ) >"$TMP/oldshape.out" 2>&1
if grep -q "sentinel=no" "$REC/requests.log" 2>/dev/null; then
  ok "-j ordering: OLD prerequisite shape provably stages before the build completes (reproduction)"
else
  bad "-j ordering: old-shape reproduction did not trigger"
  sed 's/^/    log: /' "$REC/requests.log" >&2 2>/dev/null
fi
stop_stub

# 11b. the recipe-body release-all under -j8: no request may reach the
# server before the build sentinel exists.
rm -f "$SENTINEL"
start_stub ok 8471 "$TMP/list-promoted.json" "" "$SENTINEL"
( cd "$JD" && make -j8 release-all VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL=http://127.0.0.1:8471 ) >"$TMP/newshape.out" 2>&1
rc=$?
stage_n=$(grep -c "POST /api/releases/stage" "$REC/requests.log" 2>/dev/null || echo 0)
if [ "$rc" -eq 0 ] && [ "$stage_n" = "6" ] && grep -q "sentinel=yes" "$REC/requests.log" && ! grep -q "sentinel=no" "$REC/requests.log"; then
  ok "-j ordering: recipe-body release-all serializes build -> publish under -j8 (zero requests before the sentinel)"
else
  bad "-j ordering: -j8 release-all (rc=$rc stage=$stage_n)"
  sed 's/^/    /' "$TMP/newshape.out" >&2
  sed 's/^/    log: /' "$REC/requests.log" >&2 2>/dev/null
fi
stop_stub

# ------------------------------------------- 12. listing auth + capability probe
# Mirrors the amended B2 contract: the listing accepts the admin Bearer key
# from B2 onward; a pre-B2 server 401s the same request. The auth-mode stub
# 401s anything without the exact Bearer header, which both proves the
# clients SEND the key and exercises the 401-as-not-capable path.
ADMIN_TOKEN=stub-admin-token-55671

# 12a. correct key -> staging-capable
start_stub ok 8471 "$TMP/list-promoted.json" "" "" "$ADMIN_TOKEN"
if cap=$(release_listing_probe http://127.0.0.1:8471 "$ADMIN_TOKEN" "$TMP/probe-out.json" 2>/dev/null) && [ "$cap" = "staging-capable" ]; then
  ok "probe: authenticated listing with the admin key -> staging-capable"
else
  bad "probe: staging-capable (got '${cap:-}')"
fi
# 12b. wrong key -> 401 -> pre-b2 (the HOLD refusal path)
if cap=$(release_listing_probe http://127.0.0.1:8471 wrong-key "$TMP/probe-out.json" 2>/dev/null) && [ "$cap" = "pre-b2" ]; then
  ok "probe: 401 (pre-B2 / wrong key) classified as pre-b2, not an error"
else
  bad "probe: pre-b2 classification (got '${cap:-}' rc=$?)"
fi
stop_stub
# 12c. a 200 listing WITHOUT promoted_at is a probe failure (fail closed)
printf 'not json' > "$TMP/bad-list.txt"
start_stub ok 8471 "$TMP/bad-list.txt" "" "" "$ADMIN_TOKEN"
if cap=$(release_listing_probe http://127.0.0.1:8471 "$ADMIN_TOKEN" "$TMP/probe-out.json" 2>/dev/null); then
  bad "probe: 200-without-promoted_at must fail closed (got '$cap')"
else
  ok "probe: 200 without promoted_at fails closed"
fi
stop_stub

# 12d. the make targets send the Bearer key: the full publish flow succeeds
# against the auth-required stub ONLY because every request carries it.
start_stub ok 8471 "$TMP/list-promoted.json" "" "" "$ADMIN_TOKEN"
out=$(make release-publish VERSION="$V" OAK_ADMIN_API_KEY="$ADMIN_TOKEN" OAK_URL=http://127.0.0.1:8471 2>&1); rc=$?
stage_n=$(grep -c "POST /api/releases/stage" "$REC/requests.log" 2>/dev/null || echo 0)
if [ "$rc" -eq 0 ] && [ "$stage_n" = "6" ]; then
  ok "auth: release-publish succeeds against the Bearer-enforcing stub (all requests authenticated)"
else
  bad "auth: release-publish vs auth stub (rc=$rc stage=$stage_n)"; sed 's/^/    /' <<<"$out" >&2
fi
stop_stub
# 12e. and with the WRONG key the same flow fails (server 401s are fatal).
start_stub ok 8471 "$TMP/list-promoted.json" "" "" "$ADMIN_TOKEN"
out=$(make release-publish VERSION="$V" OAK_ADMIN_API_KEY=wrong-key OAK_URL=http://127.0.0.1:8471 2>&1); rc=$?
if [ "$rc" -ne 0 ] && ! grep -q "Promote complete" <<<"$out"; then
  ok "auth: wrong key fails the publish flow closed"
else
  bad "auth: wrong key (rc=$rc)"; sed 's/^/    /' <<<"$out" >&2
fi
stop_stub

# ------------------------------------------- 13. spot-check payload guard
# An empty or unparseable payload must FAIL the spot check — the platform
# loop would otherwise never execute and return silent success.
printf '{"platforms":{}}' > "$TMP/empty-payload.json"
if release_spot_check_sha256 "$V" http://127.0.0.1:9 "$TMP/empty-payload.json" >/dev/null 2>&1; then
  bad "spot check: empty platforms map must fail"
else
  ok "spot check: empty platforms map fails (no silent no-op success)"
fi
printf 'not json' > "$TMP/garbage-payload.json"
if release_spot_check_sha256 "$V" http://127.0.0.1:9 "$TMP/garbage-payload.json" >/dev/null 2>&1; then
  bad "spot check: unparseable payload must fail"
else
  ok "spot check: unparseable payload fails"
fi
jq 'del(.platforms."darwin-mounter")' "$ATTESTED" > "$TMP/five-payload.json"
if release_spot_check_sha256 "$V" http://127.0.0.1:9 "$TMP/five-payload.json" >/dev/null 2>&1; then
  bad "spot check: five-platform payload must fail"
else
  ok "spot check: five-platform payload fails (canonical six required)"
fi
# exact-SET equality, not just count: six made-up slugs must fail, and so
# must five canonical + one extra (still six keys) — a dead OAK_URL proves
# both are rejected before any request.
jq -n '{platforms: ( [range(6)] | map({("fake-slug-\(.)"): {sha256:"a",minisig_sha256:"b"}}) | add )}' > "$TMP/six-wrong.json"
if release_spot_check_sha256 "$V" http://127.0.0.1:9 "$TMP/six-wrong.json" >/dev/null 2>&1; then
  bad "spot check: six wrong-slug keys must fail"
else
  ok "spot check: six made-up slugs fail (exact canonical set required)"
fi
jq 'del(.platforms."darwin-mounter") | .platforms."freebsd-x86_64" = {sha256:"a",minisig_sha256:"b"}' "$ATTESTED" > "$TMP/swap-payload.json"
if release_spot_check_sha256 "$V" http://127.0.0.1:9 "$TMP/swap-payload.json" >/dev/null 2>&1; then
  bad "spot check: five-canonical-plus-one-extra must fail"
else
  ok "spot check: five canonical + one foreign slug fails (exact canonical set required)"
fi

# ------------------------------------- 14. already_promoted attestation adoption
# The server's stage endpoint returns the STORED {sha256, minisig_sha256}
# attestation in its 200 already_promoted response: re-signing identical
# bytes changes the minisig hash, so a promote payload computed from local
# files would CAS-409 against the immutable stored sidecar. The local
# stage/computed-payload path must ADOPT the stored values per platform.
rm -f target/releases/stage-attestations.json target/releases/promote-payload.json
start_stub stage-adopt 8471 "$TMP/list-promoted.json"
out=$(make upload-release VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL=http://127.0.0.1:8471 2>&1); rc=$?
if [ "$rc" -eq 0 ] && [ "$(jq -r '."darwin-arm64".minisig_sha256' target/releases/stage-attestations.json 2>/dev/null)" = "stored-sig-darwin-arm64" ]; then
  ok "adopt: already_promoted stage responses persist the stored attestation"
else
  bad "adopt: stage attestation persistence (rc=$rc)"; sed 's/^/    /' <<<"$out" >&2
fi
stop_stub
start_stub ok 8471 "$TMP/list-promoted.json"
out=$(make promote-release VERSION="$V" OAK_ADMIN_API_KEY=dummy OAK_URL=http://127.0.0.1:8471 2>&1); rc=$?
payload=target/releases/promote-payload.json
local_msha=$(shasum -a 256 target/releases/oak-darwin-arm64.minisig | awk '{print $1}')
got_msha=$(jq -r '.platforms."darwin-arm64".minisig_sha256' "$payload" 2>/dev/null)
if [ "$rc" -eq 0 ] && [ "$got_msha" = "stored-sig-darwin-arm64" ] && [ "$got_msha" != "$local_msha" ] \
  && [ "$(jq -r '.platforms."darwin-mounter".sha256' "$payload")" = "stored-bin-darwin-mounter" ]; then
  ok "adopt: computed promote payload carries the STORED minisig hash, not the local file's"
else
  bad "adopt: payload adoption (rc=$rc got='$got_msha' local='$local_msha')"; sed 's/^/    /' <<<"$out" >&2
fi
stop_stub
rm -f target/releases/stage-attestations.json

echo
echo "release-state tests: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
