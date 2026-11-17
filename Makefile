# Oak CLI — build / install / release.
#
# Run from the workspace root. This repo is a Cargo workspace with two members:
# `core/` (oakvcs-core) and `cli/` (oakvcs-cli, which produces the `oak` binary).
# The CLI depends on `oak-core` via an in-workspace path, so no
# `[patch.crates-io]` is needed — a plain build resolves core from ./core.

CARGO ?= $(shell if command -v cargo >/dev/null 2>&1; then command -v cargo; elif [ -x "$(HOME)/.cargo/bin/cargo" ]; then printf '%s\n' "$(HOME)/.cargo/bin/cargo"; else printf '%s\n' cargo; fi)

# The Linux release binaries are cross-compiled with cargo-zigbuild (Zig's
# bundled clang as the cross-linker, no Docker). GLIBC_VER pins the minimum
# glibc the Linux binaries load against; 2.31 (Ubuntu 20.04) leaves headroom
# for newer hosts.
GLIBC_VER ?= 2.31

# macOS code signing. SHA-1 hash of the "Developer ID Application" cert used to
# sign the darwin binaries (a hash, not the name, because two valid Developer
# ID certs share that name and signing-by-name is ambiguous).
#
# NO DEFAULT — deliberately. A hardcoded default here went stale when the
# certificate rotated: with the MACOS_SIGN_IDENTITY secret unset, the old hash
# matched nothing in the keychain, the recipe's "identity in keychain? sign :
# warn" shape silently took the ad-hoc path, and a full multi-platform release
# run then failed LATE on the post-build signature assertion. The rules now
# are:
#   * empty (the default): Developer-ID signing is SKIPPED with a warning and
#     the binaries keep their runnable ad-hoc linker signature — so plain
#     `make build-release-macos` still works on machines without the cert.
#   * non-empty: the identity MUST be a valid codesigning identity in the
#     keychain, or the build FAILS. There is no silent ad-hoc fallback — that
#     silence was the incident.
# CI derives the value from the imported P12 rather than configuring it
# (scripts/derive-sign-identity.sh over `security find-identity` output — see
# scripts/release-oak-ci.sh), so it can never drift from the actual
# certificate. For a local signed build, pass it explicitly:
#   make build-release-macos SIGN_IDENTITY=<sha1>
# and find your <sha1> with:
#   security find-identity -v -p codesigning
SIGN_IDENTITY ?=

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
# Optional trusted comment for every signature (`minisign -t`); empty keeps
# minisign's default. Nightly builds put their version and build id here,
# which `oak upgrade --channel nightly` reads (scripts/release-oak-ci.sh).
MINISIGN_TRUSTED_COMMENT ?=

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

.PHONY: build install test fmt lint check ci \
        release-proof test-release-scripts \
        build-release-all build-release-macos build-release-linux \
        build-release-windows build-release-windows-xwin \
        sign-release sign-release-macos notarize-mac \
        upload-release promote-release release-preflight-artifacts \
        release-publish release-all

# ----------------------------------------------------------------------------
# Local build / install
# ----------------------------------------------------------------------------

build:
	$(CARGO) build --release

# Install the `oak` binary to ~/.cargo/bin.
install:
	$(CARGO) install --path cli --locked

test:
	$(CARGO) test

fmt:
	$(CARGO) fmt --all

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

check: fmt lint test

# Non-mutating verification, mirroring Oak CI
# (oak/oak's `ci` workflow): fmt is checked (not rewritten), and tests run
# under nextest so each test gets its own process (some tests set
# process-global env vars). Needs `cargo install cargo-nextest`.
ci:
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) nextest run --workspace

# Non-mutating launch/release readiness proof. This mirrors the source checks
# and crates.io dry-run semantics in oak/oak's `publish-crates` workflow:
# oakvcs-core can be dry-run-published locally, but oakvcs-cli can only be
# dry-run-published after that same core version is visible on crates.io.
release-proof:
	@CARGO="$(CARGO)" MINISIGN="$(MINISIGN)" MINISIGN_SECKEY="$(MINISIGN_SECKEY)" MINISIGN_PASSWORD="$(MINISIGN_PASSWORD)" REQUIRE_RELEASE_SIGNING="$(REQUIRE_RELEASE_SIGNING)" scripts/release-proof.sh

