#!/usr/bin/env bash
# Shared release promotion-state predicate — the SINGLE definition used by
# scripts/release-oak-ci.sh (mode
# detection, stage-step fallback, promote confirmation) and available to
# operators locally. Sourced, not executed.
#
# Contract (oakspace changeset B2): a stable release is "promoted" only when
# the /api/releases listing carries EXACTLY the six expected platform rows for
# the version, each exactly once, every one with a valid RFC3339-shaped
# promoted_at string. Anything else that isn't plainly "no promotion yet" —
# malformed JSON, schema-violating rows, wrong promoted_at types, duplicate
# platforms, a mixed staged/promoted set, or a promoted set that isn't exactly
# the expected six — is an INVALID state: callers must fail closed BEFORE any
# mutation, never guess "fresh".

# The exact platform set a promoted stable release must serve.
RELEASE_PLATFORMS_EXPECTED="darwin-arm64 darwin-mounter darwin-x86_64 linux-arm64 linux-x86_64 windows-x86_64"

# The canonical set as a sorted JSON array — the ONE comparison value every
# set-equality check in this script derives from.
release_canonical_platforms_json() {
  printf '%s\n' $RELEASE_PLATFORMS_EXPECTED | jq -R . | jq -sc 'sort'
}

# promoted_at accepts ONLY null (staged) or a nonempty STRING shaped like an
# RFC3339 timestamp: YYYY-MM-DDTHH:MM:SS with optional fractional seconds and
# optional zone (Z or +/-HH[:]MM). Numbers, objects, arrays, booleans, and
# empty strings are INVALID (return 2). jq alone cannot fully validate
# timestamp SEMANTICS (month lengths, leap seconds, real zones) — the regex
# pins the shape, which is the fail-closed bar here; the server owns semantic
# validity.
RELEASE_PROMOTED_AT_RE='^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?(Z|[+-][0-9]{2}:?[0-9]{2})?$'

