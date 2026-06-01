# Oak CLI — build / install / release.
#
# Run from the workspace root. This repo is a Cargo workspace with two members:
# `core/` (oakvcs-core) and `cli/` (oakvcs-cli, which produces the `oak`
# binary). The CLI depends on `oak-core` via an in-workspace path, so no
# `[patch.crates-io]` is needed — a plain build resolves core from ./core.

CARGO ?= $(shell if command -v cargo >/dev/null 2>&1; then command -v cargo; elif [ -x "$(HOME)/.cargo/bin/cargo" ]; then printf '%s\n' "$(HOME)/.cargo/bin/cargo"; else printf '%s\n' cargo; fi)

# The Linux release binaries are cross-compiled with cargo-zigbuild (Zig's
# bundled clang as the cross-linker, no Docker). GLIBC_VER pins the minimum
# glibc the Linux binaries load against; 2.31 (Ubuntu 20.04) leaves headroom
# for newer hosts.
GLIBC_VER ?= 2.31

# Set LINUX_MOUNT=1 to build the mount-enabled Linux binaries (see
# build-release-linux). --features can't be passed at a virtual-workspace root,
# so the mount build scopes to -p oakvcs-cli; the mount-free build builds the
# whole workspace as before.
LINUX_MOUNT ?=
ifeq ($(LINUX_MOUNT),1)
ZIG_BUILD_ARGS := --package oakvcs-cli --features mount
else
ZIG_BUILD_ARGS :=
endif

# macOS code signing. SHA-1 hash of the "Developer ID Application" cert used to
# sign the darwin binaries (a hash, not the name, because two valid Developer
# ID certs share that name and signing-by-name is ambiguous). Signing is
# skipped with a warning if this identity isn't in the keychain, so
# `build-release-all` still works on machines without the cert (the binaries
# keep their runnable ad-hoc linker signature). Override to sign with a
# different identity, or set empty to force-skip.
SIGN_IDENTITY ?= 981A6F2BB57517E4CA6A4F80CF3D1693F0F5191F

# Release-binary signing (minisign). MINISIGN_SECKEY is the path to the minisign
# secret key that signs EVERY release binary (all platforms, not just macOS).
# Its public counterpart is baked into the CLI as RELEASE_PUBKEY in
# src/commands/upgrade.rs, so `oak upgrade` verifies downloads against a key
# that never lives on the release server. Generate the keypair once with
# `minisign -G` (or `rsign generate`), keep the secret key out of the repo, and
# paste the public key into upgrade.rs. Signing is skipped with a warning when
# MINISIGN_SECKEY is empty, so non-release builds still work — but note an
# UNSIGNED release will be REFUSED by `oak upgrade` (it fails closed).
MINISIGN ?= minisign
MINISIGN_SECKEY ?=
# Optional passphrase for MINISIGN_SECKEY. When set, it's piped to `minisign -S`
# over stdin so signing runs non-interactively in CI (minisign reads the
# password from stdin when it isn't a TTY). Leave empty for an interactive
# prompt locally, or for a passwordless key (`minisign -G -W`).
MINISIGN_PASSWORD ?=

# macOS notarization. Requires a stored notarytool credential profile, created
# once with:
#   xcrun notarytool store-credentials "$(NOTARY_PROFILE)" \
#     --apple-id <you@example.com> --team-id 452XFR864N --password <app-specific-pw>
# (Create the app-specific password at appleid.apple.com → Sign-In and Security
# → App-Specific Passwords.) Not part of release-all: the curl/`oak upgrade`
# install path doesn't set the Gatekeeper quarantine bit, so notarization only
# matters once binaries are distributed via a browser/.dmg/Homebrew cask.
NOTARY_PROFILE ?= oak-notary

# Release version, e.g. v0.94.0 — derived from the workspace package version.
VERSION ?= v$(shell awk '/^\[workspace.package\]/{f=1} f&&/^version *=/{gsub(/[" ]/,"");split($$0,a,"=");print a[2];exit}' Cargo.toml)

.PHONY: build install test fmt lint check ci macos-app \
        build-release-all build-release-macos build-release-linux \
        build-release-linux-mount sign-release notarize-mac \
        upload-release release-all

# ----------------------------------------------------------------------------
# Local build / install
# ----------------------------------------------------------------------------

build:
	$(CARGO) build --release

# Install the `oak` binary to ~/.cargo/bin. Enables the `oak mount` subcommand
# (FSKit on macOS, fuser on Linux; a no-op elsewhere). On macOS, mounting also
# needs the OakFS extension installed — see `make macos-app`.
install:
ifeq ($(shell uname),Darwin)
	$(CARGO) install --path cli --locked --features mount