# Regression tests for the release pipeline's shell layer: the strict shared
# promotion predicate (scripts/release-state.sh, incl. the promoted_at
# type/format matrix), upload-release's staging-protocol rollback handling
# (404/410 + release_writes_retired) against a stub server, promote-release's
# CAS hash-map construction, the post-promotion confirmation gate, and the
# signing-identity derivation (scripts/derive-sign-identity.sh against fixture
# `security find-identity` output — the 0/1/2/expired identity matrix and the
# MACOS_SIGN_IDENTITY cross-check).
# No network; nothing real is staged or promoted; no macOS or keychain needed.
# Needs jq + python3.
test-release-scripts:
	@bash scripts/tests/release-state-test.sh
	@bash scripts/tests/derive-sign-identity-test.sh
	@bash scripts/tests/macos-signing-keychain-test.sh

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
# build-release-macos must run on macOS (it Developer-ID signs).
# Ordering is encoded in the RECIPE BODY, not a prerequisite list: recipe
# lines run sequentially even under `make -j` (prerequisite siblings do NOT —
# parallel make is free to run them concurrently, which previously let
# sign-release race the builds and could sign/ship a stale target/releases/).
# The sub-makes inherit the jobserver, so each step still parallelizes
# INTERNALLY; only the cross-step boundaries are serialized.
build-release-all:
	@$(MAKE) build-release-macos
	@$(MAKE) build-release-linux
	@$(MAKE) sign-release
	@echo "Release binaries built in target/releases/"
	@ls -la target/releases/

# macOS half of the release: build + Developer-ID-sign the two darwin arches.
# Runs on macOS only (the Apple SDK cross-compiles between its own arches, so no
# zig is needed here).
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
	@$(MAKE) --no-print-directory sign-release-macos

# Everything codesigned on darwin: the two CLI binaries.
DARWIN_SIGNED = oak-darwin-arm64 oak-darwin-x86_64

# Developer ID code signing for the darwin artifacts build-release-macos
# leaves in target/releases/. Its own target so a signing pass can run without
# invoking cargo: oak/oak's `release` workflow builds first and imports the
# certificate only afterwards, so no crate build script ever runs while the
# signing key is unlocked.
#
# Hardened runtime (--options runtime) + a trusted timestamp keep the binaries
# notarization-ready.
#
# Two paths only, and NO silent third one (see the SIGN_IDENTITY comment at the
# top of this file). SIGN_IDENTITY empty => skip with a warning, so a machine
# without the cert can still produce runnable ad-hoc-signed binaries.
# SIGN_IDENTITY set => it must be a VALID codesigning identity in the keychain
# or this fails hard. The old shape fell back to ad-hoc when a configured
# identity was not in the keychain, which is exactly how a rotated certificate
# shipped ad-hoc-signed binaries into a stable release run.
sign-release-macos:
	@[ "$$(uname -s)" = "Darwin" ] || { echo "Error: sign-release-macos must run on macOS."; exit 1; }
	@for b in $(DARWIN_SIGNED); do \
		[ -f "target/releases/$$b" ] || { echo "Error: target/releases/$$b is missing — run 'make build-release-macos' first."; exit 1; }; \
	done
	@if [ -z "$(SIGN_IDENTITY)" ]; then \
		echo "Warning: SIGN_IDENTITY not set — darwin binaries left ad-hoc signed (they run, but are not Developer-ID signed). Pass SIGN_IDENTITY=<sha1> for a signed build (see 'security find-identity -v -p codesigning')."; \
	else \
		if ! security find-identity -v -p codesigning 2>/dev/null | grep -q "$(SIGN_IDENTITY)"; then \
			echo "Error: signing identity '$(SIGN_IDENTITY)' is not a VALID codesigning identity in the keychain (rotated/expired cert, or wrong keychain?)."; \
			echo "Valid identities are:"; \
			security find-identity -v -p codesigning 2>/dev/null || true; \
			echo "Derive the current one with scripts/derive-sign-identity.sh. Refusing to fall back to ad-hoc signing."; \
			exit 1; \
		fi; \
		for b in $(DARWIN_SIGNED); do \
			echo "Signing $$b (Developer ID)..."; \
			codesign --force --timestamp --options runtime --sign "$(SIGN_IDENTITY)" "target/releases/$$b" || exit 1; \
			codesign --verify --strict --verbose=1 "target/releases/$$b" || exit 1; \
		done; \
	fi

