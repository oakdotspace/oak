# Oak CLI — build / install / release.
#
# This repo IS the `oak-cli` package (the repo root), producing the `oak`
# binary. It depends on `oak-core`; until that crate is published, a
# `[patch.crates-io]` in Cargo.toml resolves it to a sibling ../oak-core
# checkout, so a sibling clone of oak-core must be present to build locally.

CARGO ?= $(shell if command -v cargo >/dev/null 2>&1; then command -v cargo; elif [ -x "$(HOME)/.cargo/bin/cargo" ]; then printf '%s\n' "$(HOME)/.cargo/bin/cargo"; else printf '%s\n' cargo; fi)

# linux-x86_64 is cross-compiled from macOS with cargo-zigbuild (Zig's
# bundled clang as the cross-linker, no Docker). GLIBC_VER pins the minimum
# glibc the binary loads against; 2.31 (Ubuntu 20.04) leaves headroom for
# newer hosts.
LINUX_TARGET ?= x86_64-unknown-linux-gnu
GLIBC_VER ?= 2.31

# Release version, e.g. v0.94.0 — derived from the workspace package version.
VERSION ?= v$(shell awk '/^\[workspace.package\]/{f=1} f&&/^version *=/{gsub(/[" ]/,"");split($$0,a,"=");print a[2];exit}' Cargo.toml)

.PHONY: build install test fmt lint check \
        build-release-all build-release-linux-mount upload-release release-all

# ----------------------------------------------------------------------------
# Local build / install
# ----------------------------------------------------------------------------

build:
	$(CARGO) build --release

# Install the `oak` binary to ~/.cargo/bin. Enables the FUSE-backed
# `oak mount` subcommand on macOS/Linux (the feature is a no-op elsewhere).
install:
ifeq ($(shell uname),Darwin)
	$(CARGO) install --path . --locked --features mount
else ifeq ($(shell uname),Linux)
	$(CARGO) install --path . --locked --features mount
else
	$(CARGO) install --path . --locked
endif

test:
	$(CARGO) test

fmt:
	$(CARGO) fmt --all

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

check: fmt lint test

# ----------------------------------------------------------------------------
# Release binaries (darwin-arm64 + linux-x86_64)
# ----------------------------------------------------------------------------

# Build release binaries for darwin-arm64 and linux-x86_64.
#
# darwin-arm64 covers Apple Silicon (and Intel Macs via Rosetta 2).
# linux-x86_64 is built with cargo-zigbuild (no Docker). Set GLIBC_VER to
# control the minimum glibc the binary loads against.
build-release-all:
	@echo "Building darwin-arm64..."
	@mkdir -p target/releases
	$(CARGO) build --release --target aarch64-apple-darwin --package oak-cli
	cp target/aarch64-apple-darwin/release/oak target/releases/oak-darwin-arm64
	@echo "Building linux-x86_64 (glibc $(GLIBC_VER))..."
	@command -v cargo-zigbuild >/dev/null || { echo "Error: cargo-zigbuild not found. Install with: cargo install cargo-zigbuild"; exit 1; }
	@command -v zig >/dev/null || { echo "Error: zig not found. Install with: brew install zig"; exit 1; }
	@rustup target list --installed 2>/dev/null | grep -qx "$(LINUX_TARGET)" || { echo "Error: rust target $(LINUX_TARGET) not installed. Install with: rustup target add $(LINUX_TARGET)"; exit 1; }
	# `ulimit -n` shell-builtin must be on the same line as the build: Zig's
	# linker opens many rlibs in parallel and blows past macOS's default 256
	# fd limit with ProcessFdQuotaExceeded.
	#
	# NOTE: this is the mount-FREE linux binary. The mount feature cannot be
	# cross-compiled: fuser's build.rs gates its backend on the *host* OS
	# (cfg!(target_os), evaluated for the macOS build host) rather than the
	# --target, so both the pure-rust and libfuse paths fail when built from
	# macOS. The mount-enabled linux binary is built natively on a Linux host
	# via `make build-release-linux-mount` (below), which overwrites this same
	# artifact path before upload.
	ulimit -n 65536 && $(CARGO) zigbuild --release --target $(LINUX_TARGET).$(GLIBC_VER) --package oak-cli
	cp target/$(LINUX_TARGET)/release/oak target/releases/oak-linux-x86_64
	@echo "Release binaries built in target/releases/"
	@ls -la target/releases/

# Build the mount-enabled linux-x86_64 binary. MUST run on a Linux host
# (native build) — the mount feature does not cross-compile from macOS. The
# linux fuser backend uses default-features = false (see Cargo.toml), so the
# binary does NOT link libfuse: it mounts by exec'ing the `fusermount3` setuid
# helper at runtime. The host running `oak mount` therefore needs the `fuse3`
# package installed and access to /dev/fuse. Writes the same
# target/releases/oak-linux-x86_64 path that upload-release reads.
build-release-linux-mount:
	@if [ "$$(uname -s)" != "Linux" ]; then echo "Error: build-release-linux-mount must run on a Linux host (mount cannot cross-compile from $$(uname -s))."; exit 1; fi
	@command -v fusermount3 >/dev/null || echo "Warning: fusermount3 not found on this build host. The binary will still build (pure-rust backend, no link-time libfuse), but 'oak mount' needs the 'fuse3' package at runtime."
	@mkdir -p target/releases
	$(CARGO) build --release --package oak-cli --features mount
	cp target/release/oak target/releases/oak-linux-x86_64
	@echo "Mount-enabled linux-x86_64 binary written to target/releases/oak-linux-x86_64"
	@echo "Verify it does not hard-link libfuse:  ldd target/releases/oak-linux-x86_64 | grep -i fuse  (expect no output)"

# Upload release binaries to the Oak server.
# Usage: make upload-release VERSION=v0.94.0 OAK_URL=https://oakvcs.com OAK_ADMIN_API_KEY=your-key
upload-release:
	@if [ -z "$(VERSION)" ]; then echo "VERSION is required. Usage: make upload-release VERSION=v0.94.0"; exit 1; fi
	@if [ -z "$(OAK_ADMIN_API_KEY)" ]; then echo "OAK_ADMIN_API_KEY is required"; exit 1; fi
	@OAK_URL=$${OAK_URL:-https://oakvcs.com}; \
	for platform in darwin-arm64 linux-x86_64; do \
		binary="target/releases/oak-$$platform"; \
		if [ -f "$$binary" ]; then \
			echo "Uploading $$platform..."; \
			curl -X POST "$$OAK_URL/api/releases" \
				-H "Authorization: Bearer $(OAK_ADMIN_API_KEY)" \
				-F "version=$(VERSION)" \
				-F "platform=$$platform" \
				-F "binary=@$$binary"; \
			echo ""; \
		else \
			echo "Error: $$binary not found"; \
			exit 1; \
		fi; \
	done
	@echo "Upload complete!"

# Build and upload everything.
# Usage: make release-all VERSION=v0.94.0 OAK_ADMIN_API_KEY=your-key
release-all: build-release-all upload-release
