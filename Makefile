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

# macOS code signing. SHA-1 hash of the "Developer ID Application" cert used to
# sign the darwin binaries (a hash, not the name, because two valid Developer
# ID certs share that name and signing-by-name is ambiguous). Signing is
# skipped with a warning if this identity isn't in the keychain, so
# `build-release-all` still works on machines without the cert (the binaries
# keep their runnable ad-hoc linker signature). Override to sign with a
# different identity, or set empty to force-skip.
SIGN_IDENTITY ?= 981A6F2BB57517E4CA6A4F80CF3D1693F0F5191F

# Release-artifact signing (minisign). MINISIGN_SECKEY is the path to the minisign
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
        release-proof \
        build-release-all build-release-macos build-release-linux \
        build-release-windows \
        sign-release notarize-mac \
        upload-release release-all

# ----------------------------------------------------------------------------
# Local build / install
# ----------------------------------------------------------------------------

build:
	$(CARGO) build --release

# Install the `oak` binary to ~/.cargo/bin. The `oak mount` subcommand is
# always built in (FSKit on macOS, fuser on Linux, ProjFS on Windows). On
# macOS, mounting also needs the OakFS extension installed — see `make macos-app`.
install:
	$(CARGO) install --path cli --locked

test:
	$(CARGO) test

# Build (and optionally install) the macOS OakFS FSKit extension + host app.
# This is what lets `oak mount` work on macOS with no kernel extension. Needs
# Xcode 16+ with the macOS 26 SDK, `xcodegen`, and the
# `com.apple.developer.fskit.fsmodule` entitlement granted to DEVELOPMENT_TEAM
# (set it in macos/OakFS/project.yml). Run `make macos-app INSTALL=1` to also
# copy "Oak Mount.app" into /Applications.
macos-app:
	@if [ "$$(uname -s)" != "Darwin" ]; then echo "Error: macos-app only builds on macOS."; exit 1; fi
	@command -v xcodegen >/dev/null || { echo "Error: xcodegen not found. Install with: brew install xcodegen"; exit 1; }
	@command -v xcodebuild >/dev/null || { echo "Error: xcodebuild not found (install Xcode)."; exit 1; }
	cd macos/OakFS && xcodegen generate
	cd macos/OakFS && xcodebuild -project OakFS.xcodeproj -scheme OakMounter \
		-configuration Release -derivedDataPath build
	@if [ -n "$(INSTALL)" ]; then \
		echo "Installing Oak Mount.app to /Applications..."; \
		rm -rf "/Applications/Oak Mount.app"; \
		cp -R "macos/OakFS/build/Build/Products/Release/Oak Mount.app" /Applications/; \
		echo "Installed. Open it and enable OakFS in System Settings → General →"; \
		echo "Login Items & Extensions → File System Extensions."; \
	else \
		echo "Built macos/OakFS/build/Build/Products/Release/Oak Mount.app"; \
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

# Non-mutating launch/release readiness proof. This mirrors the source checks
# and crates.io dry-run semantics in .github/workflows/publish-crates.yml:
# oakvcs-core can be dry-run-published locally, but oakvcs-cli can only be
# dry-run-published after that same core version is visible on crates.io.
release-proof:
	@CARGO="$(CARGO)" MINISIGN="$(MINISIGN)" MINISIGN_SECKEY="$(MINISIGN_SECKEY)" MINISIGN_PASSWORD="$(MINISIGN_PASSWORD)" REQUIRE_RELEASE_SIGNING="$(REQUIRE_RELEASE_SIGNING)" scripts/release-proof.sh

# ----------------------------------------------------------------------------
# Release binaries
# ----------------------------------------------------------------------------

# Build the distributed release binaries for all four platform/arch targets:
#
#   darwin-arm64    Apple Silicon         native `cargo build --target` + codesign
#   darwin-x86_64   Intel Macs            native `cargo build --target` + codesign
#   linux-x86_64    glibc Linux x86_64    cargo-zigbuild (glibc $(GLIBC_VER))
#   linux-arm64     glibc Linux aarch64   cargo-zigbuild (glibc $(GLIBC_VER))
#
# The two macOS arches build with the plain Apple toolchain — the macOS SDK
# cross-compiles between its own arches, so no zig is needed there — and are
# then Developer-ID signed (see SIGN_IDENTITY). The two Linux arches build with
# cargo-zigbuild (Zig's bundled clang as the cross-linker), arm64 cross-built
# from an x86_64 host.
#
# NOT a single-host target: the Linux mount backend (fuser, always built)
# refuses to compile from a non-Linux host — its build.rs panics with "Building
# without libfuse is only supported on Linux" — so the darwin and linux halves
# must each run on their own native host. build-release-macos must run on macOS;
# build-release-linux must run on Linux. The GitHub Actions release workflow
# (.github/workflows/release.yml) does exactly this, one job per OS.
build-release-all: build-release-macos build-release-linux sign-release
	@echo "Release binaries built in target/releases/"
	@ls -la target/releases/