# Linux half of the release: cargo-zigbuild both linux arches (no Docker),
# pinned to glibc $(GLIBC_VER) for broad distro compatibility. reqwest is on
# rustls + webpki-roots (no OpenSSL) and every unix-only path is #[cfg(unix)]-
# gated.
build-release-linux:
	@command -v cargo-zigbuild >/dev/null || { echo "Error: cargo-zigbuild not found. Install with: cargo install cargo-zigbuild"; exit 1; }
	@command -v zig >/dev/null || { echo "Error: zig not found. Install with: brew install zig (or your distro's package)"; exit 1; }
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

# Windows half of the release: the x86_64 windows-msvc binary.
#
# MUST run on a Windows host with the MSVC toolchain; to cross-compile from
# macOS/Linux, use build-release-windows-xwin below. reqwest is on
# rustls + the `ring` crypto provider (NOT aws-lc-rs), so no NASM/CMake/C
# toolchain is needed — only the Rust + MSVC linker the runner already has.
# Invoked through Git Bash on the GitHub windows runner (shell: bash).
build-release-windows:
	@[ "$$(uname -s 2>/dev/null | cut -c1-5)" != "Linux" ] && [ "$$(uname -s 2>/dev/null)" != "Darwin" ] || { echo "Error: build-release-windows must run on a Windows host; use build-release-windows-xwin to cross-compile."; exit 1; }
	@echo "Building windows-x86_64..."
	@mkdir -p target/releases
	@rustup target list --installed 2>/dev/null | grep -qx "x86_64-pc-windows-msvc" || { echo "Error: rust target x86_64-pc-windows-msvc not installed. Install with: rustup target add x86_64-pc-windows-msvc"; exit 1; }
	$(CARGO) build --release --package oakvcs-cli --target x86_64-pc-windows-msvc
	cp target/x86_64-pc-windows-msvc/release/oak.exe target/releases/oak-windows-x86_64.exe

# The same windows-x86_64 binary cross-compiled from macOS (or Linux) with
# cargo-xwin, which fetches the MSVC CRT + Windows SDK libraries (accepting
# Microsoft's license) and links with clang-cl/lld-link. This is how
# oak/oak's `release` workflow builds Windows on the macOS release runner. The
# `windows` crate ships its own import libraries, so nothing beyond the CRT and
# SDK is needed. ring's C code needs
# llvm-lib, which Homebrew's keg-only llvm provides (on Linux, the distro's
# llvm/clang). Setup: `cargo install cargo-xwin --locked`, `brew install llvm`,
# `rustup target add x86_64-pc-windows-msvc`.
CARGO_XWIN ?= cargo xwin
LLVM_BIN = $(shell brew --prefix llvm 2>/dev/null)/bin
build-release-windows-xwin:
	@$(CARGO_XWIN) --version >/dev/null 2>&1 || { echo "Error: cargo-xwin not found. Install with: cargo install cargo-xwin --locked"; exit 1; }
	@rustup target list --installed 2>/dev/null | grep -qx "x86_64-pc-windows-msvc" || { echo "Error: rust target x86_64-pc-windows-msvc not installed. Install with: rustup target add x86_64-pc-windows-msvc"; exit 1; }
	@PATH="$(LLVM_BIN):$$PATH" command -v llvm-lib >/dev/null || { echo "Error: llvm-lib not found. Install with: brew install llvm (keg-only; this target adds it to PATH)"; exit 1; }
	@echo "Building windows-x86_64 (cargo-xwin)..."
	@mkdir -p target/releases
	PATH="$(LLVM_BIN):$$PATH" $(CARGO_XWIN) build --release --locked --package oakvcs-cli --target x86_64-pc-windows-msvc
	cp target/x86_64-pc-windows-msvc/release/oak.exe target/releases/oak-windows-x86_64.exe