# release_promotion_state VERSION LIST_JSON_FILE
#
# Classify VERSION's promotion state from a saved /api/releases response body.
# The response ENVELOPE must be a TOP-LEVEL JSON ARRAY of objects — only its
# direct elements are inspected (no recursive descent), so a wrapped shape
# like {"metadata": [...]} or {"releases": [...]} is an invalid envelope.
#   stdout "promoted"   — exactly the six expected platforms, each exactly
#                         once, all with a valid RFC3339-shaped promoted_at
#   stdout "unpromoted" — the version has no rows at all, or rows exist and
#                         NONE are promoted (staging in progress or complete)
#   return 2            — invalid/ambiguous state (details above); the
#                         offending payload/rows are printed to stderr
release_promotion_state() {
  local version="$1" list="$2"

  if ! jq -e . "$list" >/dev/null 2>&1; then
    echo "error: /api/releases response is not valid JSON — failing closed. Raw payload:" >&2
    cat "$list" >&2 || true
    echo >&2
    return 2
  fi

  if ! jq -e 'type == "array"' "$list" >/dev/null; then
    echo "error: /api/releases response envelope is not a top-level array (got $(jq -r 'type' "$list")) — failing closed. Raw payload:" >&2
    cat "$list" >&2
    echo >&2
    return 2
  fi

  if ! jq -e 'all(.[]; type == "object")' "$list" >/dev/null; then
    echo "error: /api/releases response contains non-object elements — failing closed. Raw payload:" >&2
    cat "$list" >&2
    echo >&2
    return 2
  fi

  # EVERY top-level row must satisfy the row schema BEFORE any filtering:
  # version must be a string, platform must be a string, promoted_at must be
  # present and either null or a valid RFC3339-shaped string. A malformed row
  # (missing/renamed/numeric version, {}, bad promoted_at) must fail closed
  # here — a version filter would silently drop it and read the listing as
  # "no matching release", which is fail-open.
  local bad_rows
  bad_rows=$(jq -c --arg re "$RELEASE_PROMOTED_AT_RE" '[.[] | select(
      (
        ((.version? | type) == "string")
        and ((.platform? | type) == "string")
        and has("promoted_at")
        and ((.promoted_at == null) or (((.promoted_at | type) == "string") and (.promoted_at | test($re))))
      ) | not)]' "$list") || {
    echo "error: could not validate /api/releases rows — failing closed. Raw payload:" >&2
    cat "$list" >&2
    echo >&2
    return 2
  }
  if [ "$(jq 'length' <<<"$bad_rows")" -ne 0 ]; then
    echo "error: /api/releases contains row(s) violating the row schema (version: string, platform: string, promoted_at: null | RFC3339 string) — failing closed. Offending rows:" >&2
    jq . <<<"$bad_rows" >&2
    return 2
  fi

  local rows
  if ! rows=$(jq -c --arg v "$version" '[.[] | select(.version == $v)]' "$list"); then
    echo "error: could not extract rows for $version — failing closed. Raw payload:" >&2
    cat "$list" >&2
    echo >&2
    return 2
  fi

  local n
  n=$(jq 'length' <<<"$rows")
  if [ "$n" -eq 0 ]; then
    echo unpromoted
    return 0
  fi

  # (Row schema and promoted_at types were already validated for EVERY row
  # above, before filtering — only version-scoped invariants remain here.)

  # Slug validity: every platform must be one of the canonical six, compared
  # as JSON values (no textual normalization — an empty string, whitespace
  # padding, or an unknown slug is rejected explicitly and printed quoted).
  local canon
  canon=$(release_canonical_platforms_json)
  if ! jq -e --argjson canon "$canon" 'all(.[]; .platform as $p | ($canon | index($p)) != null)' <<<"$rows" >/dev/null; then
    echo "error: rows for $version carry platform slug(s) outside the canonical set — failing closed. Offending slugs (quoted):" >&2
    jq -c --argjson canon "$canon" '[.[].platform] | map(select(. as $p | ($canon | index($p)) == null))' <<<"$rows" >&2
    return 2
  fi

  if ! jq -e '[.[].platform] | length == (unique | length)' <<<"$rows" >/dev/null; then
    echo "error: duplicate platform rows for $version — failing closed. Rows found:" >&2
    jq -r '.[] | "  \(.platform) promoted_at=\(.promoted_at)"' <<<"$rows" >&2
    return 2
  fi

  local promoted_n
  promoted_n=$(jq '[.[] | select(.promoted_at != null)] | length' <<<"$rows")

  if [ "$promoted_n" -eq 0 ]; then
    echo unpromoted
    return 0
  fi

  if [ "$promoted_n" -ne "$n" ]; then
    echo "error: $version is in a MIXED staged/promoted state ($promoted_n of $n rows promoted) — failing closed. Rows found:" >&2
    jq -r '.[] | "  \(.platform) promoted_at=\(.promoted_at)"' <<<"$rows" >&2
    return 2
  fi

  # Every row promoted: the platform set must be EXACTLY the expected six.
  # Compared as sorted JSON arrays in jq — never as whitespace-joined text,
  # which would normalize away padding/empty slugs (those are already
  # rejected above, but the comparison must not rely on that).
  if ! jq -e --argjson canon "$canon" '([.[].platform] | sort | unique) == $canon' <<<"$rows" >/dev/null; then
    echo "error: $version's promoted platform set is not exactly the expected six — failing closed." >&2
    echo "  expected: $canon" >&2
    jq -c '[.[].platform] | sort' <<<"$rows" | sed 's/^/  found:    /' >&2
    jq -c --argjson canon "$canon" '$canon - [.[].platform]' <<<"$rows" | sed 's/^/  missing:  /' >&2
    return 2
  fi

  echo promoted
}

