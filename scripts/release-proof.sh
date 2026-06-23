#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo_bin="${CARGO:-cargo}"
minisign_bin="${MINISIGN:-minisign}"
require_release_signing="${REQUIRE_RELEASE_SIGNING:-0}"

run() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  "$@"
}

workspace_version() {
  awk '/^\[workspace.package\]/{f=1} f&&/^version *=/{gsub(/[" ]/,"");split($0,a,"=");print a[2];exit}' Cargo.toml
}

core_package_version() {
  awk '/^\[package\]/{f=1} f&&/^version *=/{gsub(/[" ]/,"");split($0,a,"=");print a[2];exit}' core/Cargo.toml
}

workspace_core_dependency_version() {
  sed -n 's/^oak-core *=.*version *= *"\([^"]*\)".*/\1/p' Cargo.toml
}

crates_version_exists() {
  local crate="$1"
  local version="$2"
  command -v curl >/dev/null 2>&1 || return 2
  curl -fsSL -A "oak-release-proof (https://oak.space)" \
    "https://crates.io/api/v1/crates/${crate}/${version}" >/dev/null 2>&1
}

check_versions() {
  local workspace core dep
  workspace="$(workspace_version)"
  core="$(core_package_version)"
  dep="$(workspace_core_dependency_version)"

  if [[ -z "$workspace" || -z "$core" || -z "$dep" ]]; then
    echo "error: could not derive workspace/core/dependency versions from Cargo.toml" >&2
    exit 1
  fi
  if [[ "$core" != "$workspace" ]]; then
    echo "error: core/Cargo.toml version '$core' != workspace version '$workspace'" >&2
    exit 1
  fi
  if [[ "$dep" != "$workspace" ]]; then
    echo "error: workspace oak-core dependency version '$dep' != workspace version '$workspace'" >&2
    exit 1
  fi
  echo "version lockstep OK: ${workspace}"
}

check_signing() {
  if [[ -z "${MINISIGN_SECKEY:-}" ]]; then
    if [[ "$require_release_signing" == "1" ]]; then
      echo "error: MINISIGN_SECKEY is required when REQUIRE_RELEASE_SIGNING=1" >&2
      exit 1
    fi
    echo "warning: MINISIGN_SECKEY not set; signing smoke skipped. Set REQUIRE_RELEASE_SIGNING=1 to make this a hard gate." >&2
    return
  fi

  command -v "$minisign_bin" >/dev/null 2>&1 || {
    echo "error: '$minisign_bin' not found; install minisign or set MINISIGN=<path>" >&2
    exit 1
  }
  [[ -f "$MINISIGN_SECKEY" ]] || {
    echo "error: MINISIGN_SECKEY '$MINISIGN_SECKEY' does not exist" >&2
    exit 1
  }

  mkdir -p target/release-proof
  local smoke="target/release-proof/minisign-smoke.txt"
  local sig="${smoke}.minisig"
  printf 'oak release signing smoke\n' > "$smoke"
  rm -f "$sig"

  if [[ -n "${MINISIGN_PASSWORD:-}" ]]; then
    printf '%s\n' "$MINISIGN_PASSWORD" | "$minisign_bin" -S -s "$MINISIGN_SECKEY" -m "$smoke" -x "$sig"
  else
    if [[ ! -t 0 ]]; then
      echo "error: MINISIGN_PASSWORD is unset and stdin is not interactive; cannot unlock MINISIGN_SECKEY" >&2
      exit 1
    fi
    "$minisign_bin" -S -s "$MINISIGN_SECKEY" -m "$smoke" -x "$sig"
  fi

  [[ -s "$sig" ]] || {
    echo "error: minisign smoke did not create '$sig'" >&2
    exit 1
  }
  echo "minisign signing smoke OK: $sig"
}

check_versions
check_signing

run "$cargo_bin" build --release
run "$cargo_bin" test --workspace
run "$cargo_bin" clippy --workspace --all-targets -- -D warnings
run "$cargo_bin" publish --dry-run --package oakvcs-core

version="$(workspace_version)"
if crates_version_exists "oakvcs-core" "$version"; then
  echo "oakvcs-core ${version} is already on crates.io; running oakvcs-cli publish dry-run."
  run "$cargo_bin" publish --dry-run --package oakvcs-cli
else
  status=$?
  if [[ "$status" == "2" ]]; then
    echo "warning: curl unavailable; cannot check whether oakvcs-core ${version} is on crates.io." >&2
  else
    echo "oakvcs-core ${version} is not on crates.io; using the publish workflow's CLI dry-run proxy."
  fi
  echo "reason: cargo publish --dry-run --package oakvcs-cli resolves oakvcs-core from crates.io, not the local path."
  run "$cargo_bin" build --release --package oakvcs-cli
fi

echo "release proof OK"