# minisign-sign every release artifact in target/releases/ that clients verify
# directly, writing a <artifact>.minisig sidecar next to each. Run automatically
# at the end of build-release-all; also runnable standalone to re-sign. No-op
# with a warning when MINISIGN_SECKEY is unset.
sign-release:
	@if [ -z "$(MINISIGN_SECKEY)" ]; then \
		echo "Warning: MINISIGN_SECKEY not set — release artifacts are UNSIGNED. 'oak upgrade' and install.sh will refuse them. Set MINISIGN_SECKEY=<path-to-.key> to sign."; \
	else \
		command -v $(MINISIGN) >/dev/null || { echo "Error: '$(MINISIGN)' not found (try 'brew install minisign') but MINISIGN_SECKEY is set."; exit 1; }; \
		[ -f "$(MINISIGN_SECKEY)" ] || { echo "Error: MINISIGN_SECKEY '$(MINISIGN_SECKEY)' does not exist."; exit 1; }; \
		for artifact in oak-darwin-arm64 oak-darwin-x86_64 oak-linux-x86_64 oak-linux-arm64 oak-windows-x86_64.exe; do \
			[ -f "target/releases/$$artifact" ] || continue; \
			echo "Signing $$artifact (minisign)..."; \
			if [ -n "$(MINISIGN_PASSWORD)" ]; then \
				printf '%s\n' "$(MINISIGN_PASSWORD)" | $(MINISIGN) -S -s "$(MINISIGN_SECKEY)" -m "target/releases/$$artifact" -x "target/releases/$$artifact.minisig" $(if $(MINISIGN_TRUSTED_COMMENT),-t "$(MINISIGN_TRUSTED_COMMENT)") || exit 1; \
			else \
				$(MINISIGN) -S -s "$(MINISIGN_SECKEY)" -m "target/releases/$$artifact" -x "target/releases/$$artifact.minisig" $(if $(MINISIGN_TRUSTED_COMMENT),-t "$(MINISIGN_TRUSTED_COMMENT)") || exit 1; \
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
# Fails closed: every curl uses --fail-with-body (curl >= 7.76), so an HTTP
# 4xx/5xx from /api/releases aborts the target (after printing the error body)
# instead of letting a partial sync look like success.
# The canonical platform set of a stable release — the SINGLE definition the
# upload and promote targets derive from. A promotion naming anything other
# than exactly these five is refused before any request is made.
RELEASE_PLATFORMS := darwin-arm64 darwin-x86_64 linux-x86_64 linux-arm64 windows-x86_64

