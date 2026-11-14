#!/usr/bin/env bash
# Install one native-CI archive tool only after its download is complete.
set -euo pipefail

if [ "$#" -lt 3 ] || [ "$#" -gt 4 ]; then
    echo "usage: $0 cargo-nextest|cargo-llvm-cov URL INSTALL_DIR [SHA256]" >&2
    exit 2
fi
tool=$1
url=$2
install_dir=$3
expected_digest=${4:-}
if [ -n "$expected_digest" ] && ! [[ "$expected_digest" =~ ^[0-9a-f]{64}$ ]]; then
    echo "expected SHA256 must be 64 lowercase hex digits" >&2
    exit 2
fi
case "$tool" in
    cargo-nextest|cargo-llvm-cov) ;;
    *) echo "unsupported CI tool: $tool" >&2; exit 2 ;;
esac
mkdir -p "$install_dir"
# Stage on the destination filesystem so publication is one rename.
work_dir=$(mktemp -d "$install_dir/.oak-ci-$tool.XXXXXX")
trap 'rm -rf "$work_dir"' EXIT
trap 'exit 1' HUP INT TERM

# Four attempts at most, with finite connection/transfer/retry budgets. A file
# output lets curl discard an incomplete attempt before retrying (unlike a pipe).
curl --fail --location --silent --show-error \
    --retry 3 --retry-all-errors --retry-delay 1 --retry-max-time 120 \
    --connect-timeout 10 --max-time 60 \
    --header 'Accept: application/octet-stream' \
    --output "$work_dir/archive.tar.gz" -- "$url"
if [ -n "$expected_digest" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
        actual_digest=$(sha256sum "$work_dir/archive.tar.gz")
    else
        actual_digest=$(shasum -a 256 "$work_dir/archive.tar.gz")
    fi
    if [ "${actual_digest%% *}" != "$expected_digest" ]; then
        echo "archive SHA256 mismatch for $tool" >&2
        exit 1
    fi
fi
# A successful HTTP response can still contain a truncated or invalid archive.
# Validate before extraction, then extract only the expected executable.
gzip -t "$work_dir/archive.tar.gz"
mkdir "$work_dir/stage"
tar -xzf "$work_dir/archive.tar.gz" -C "$work_dir/stage" -- "$tool"
test -f "$work_dir/stage/$tool"
test ! -L "$work_dir/stage/$tool"
test -s "$work_dir/stage/$tool"
test -x "$work_dir/stage/$tool"
test ! -d "$install_dir/$tool"
mv -f "$work_dir/stage/$tool" "$install_dir/$tool"