else ifeq ($(shell uname),Linux)
	$(CARGO) install --path cli --locked --features mount
else
	$(CARGO) install --path cli --locked
endif

test:
	$(CARGO) test

# Build (and optionally install) the macOS OakFS FSKit extension + host app.
# This is what lets `oak mount` work on macOS with no kernel extension. Needs
# Xcode 16+ with the macOS 26 SDK, `xcodegen`, and the
# `com.apple.developer.fskit.fsmodule` entitlement granted to DEVELOPMENT_TEAM
# (set it in macos/OakFS/project.yml). Run `make macos-app INSTALL=1` to also
# copy "Oak Mounter.app" into /Applications.
macos-app:
	@if [ "$$(uname -s)" != "Darwin" ]; then echo "Error: macos-app only builds on macOS."; exit 1; fi
	@command -v xcodegen >/dev/null || { echo "Error: xcodegen not found. Install with: brew install xcodegen"; exit 1; }
	@command -v xcodebuild >/dev/null || { echo "Error: xcodebuild not found (install Xcode)."; exit 1; }
	cd macos/OakFS && xcodegen generate
	cd macos/OakFS && xcodebuild -project OakFS.xcodeproj -scheme OakMounter \
		-configuration Release -derivedDataPath build
	@if [ -n "$(INSTALL)" ]; then \
		echo "Installing Oak Mounter.app to /Applications..."; \
		rm -rf "/Applications/Oak Mounter.app"; \
		cp -R "macos/OakFS/build/Build/Products/Release/Oak Mounter.app" /Applications/; \
		echo "Installed. Open it and enable OakFS in System Settings → General →"; \
		echo "Login Items & Extensions → File System Extensions."; \
	else \
		echo "Built macos/OakFS/build/Build/Products/Release/Oak Mounter.app"; \
		echo "Re-run with INSTALL=1 to copy it to /Applications."; \
	fi

fmt:
	$(CARGO) fmt --all

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

check: fmt lint test

# Non-mutating verification, mirroring the GitHub Actions CI
# (.github/workflows/ci.yml): fmt is checked (not rewritten), and tests run
# under nextest so each test gets its own process — the mount tests share a
# process-global OAK_MOUNTS_ROOT env var and race under `cargo test`'s in-process
# threads, but pass cleanly when isolated. Needs `cargo install cargo-nextest`.
ci:
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) nextest run --workspace

# ----------------------------------------------------------------------------
# Release binaries (mount-free, cross-compiled from one host)
# ----------------------------------------------------------------------------

# Build the distributed release binaries for all four platform/arch targets
# from a single host (no Docker, no remote runners):
#
#   darwin-arm64    Apple Silicon         native `cargo build --target` + codesign
#   darwin-x86_64   Intel Macs            native `cargo build --target` + codesign
#   linux-x86_64    glibc Linux x86_64    cargo-zigbuild (glibc $(GLIBC_VER))
#   linux-arm64     glibc Linux aarch64   cargo-zigbuild (glibc $(GLIBC_VER))
#
# The two macOS arches build with the plain Apple toolchain — the macOS SDK
# cross-compiles between its own arches, so no zig is needed there — and are
# then Developer-ID signed (see SIGN_IDENTITY). The two Linux targets use
# cargo-zigbuild (Zig's bundled clang as the cross-linker). reqwest is on
# rustls + webpki-roots (no OpenSSL) and every unix-only path in src/ is
# #[cfg(unix)]-gated, which is what lets Linux cross-compile cleanly from a Mac.
#
# These are the MOUNT-FREE binaries — the same ones Oak distributes. The mount
# feature does not cross-compile (fuser's build.rs gates its backend on the
# *host* OS), so the mount-enabled linux binary is built natively on a Linux
# host (see build-release-linux-mount) and overwrites its artifact before upload.
#
# build-release-all is the single-host (one Mac) entry point. The GitHub Actions
# release workflow instead calls the per-OS targets below directly, so each
# platform builds on its own native runner — that's what lets the Linux binaries
# be built mount-enabled (LINUX_MOUNT=1), which does not cross-compile from macOS.
build-release-all: build-release-macos build-release-linux sign-release
	@echo "Release binaries built in target/releases/"
	@ls -la target/releases/