# Uploads only STAGE assets: nothing becomes selectable by install.sh or
# `oak upgrade`'s `latest` resolution until a separate `make promote-release`.
# Staging is content-addressed server-side and re-uploads are allowed, so a
# failed or partial upload is always safe to re-run. Verify the staged assets
# (the version-pinned GETs serve staged releases exactly for this purpose)
# BEFORE promoting — release CI does this byte-level between the two targets.
# Uses the STAGING endpoint POST /api/releases/stage (oakspace changeset B2):
# any pre-B2 or rolled-back server 404s it, so an upload to a server that
# would silently promote platform-by-platform fails STRUCTURALLY here — this
# endpoint response, not the workflow's advisory pre-build probe, is the
# actual guard.
upload-release:
	@if [ -z "$(VERSION)" ]; then echo "VERSION is required. Usage: make upload-release VERSION=v0.94.0"; exit 1; fi
	@if [ -z "$(OAK_ADMIN_API_KEY)" ]; then echo "OAK_ADMIN_API_KEY is required"; exit 1; fi
	@command -v jq >/dev/null || { echo "Error: jq is required for upload-release (already_promoted attestation adoption)."; exit 1; }
	@OAK_URL=$${OAK_URL:-https://oak.space}; \
	stage_body=$$(mktemp); \
	rm -f target/releases/stage-attestations.json; \
	stage_one() { \
		platform="$$1"; binary="$$2"; \
		echo "Staging $$platform..."; \
		if [ ! -f "$$binary.minisig" ]; then echo "Error: $$binary.minisig not found — clients refuse unsigned release artifacts."; exit 1; fi; \
		code=$$(curl -sS -o "$$stage_body" -w '%{http_code}' -X POST "$$OAK_URL/api/releases/stage" \
			-H "Authorization: Bearer $(OAK_ADMIN_API_KEY)" \
			-F "version=$(VERSION)" \
			-F "platform=$$platform" \
			-F "minisig=@$$binary.minisig" \
			-F "binary=@$$binary"); \
		case "$$code" in \
			2??) cat "$$stage_body"; echo ""; \
				if jq -e '.already_promoted == true' "$$stage_body" >/dev/null 2>&1; then \
					as=$$(jq -r '.sha256 // empty' "$$stage_body"); \
					am=$$(jq -r '.minisig_sha256 // empty' "$$stage_body"); \
					if [ -n "$$as" ] && [ -n "$$am" ]; then \
						[ -f target/releases/stage-attestations.json ] || printf '{}' > target/releases/stage-attestations.json; \
						jq --arg p "$$platform" --arg s "$$as" --arg m "$$am" \
							'.[$$p] = {sha256: $$s, minisig_sha256: $$m}' \
							target/releases/stage-attestations.json > target/releases/stage-attestations.json.tmp \
							&& mv target/releases/stage-attestations.json.tmp target/releases/stage-attestations.json || exit 1; \
						echo "note: $$platform is already promoted — adopted the server's STORED attestation for the promote payload (a re-signed local minisig hashes differently and would CAS-mismatch)."; \
					fi; \
				fi;; \
			*) if grep -q '"error_code"[^}]*"release_writes_retired"' "$$stage_body" 2>/dev/null; then \
				echo "Error: POST $$OAK_URL/api/releases/stage returned HTTP $$code with error_code release_writes_retired — server does not speak the staging protocol (rolled back?). B1 is unconditionally read-only for releases; deploy oakspace changeset B2 and re-run."; exit 1; \
			elif [ "$$code" = "404" ] || [ "$$code" = "410" ]; then \
				echo "Error: POST $$OAK_URL/api/releases/stage returned HTTP $$code — server does not speak the staging protocol (rolled back?). Refusing any legacy fallback; deploy oakspace changeset B2 (after B1) and re-run."; exit 1; \
			else \
				echo "Error: staging $$platform failed (HTTP $$code) — aborting so a partial sync can't pass for a release. Server said:"; cat "$$stage_body"; echo ""; exit 1; \
			fi;; \
		esac; \
	}; \
	for platform in $(RELEASE_PLATFORMS); do \
		binary="target/releases/oak-$$platform"; \
		case "$$platform" in windows-*) binary="$$binary.exe";; esac; \
		if [ -f "$$binary" ]; then \
			stage_one "$$platform" "$$binary"; \
		else \
			echo "Error: $$binary not found"; \
			exit 1; \
		fi; \
	done; \
	rm -f "$$stage_body"
	@echo "Upload complete — $(VERSION) is STAGED, not yet selectable. Verify the staged assets, then run: make promote-release VERSION=$(VERSION) OAK_ADMIN_API_KEY=..."