# macOS half of the release: build + Developer-ID-sign the two darwin arches.
# Runs on macOS only (the Apple SDK cross-compiles between its own arches, so no
# zig is needed here). The macOS `oak mount` backend is Apple FSKit — no
# libfuse, no kernel extension — so these binaries are self-contained.
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
# gated.
#
# MUST run on a Linux host. The Linux `oak mount` backend (fuser, always built)
# is default-features = false: it never link-time-depends on libfuse and execs
# the `fusermount3` setuid helper at runtime, so the shipped binary still RUNS
# without fuse3 installed (only `oak mount` itself needs it). But fuser's
# build.rs only supports that libfuse-free build on a Linux host, so it cannot
# cross-compile from macOS. From a Linux host zigbuild still builds both arches
# (arm64 is a zig cross-link); fuser compiles for both because the host is Linux.
build-release-linux:
	@if [ "$$(uname -s)" != "Linux" ]; then echo "Error: build-release-linux must run on a Linux host (the mount backend, fuser, cannot cross-compile from $$(uname -s))."; exit 1; fi
	@command -v cargo-zigbuild >/dev/null || { echo "Error: cargo-zigbuild not found. Install with: cargo install cargo-zigbuild"; exit 1; }
	@command -v zig >/dev/null || { echo "Error: zig not found. Install with: brew install zig (or your distro's package)"; exit 1; }
	@command -v fusermount3 >/dev/null || echo "Warning: fusermount3 not found on this build host. The binary still builds (pure-rust backend, no link-time libfuse), but 'oak mount' needs the 'fuse3' package at runtime."
	@mkdir -p target/releases
	@for t in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do \
		rustup target list --installed 2>/dev/null | grep -qx "$$t" || { echo "Error: rust target $$t not installed. Install with: rustup target add $$t"; exit 1; }; \
	done
	# `ulimit -n` shell-builtin must be on the same line as each zigbuild:
	# Zig's linker opens many rlibs in parallel and blows past the default
	# fd limit with ProcessFdQuotaExceeded.
	@echo "Building linux-x86_64 (glibc $(GLIBC_VER))..."
	ulimit -n 65536 && $(CARGO) zigbuild --release --package oakvcs-cli --target x86_64-unknown-linux-gnu.$(GLIBC_VER)
	cp target/x86_64-unknown-linux-gnu/release/oak target/releases/oak-linux-x86_64
	@echo "Building linux-arm64 (glibc $(GLIBC_VER))..."
	ulimit -n 65536 && $(CARGO) zigbuild --release --package oakvcs-cli --target aarch64-unknown-linux-gnu.$(GLIBC_VER)
	cp target/aarch64-unknown-linux-gnu/release/oak target/releases/oak-linux-arm64
	@echo "Verify the binaries do not hard-link libfuse:  ldd target/releases/oak-linux-x86_64 | grep -i fuse  (expect no output)"

# Windows half of the release: the x86_64 windows-msvc binary. The `oak mount`
# backend here is ProjFS (the `windows` crate's bindings — no FUSE, no external
# helper), so the binary is self-contained; the user enables the "Windows
# Projected File System" optional feature once per machine (see the README and
# cli/src/commands/mount/projfs_fs.rs).
#
# MUST run on a Windows host with the MSVC toolchain (the ProjFS bindings link
# against Win32, so this cannot cross-compile from macOS/Linux). reqwest is on
# rustls + the `ring` crypto provider (NOT aws-lc-rs), so no NASM/CMake/C
# toolchain is needed — only the Rust + MSVC linker the runner already has.
# Invoked through Git Bash on the GitHub windows runner (shell: bash).
build-release-windows:
	@[ "$$(uname -s 2>/dev/null | cut -c1-5)" != "Linux" ] && [ "$$(uname -s 2>/dev/null)" != "Darwin" ] || { echo "Error: build-release-windows must run on a Windows host (ProjFS bindings cannot cross-compile)."; exit 1; }
	@echo "Building windows-x86_64..."
	@mkdir -p target/releases
	@rustup target list --installed 2>/dev/null | grep -qx "x86_64-pc-windows-msvc" || { echo "Error: rust target x86_64-pc-windows-msvc not installed. Install with: rustup target add x86_64-pc-windows-msvc"; exit 1; }
	$(CARGO) build --release --package oakvcs-cli --target x86_64-pc-windows-msvc
	cp target/x86_64-pc-windows-msvc/release/oak.exe target/releases/oak-windows-x86_64.exe