# macOS half of the release: build + Developer-ID-sign the two darwin arches.
# Runs on macOS only (the Apple SDK cross-compiles between its own arches, so no
# zig is needed here). Mount-free — a mount-enabled mac binary hard-links
# libfuse.2.dylib and won't launch without macFUSE.
build-release-macos:
	@[ "$$(uname -s)" = "Darwin" ] || { echo "Error: build-release-macos must run on macOS."; exit 1; }
	@echo "Building macOS release binaries..."
	@mkdir -p target/releases
	@for t in aarch64-apple-darwin x86_64-apple-darwin; do \
		rustup target list --installed 2>/dev/null | grep -qx "$$t" || { echo "Error: rust target $$t not installed. Install with: rustup target add $$t"; exit 1; }; \
	done
	@echo "Building darwin-arm64..."
	$(CARGO) build --release --target aarch64-apple-darwin
	cp target/aarch64-apple-darwin/release/oak target/releases/oak-darwin-arm64
	@echo "Building darwin-x86_64..."
	$(CARGO) build --release --target x86_64-apple-darwin
	cp target/x86_64-apple-darwin/release/oak target/releases/oak-darwin-x86_64
	# --- Developer ID code signing ------------------------------------------
	# Hardened runtime (--options runtime) + a trusted timestamp keep the
	# binaries notarization-ready. Skips gracefully (ad-hoc signature retained)
	# when the cert isn't on this machine, so non-release builds still work.
	@if [ -n "$(SIGN_IDENTITY)" ] && security find-identity -v -p codesigning 2>/dev/null | grep -q "$(SIGN_IDENTITY)"; then \
		for b in oak-darwin-arm64 oak-darwin-x86_64; do \
			echo "Signing $$b (Developer ID)..."; \
			codesign --force --timestamp --options runtime --sign "$(SIGN_IDENTITY)" "target/releases/$$b" || exit 1; \
			codesign --verify --strict --verbose=1 "target/releases/$$b" || exit 1; \
		done; \
	else \
		echo "Warning: signing identity '$(SIGN_IDENTITY)' not in keychain — darwin binaries left ad-hoc signed (they run, but are not Developer-ID signed)."; \
	fi

# Linux half of the release: cargo-zigbuild both linux arches (no Docker),
# pinned to glibc $(GLIBC_VER) for broad distro compatibility. reqwest is on
# rustls + webpki-roots (no OpenSSL) and every unix-only path is #[cfg(unix)]-
# gated, which is what lets Linux cross-compile cleanly.
#
# LINUX_MOUNT=1 builds the mount-enabled CLI. On Linux the fuser backend is
# default-features = false: it never link-time-depends on libfuse and execs the
# `fusermount3` setuid helper at runtime, so the binary still RUNS without fuse3
# installed (only `oak mount` itself needs it). Because --features can't be
# passed at a virtual-workspace root, the mount build targets -p oakvcs-cli.
# Run this on a Linux host (zigbuild's host=linux makes the fusermount3 backend
# compile for both linux targets, including the arm64 cross).
build-release-linux:
	@command -v cargo-zigbuild >/dev/null || { echo "Error: cargo-zigbuild not found. Install with: cargo install cargo-zigbuild"; exit 1; }
	@command -v zig >/dev/null || { echo "Error: zig not found. Install with: brew install zig (or your distro's package)"; exit 1; }
	@mkdir -p target/releases
	@for t in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do \
		rustup target list --installed 2>/dev/null | grep -qx "$$t" || { echo "Error: rust target $$t not installed. Install with: rustup target add $$t"; exit 1; }; \
	done
	@if [ "$(LINUX_MOUNT)" = "1" ]; then echo "Linux binaries: MOUNT-ENABLED (oak mount available; needs fuse3 at runtime)."; else echo "Linux binaries: mount-free."; fi
	# `ulimit -n` shell-builtin must be on the same line as each zigbuild:
	# Zig's linker opens many rlibs in parallel and blows past macOS's default
	# 256 fd limit with ProcessFdQuotaExceeded.
	@echo "Building linux-x86_64 (glibc $(GLIBC_VER))..."
	ulimit -n 65536 && $(CARGO) zigbuild --release $(ZIG_BUILD_ARGS) --target x86_64-unknown-linux-gnu.$(GLIBC_VER)
	cp target/x86_64-unknown-linux-gnu/release/oak target/releases/oak-linux-x86_64
	@echo "Building linux-arm64 (glibc $(GLIBC_VER))..."
	ulimit -n 65536 && $(CARGO) zigbuild --release $(ZIG_BUILD_ARGS) --target aarch64-unknown-linux-gnu.$(GLIBC_VER)
	cp target/aarch64-unknown-linux-gnu/release/oak target/releases/oak-linux-arm64

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
	$(CARGO) build --release --package oakvcs-cli --features mount
	cp target/release/oak target/releases/oak-linux-x86_64
	@echo "Mount-enabled linux-x86_64 binary written to target/releases/oak-linux-x86_64"
	@echo "Verify it does not hard-link libfuse:  ldd target/releases/oak-linux-x86_64 | grep -i fuse  (expect no output)"