# Promote a fully staged release: POST /api/releases/$(VERSION)/promote with a
# HASH MAP body — {"platforms": {"<slug>": {"sha256": "...",
# "minisig_sha256": "..."}}} — which the server CAS-compares against the
# staged bytes inside its locked transaction and, only on an exact match for
# the exact platform set, atomically marks the release promoted. Only
# promoted releases are selectable by install.sh and `oak upgrade`'s `latest`
# resolution. Promote is IDEMPOTENT: re-promoting an already-promoted version
# with an identical map returns 200 (already_promoted); a mismatch is a 409
# carrying the server's platform/expected/actual detail (surfaced verbatim).
# PROMOTE_PAYLOAD=<file> posts that JSON verbatim — the release workflow
# passes the payload its VERIFICATION step wrote, so the promoted hashes are
# exactly the verified ones. Without it, the map is computed from
# target/releases/ (the local `make release-all` path; requires jq). Run only
# AFTER verifying the staged assets — promotion is the moment $(VERSION)
# becomes publicly selectable. (Server side: oakspace changesets A/B1/B2 must
# be deployed — the release workflow's preflight probes are advisory; the
# staging/promote endpoints themselves are the guard.)
promote-release:
	@if [ -z "$(VERSION)" ]; then echo "VERSION is required. Usage: make promote-release VERSION=v0.94.0"; exit 1; fi
	@if [ -z "$(OAK_ADMIN_API_KEY)" ]; then echo "OAK_ADMIN_API_KEY is required"; exit 1; fi
	@OAK_URL=$${OAK_URL:-https://oak.space}; \
	command -v jq >/dev/null || { echo "Error: jq is required for promote-release (payload validation and construction)."; exit 1; }; \
	payload="$(PROMOTE_PAYLOAD)"; \
	if [ -n "$$payload" ]; then \
		[ -s "$$payload" ] || { echo "Error: PROMOTE_PAYLOAD '$$payload' is missing or empty."; exit 1; }; \
	else \
		payload="target/releases/promote-payload.json"; \
		printf '{"platforms":{}}' > "$$payload"; \
		for platform in $(RELEASE_PLATFORMS); do \
			case "$$platform" in \
				windows-x86_64) binary="target/releases/oak-windows-x86_64.exe";; \
				*) binary="target/releases/oak-$$platform";; \
			esac; \
			[ -f "$$binary" ] || { echo "Error: $$binary not found — a promotion covers exactly the canonical five platforms ($(RELEASE_PLATFORMS)); cannot compute the $$platform hash."; exit 1; }; \
			[ -f "$$binary.minisig" ] || { echo "Error: $$binary.minisig not found — cannot compute its promote hash."; exit 1; }; \
			bsha=$$(shasum -a 256 "$$binary" | awk '{print $$1}'); \
			msha=$$(shasum -a 256 "$$binary.minisig" | awk '{print $$1}'); \
			if [ -f target/releases/stage-attestations.json ]; then \
				aas=$$(jq -r --arg p "$$platform" '.[$$p].sha256 // empty' target/releases/stage-attestations.json); \
				aam=$$(jq -r --arg p "$$platform" '.[$$p].minisig_sha256 // empty' target/releases/stage-attestations.json); \
				if [ -n "$$aas" ] && [ -n "$$aam" ]; then \
					bsha=$$aas; msha=$$aam; \
					echo "note: using the server's STORED attestation for $$platform (already promoted at stage time; the local re-signed minisig would CAS-mismatch)."; \
				fi; \
			fi; \
			jq --arg p "$$platform" --arg s "$$bsha" --arg m "$$msha" \
				'.platforms[$$p] = {sha256: $$s, minisig_sha256: $$m}' "$$payload" > "$$payload.tmp" \
				&& mv "$$payload.tmp" "$$payload" || exit 1; \
		done; \
	fi; \
	jq -e '.platforms | type == "object"' "$$payload" >/dev/null 2>&1 \
		|| { echo "Error: refusing to POST promote — payload is not valid JSON with a platforms map:"; cat "$$payload"; echo ""; exit 1; }; \
	canon=$$(printf '%s\n' $(RELEASE_PLATFORMS) | jq -R . | jq -sc 'sort'); \
	keys=$$(jq -c '.platforms | keys | sort' "$$payload"); \
	if [ "$$keys" != "$$canon" ]; then \
		missing=$$(jq -rn --argjson c "$$canon" --argjson k "$$keys" '($$c - $$k) | join(" ")'); \
		extra=$$(jq -rn --argjson c "$$canon" --argjson k "$$keys" '($$k - $$c) | join(" ")'); \
		echo "Error: refusing to POST promote — payload platform keys are not exactly the canonical five."; \
		if [ -n "$$missing" ]; then echo "  missing: $$missing"; fi; \
		if [ -n "$$extra" ]; then echo "  extra: $$extra"; fi; \
		exit 1; \
	fi; \
	echo "Promoting $(VERSION) with hash map (canonical five validated):"; cat "$$payload"; echo ""; \
	body=$$(mktemp); \
	code=$$(curl -sS -o "$$body" -w '%{http_code}' -X POST "$$OAK_URL/api/releases/$(VERSION)/promote" \
		-H "Authorization: Bearer $(OAK_ADMIN_API_KEY)" \
		-H "Content-Type: application/json" \
		--data-binary @"$$payload"); \
	case "$$code" in \
		200) cat "$$body"; echo ""; rm -f "$$body";; \
		409) echo "Error: promote of $(VERSION) rejected with 409 (CAS mismatch: staged bytes differ from the verified hashes, or the platform set is wrong). Server detail:"; cat "$$body"; echo ""; rm -f "$$body"; exit 1;; \
		404|410) echo "Error: POST $$OAK_URL/api/releases/$(VERSION)/promote returned HTTP $$code — server does not speak the staging protocol (rolled back?). Deploy oakspace changeset B2 (after B1) and re-run."; rm -f "$$body"; exit 1;; \
		*) echo "Error: promote of $(VERSION) failed (HTTP $$code) — the release stays staged and unselectable. Server said:"; cat "$$body"; echo ""; rm -f "$$body"; exit 1;; \
	esac
	@echo "Promote complete — $(VERSION) is now selectable."