# minisign-sign every release artifact in target/releases/ that clients verify
# directly, writing a <artifact>.minisig sidecar next to each. Run automatically
# at the end of build-release-all; also runnable standalone to re-sign. No-op
# with a warning when MINISIGN_SECKEY is unset.
sign-release:
	@if [ -z "$(MINISIGN_SECKEY)" ]; then \
		echo "Warning: MINISIGN_SECKEY not set — release artifacts are UNSIGNED. 'oak upgrade' and first-run 'oak mount' installs will refuse them. Set MINISIGN_SECKEY=<path-to-.key> to sign."; \
	else \
		command -v $(MINISIGN) >/dev/null || { echo "Error: '$(MINISIGN)' not found (try 'brew install minisign') but MINISIGN_SECKEY is set."; exit 1; }; \
		[ -f "$(MINISIGN_SECKEY)" ] || { echo "Error: MINISIGN_SECKEY '$(MINISIGN_SECKEY)' does not exist."; exit 1; }; \
		for artifact in oak-darwin-arm64 oak-darwin-x86_64 oak-linux-x86_64 oak-linux-arm64 oak-windows-x86_64.exe OakMount.zip; do \
			[ -f "target/releases/$$artifact" ] || continue; \
			echo "Signing $$artifact (minisign)..."; \
			if [ -n "$(MINISIGN_PASSWORD)" ]; then \
				printf '%s\n' "$(MINISIGN_PASSWORD)" | $(MINISIGN) -S -s "$(MINISIGN_SECKEY)" -m "target/releases/$$artifact" -x "target/releases/$$artifact.minisig" || exit 1; \
			else \
				$(MINISIGN) -S -s "$(MINISIGN_SECKEY)" -m "target/releases/$$artifact" -x "target/releases/$$artifact.minisig" || exit 1; \
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

# Upload signed release artifacts to the Oak server.
# Usage: make upload-release VERSION=v0.94.0 OAK_URL=https://oak.space OAK_ADMIN_API_KEY=your-key
# Set REQUIRE_MOUNTER=1 to fail (instead of warn) when target/releases/OakMount.zip
# is absent — CI passes it when the macOS app job actually built the zip.
upload-release:
	@if [ -z "$(VERSION)" ]; then echo "VERSION is required. Usage: make upload-release VERSION=v0.94.0"; exit 1; fi
	@if [ -z "$(OAK_ADMIN_API_KEY)" ]; then echo "OAK_ADMIN_API_KEY is required"; exit 1; fi
	@OAK_URL=$${OAK_URL:-https://oak.space}; \
	for platform in darwin-arm64 darwin-x86_64 linux-x86_64 linux-arm64 windows-x86_64; do \
		binary="target/releases/oak-$$platform"; \
		case "$$platform" in windows-*) binary="$$binary.exe";; esac; \
		if [ -f "$$binary" ]; then \
			echo "Uploading $$platform..."; \
			sig_arg=""; \
			if [ -f "$$binary.minisig" ]; then sig_arg="-F minisig=@$$binary.minisig"; \
			else echo "Error: $$binary.minisig not found — 'oak upgrade' will refuse unsigned release binaries."; exit 1; fi; \
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
	@OAK_URL=$${OAK_URL:-https://oak.space}; \
	mounter="target/releases/OakMount.zip"; \
	if [ -f "$$mounter" ]; then \
		echo "Uploading darwin-mounter (Oak Mount.app)..."; \
		sig_arg=""; \
		if [ -f "$$mounter.minisig" ]; then sig_arg="-F minisig=@$$mounter.minisig"; \
		else echo "Error: $$mounter.minisig not found — first-run 'oak mount' installs will refuse unsigned app zips."; exit 1; fi; \
		curl -X POST "$$OAK_URL/api/releases" \
			-H "Authorization: Bearer $(OAK_ADMIN_API_KEY)" \
			-F "version=$(VERSION)" \
			-F "platform=darwin-mounter" \
			$$sig_arg \
			-F "binary=@$$mounter"; \
		echo ""; \
	elif [ -n "$(REQUIRE_MOUNTER)" ]; then \
		echo "::error::target/releases/OakMount.zip is missing but the app build succeeded — refusing to publish a release without the darwin-mounter asset ('oak mount' first-run install and install.sh depend on it)"; \
		exit 1; \
	else \
		echo "::warning::no target/releases/OakMount.zip — skipping the darwin-mounter upload; 'oak mount' first-run install and install.sh won't find the Oak Mount app for $(VERSION)"; \
	fi
	@echo "Upload complete!"

# Build and upload everything.
# Usage: make release-all VERSION=v0.94.0 OAK_ADMIN_API_KEY=your-key
release-all: build-release-all upload-release
