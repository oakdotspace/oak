#!/usr/bin/env bash
# Shared workflow entry point; also runs in exported checkouts without .oak/.
set -euo pipefail
if [ "$#" -lt 1 ] || [ "$#" -gt 4 ]; then
  echo "usage: $0 cargo-nextest|cargo-llvm-cov [cargo-directory [archive-url [sha256]]]" >&2
  exit 2
fi
tool=$1
cargo_dir=${2:-"$HOME/.cargo"}
digest=
case "$tool" in
  cargo-nextest) url=https://get.nexte.st/latest/linux ;;
  cargo-llvm-cov)
    # Official v0.9.1 x86_64 Linux asset. The release-page download route can
    # return 504 while this official API asset route remains available.
    url=https://api.github.com/repos/taiki-e/cargo-llvm-cov/releases/assets/547403171
    digest=b3f68e625481fed9b16444174f3fa5ebcdbde4a1878803a35eabe2dcefcdc41a
    ;;
  *) echo "unsupported CI tool: $tool" >&2; exit 2 ;;
esac
url=${3:-$url}
digest=${4:-$digest}
. "$cargo_dir/env"
if [ "$tool" = cargo-llvm-cov ]; then
  rustup component add llvm-tools-preview
fi
if ! command -v "$tool" >/dev/null 2>&1; then
  bash "$(dirname "$0")/install-ci-tool.sh" "$tool" "$url" "$cargo_dir/bin" "$digest"
fi