# Zero-request preflight: ALL FIVE artifacts and their minisign signatures
# must exist on disk BEFORE the first staging request is made — a missing
# artifact must fail here, with zero requests sent, not after the others
# have already staged.
release-preflight-artifacts:
	@missing=""; \
	for platform in $(RELEASE_PLATFORMS); do \
		case "$$platform" in \
			windows-x86_64) binary="target/releases/oak-windows-x86_64.exe";; \
			*) binary="target/releases/oak-$$platform";; \
		esac; \
		[ -f "$$binary" ] || missing="$$missing $$binary"; \
		[ -f "$$binary.minisig" ] || missing="$$missing $$binary.minisig"; \
	done; \
	if [ -n "$$missing" ]; then \
		echo "Error: release preflight failed — missing artifact(s) before any staging request was made:$$missing"; \
		echo "A release covers exactly the canonical five platforms ($(RELEASE_PLATFORMS)); build/sign everything first."; \
		exit 1; \
	fi; \
	echo "Release preflight OK: all five artifacts + signatures present."

# The publish half of release-all — preflight, stage, promote — split out so
# it can be exercised end-to-end against a stub server without building.
# Sequenced in the recipe body (safe under `make -j`; see build-release-all).
release-publish:
	@$(MAKE) release-preflight-artifacts
	@$(MAKE) upload-release
	@$(MAKE) promote-release

# Build, stage, and promote everything. Requires the FULL canonical five:
# release-preflight-artifacts fails with ZERO requests made when anything is
# missing, and promote-release refuses any payload that is not exactly the five platforms. Sequenced in
# the recipe body — under `make -j` a prerequisite list (the previous shape,
# `release-all: build-release-all release-publish`) lets parallel make start
# preflight/staging while builds and signing still run, staging whatever
# stale bytes sit in target/releases/; recipe lines cannot be reordered.
# NOTE: this local path promotes immediately after staging with no
# independent verification pass between — the release workflow
# (oak/oak's `release`) is the recommended route; it byte-verifies every staged asset against the
# build before promoting.
# Usage: make release-all VERSION=v0.94.0 OAK_ADMIN_API_KEY=your-key
release-all:
	@$(MAKE) build-release-all
	@$(MAKE) release-publish