# minisign-sign every release binary in target/releases/, writing a
# <binary>.minisig sidecar next to each. Run automatically at the end of
# build-release-all; also runnable standalone to re-sign (e.g. after building
# the mount-enabled linux binary natively). No-op with a warning when
# MINISIGN_SECKEY is unset.
sign-release:
	@if [ -z "$(MINISIGN_SECKEY)" ]; then \
		echo "Warning: MINISIGN_SECKEY not set — release binaries are UNSIGNED. 'oak upgrade' will refuse them. Set MINISIGN_SECKEY=<path-to-.key> to sign."; \
	else \
		command -v $(MINISIGN) >/dev/null || { echo "Error: '$(MINISIGN)' not found (try 'brew install minisign') but MINISIGN_SECKEY is set."; exit 1; }; \
		[ -f "$(MINISIGN_SECKEY)" ] || { echo "Error: MINISIGN_SECKEY '$(MINISIGN_SECKEY)' does not exist."; exit 1; }; \
		for b in oak-darwin-arm64 oak-darwin-x86_64 oak-linux-x86_64 oak-linux-arm64; do \
			[ -f "target/releases/$$b" ] || continue; \
			echo "Signing $$b (minisign)..."; \
			if [ -n "$(MINISIGN_PASSWORD)" ]; then \
				printf '%s\n' "$(MINISIGN_PASSWORD)" | $(MINISIGN) -S -s "$(MINISIGN_SECKEY)" -m "target/releases/$$b" -x "target/releases/$$b.minisig" || exit 1; \
			else \
				$(MINISIGN) -S -s "$(MINISIGN_SECKEY)" -m "target/releases/$$b" -x "target/releases/$$b.minisig" || exit 1; \
			fi; \
		done; \
		echo "minisign signatures written (*.minisig)."; \
	fi

# Notarize the signed darwin binaries with Apple's notary service. Run AFTER
# build-release-all (the binaries must already be Developer-ID signed with a
# hardened runtime). Bare CLI binaries can't be stapled (stapling needs a
# .app/.pkg/.dmg), so this registers the ticket with Apple; Gatekeeper checks
# it online if a binary is ever quarantined. See NOTARY_PROFILE above for the
# one-time credential setup.
notarize-mac:
	@command -v xcrun >/dev/null || { echo "Error: xcrun not found (Xcode command line tools required)."; exit 1; }
	@for b in oak-darwin-arm64 oak-darwin-x86_64; do \
		[ -f "target/releases/$$b" ] || { echo "Error: target/releases/$$b not found — run 'make build-release-all' first."; exit 1; }; \
	done
	@echo "Zipping darwin binaries for notarization..."
	@rm -f target/releases/oak-darwin-notarize.zip
	@cd target/releases && zip -q oak-darwin-notarize.zip oak-darwin-arm64 oak-darwin-x86_64
	@echo "Submitting to Apple notary service (profile '$(NOTARY_PROFILE)'; this can take a few minutes)..."
	xcrun notarytool submit target/releases/oak-darwin-notarize.zip --keychain-profile "$(NOTARY_PROFILE)" --wait
	@rm -f target/releases/oak-darwin-notarize.zip
	@echo "Notarization done. (Bare CLI binaries can't be stapled; the ticket is registered with Apple online.)"

# Upload release binaries to the Oak server.
# Usage: make upload-release VERSION=v0.94.0 OAK_URL=https://oakvcs.com OAK_ADMIN_API_KEY=your-key
upload-release:
	@if [ -z "$(VERSION)" ]; then echo "VERSION is required. Usage: make upload-release VERSION=v0.94.0"; exit 1; fi
	@if [ -z "$(OAK_ADMIN_API_KEY)" ]; then echo "OAK_ADMIN_API_KEY is required"; exit 1; fi
	@OAK_URL=$${OAK_URL:-https://oakvcs.com}; \
	for platform in darwin-arm64 darwin-x86_64 linux-x86_64 linux-arm64; do \
		binary="target/releases/oak-$$platform"; \
		if [ -f "$$binary" ]; then \
			echo "Uploading $$platform..."; \
			sig_arg=""; \
			if [ -f "$$binary.minisig" ]; then sig_arg="-F minisig=@$$binary.minisig"; \
			else echo "  (no $$binary.minisig — uploading unsigned; oak upgrade will refuse it)"; fi; \
			curl -X POST "$$OAK_URL/api/releases" \
				-H "Authorization: Bearer $(OAK_ADMIN_API_KEY)" \
				-F "version=$(VERSION)" \
				-F "platform=$$platform" \
				$$sig_arg \
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