# release_decide_mode PROMO_STATE
#
# The release-mode decision, factored out so it is directly testable.
# PROMO_STATE is promoted|unpromoted, as printed by release_promotion_state (an
# invalid predicate result never reaches this function; callers fail closed on
# it first). oak.space's promotion is the only published state a release has:
# the notes are docs/release-X.Y.Z.md in the repo.
#   stdout "fresh"             — not promoted yet: build, stage, verify, promote
#   stdout "already-published" — promoted: confirm, rebuild nothing
#   return 3 — an unknown state (reason on stderr); callers fail closed.
release_decide_mode() {
  case "$1" in
    unpromoted) echo fresh ;;
    promoted) echo already-published ;;
    *)
      echo "error: unrecognized promotion state '$1'" >&2
      return 3 ;;
  esac
}

# release_spot_check_sha256 VERSION BASE PAYLOAD_FILE
#
# Post-promote defense-in-depth, factored out for testability.
# NOT a re-verification: staged bytes are content-addressed and immutable
# server-side, and the full byte comparison already ran pre-promote. This
# cheaply re-GETs each attested platform's /sha256 endpoint (no downloads)
# and requires it to equal the attested payload hash — closing the gap
# between "the rows say promoted" and "the promoted rows serve the hashes
# this run verified". Returns 1 on any missing endpoint or mismatch.
release_spot_check_sha256() {
  local version="$1" base="$2" payload="$3" platform want got canon keys
  # Guard: an empty or unparseable payload would make the loop below a
  # silent no-op success, and ANY six keys would loop happily over the
  # wrong slugs. The attested payload's platform keys must be EXACTLY the
  # canonical six — same set, same comparison source
  # (release_canonical_platforms_json) as the promote validation.
  canon=$(release_canonical_platforms_json)
  keys=$(jq -c '.platforms | keys | sort' "$payload" 2>/dev/null) || keys=""
  if [ -z "$keys" ] || [ "$keys" != "$canon" ]; then
    echo "error: spot-check payload platform keys must be exactly the canonical six $canon; found '${keys:-unparseable}' — refusing to pass on an empty/short/wrong-set/invalid payload" >&2
    return 1
  fi
  while read -r platform; do
    want=$(jq -r --arg p "$platform" '.platforms[$p].sha256' "$payload")
    if ! got=$(curl --fail-with-body -sS "$base/api/releases/$version/$platform/sha256"); then
      echo "error: $base/api/releases/$version/$platform/sha256 is not being served after promote" >&2
      return 1
    fi
    if [ "$(printf '%s' "$got" | tr -d '[:space:]')" != "$want" ]; then
      echo "error: post-promote spot check: $platform /sha256 publishes '$got' but the attested payload hash is $want" >&2
      return 1
    fi
    echo "spot check OK: $platform /sha256 == attested $want"
  done < <(jq -r '.platforms | keys[]' "$payload")
}

# release_listing_probe BASE ADMIN_KEY OUT_FILE
#
# Probe the authenticated release listing with the ADMIN Bearer key and
# classify the server's staging capability. Contract note: pre-B2 servers
# only accept a logged-in user session on GET /api/releases — the admin key
# is consumed by the write endpoints and yields no user, so the same request
# 401s there. Changeset B2 amends the listing to also accept the admin key
# (read-only, same require_admin_key gate). Therefore:
#   stdout "staging-capable" — HTTP 200 AND rows carry promoted_at (B2+)
#   stdout "pre-b2"          — HTTP 401 (server not staging-capable yet;
#                              callers must refuse dispatch — the HOLD)
#   return 2                 — anything else (code + body logged): fail closed
release_listing_probe() {
  local base="$1" key="$2" out="$3" code
  code=$(curl -sS -o "$out" -w '%{http_code}' -H "Authorization: Bearer $key" "$base/api/releases")
  case "$code" in
    200)
      if grep -q '"promoted_at"' "$out"; then
        echo staging-capable
      else
        echo "error: authenticated listing returned 200 but rows carry no promoted_at — not a staging-capable server shape. Body:" >&2
        cat "$out" >&2
        echo >&2
        return 2
      fi ;;
    401)
      echo pre-b2 ;;
    *)
      echo "error: listing probe got HTTP $code (expected exactly 200 = staging-capable or 401 = pre-B2). Body:" >&2
      cat "$out" >&2
      echo >&2
      return 2 ;;
  esac
}
