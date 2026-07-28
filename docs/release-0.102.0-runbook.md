# Release 0.102.0 Runbook

Status: the version bump (`0.101.0` → `0.102.0`) is prepared on a branch; the
steps below are what remains AFTER that branch merges to `main`. Nothing here
has been executed. Follow the standing process in
[release-readiness.md](release-readiness.md); this note pins the
version-specific parameters.

> **FREEZE — active; guard + B1 LANDED.** The legacy Release workflow is
> disabled at the repository level on the GitHub mirror
> (`disabled_manually`), and the guard changeset (branch mrmrs-925c46,
> MERGED — oak/oak main e45188cc) adds a mechanical `freeze` job that
> refuses any non-dry-run dispatch plus a fail-closed Makefile upload guard
> for manual paths. Server-side, changesets A and B1 are DEPLOYED (oakspace
> main 9c4077d2). The remaining HOLD is solely: the B2 deploy (being
> recreated) and this branch's merge-day resolution below.
> After B2 lands, enable the NEW workflow identity only —
> `.github/workflows/release-staging.yml`, "Release (staging)", added at
> merge-day while `release.yml` is deleted. The legacy "Release" identity
> (ID 287206482) remains `disabled_manually` PERMANENTLY and is never
> re-enabled: its pre-freeze runs stay rerunnable under the old
> publish-first pipeline until GitHub's rerun window closes (~2026-08-06
> for the 2026-07-07 runs), and reruns execute their original SHA.
>
> **HOLD — server dependency.** Do NOT dispatch the release workflow until
> ALL companion oakspace promotion changesets are deployed, in order:
> **A** (readers-first `promoted_at` semantics — the `latest` selection that
> install.sh and `oak upgrade` read considers promoted releases only;
> already deployed), then **B1** (readers + verification semantics — must be
> both the CURRENT deploy **and** the `.prev` rollback slot before B2 ships,
> so a rollback can never resurrect pre-promotion reader behavior), then
> **B2** (content-addressed staging behind the NEW endpoint
> `POST /api/releases/stage` — any pre-B2 or rolled-back binary 404s it, so
> uploads to a wrong server fail STRUCTURALLY instead of silently promoting
> platform-by-platform — plus `POST /api/releases/{version}/promote` taking
> a HASH MAP body, `{"platforms": {"<slug>": {"sha256": "...",
> "minisig_sha256": "..."}}}`, which the server CAS-compares against the
> staged bytes inside its locked transaction; per-version immutability and
> idempotent re-promote — an identical map returns 200
> `{"already_promoted": true}`, a mismatch is a 409 carrying
> platform/expected/actual detail). B1 is UNCONDITIONALLY read-only for
> releases: every legacy release write returns 410 with error_code
> `release_writes_retired` (the stage step recognizes that error_code
> specifically and reports it as the rollback condition). B2's staging +
> CAS promote endpoints are the single write surface. B2's
> content-addressed staging is also what closes the R2 overwrite race the
> old direct-upload path had — a re-upload can no longer clobber bytes a
> reader is mid-download on. The workflow's preflight probes are ADVISORY
> fast-fail UX only (the staging/promote endpoints themselves are the
> structural guard): listing probe — `GET /api/releases` with the admin
> Bearer key: 200 + `promoted_at` = staging-capable (B2+), exactly 401 =
> pre-B2 → refuse dispatch ("server not staging-capable yet"), anything
> else fails the probe; B2-probes — unauthenticated POSTs to the promote
> path (400/401 present; 404/405 absent) and to `/api/releases/stage`
> (400/401 present; 404/410 absent); any other code fails the probe
> outright. (Authoritative details in the probe matrix below.) **B1 exposes no distinct probeable surface** —
> its presence (current AND `.prev`) is asserted operationally via the
> checklist in step 0 below, not probed. On the promotion-aware server,
> uploads only STAGE assets: a release is not selectable until promote
> CAS-verifies every platform and atomically marks it promoted. Partial
> uploads are therefore never installable, and neither are
> complete-but-unpromoted ones. "Promoted" everywhere in the pipeline means
> the STRICT predicate in `scripts/release-state.sh`: exactly the six
> expected platforms, each exactly once, all with a valid RFC3339-shaped `promoted_at` —
> partial, duplicate, mixed, or malformed listings fail closed before any
> mutation.

## What 0.102.0 ships (since the v0.101.0 tag)

Checkout-free review fixes:

- 16 MiB text-diff threshold (`oak_core::MAX_TEXT_DIFF_BYTES`, up from 1 MiB)
  plus an `-a/--text` flag to force a text diff past the size guard while
  keeping the NUL-byte binary guard (filed as fb-81/fb-82; landed with the
  fb-31..fb-39 audit commit `7f710e30`).
- `oak branch review` net-merge preview falls back to a tree diff with a
  `net_merge_unavailable_tree_fallback` caveat instead of reporting zero
  counts when the net-merge prediction is unavailable (fb-87; same commit).

Release-pipeline hardening shipped in this same changeset (first exercised by
this release):

- `release-staging.yml` publishes in a fail-closed, no-split-brain order on stable
  (non-dry-run) runs: a `preflight` job requires every hard-required secret
  (`OAK_ADMIN_API_KEY`, `MINISIGN_SECKEY`, the `MACOS_*` signing/profile and
  `MACOS_NOTARY_*` secrets), STRICTLY probes the server promotion capability
  (see the probe matrix below), and decides the release MODE from the
  observed state of both channels — all BEFORE any build starts. In `fresh`
  mode the GitHub Release is created as an invisible DRAFT; assets are
  STAGED on oak.space (unpromoted, not selectable); a byte-level
  verification step checks both channels while everything is still
  invisible; then promote makes oak.space selectable; and only then is the
  draft flipped live and marked `--latest`.

Release-mode state machine (decided pre-build; builds and draft mutations
run ONLY in `fresh` mode — rebuilding on a retry is fatal because minisign
trusted comments are timestamped, so a fresh signature never byte-matches
the immutable promoted `.minisig` sidecar):

The oak.space column is the STRICT shared predicate
(`scripts/release-state.sh`, sourced by mode detection, the stage-step
fallback, and promote confirmation alike). The `/api/releases` response
envelope must be a TOP-LEVEL JSON ARRAY of row objects — only direct
elements are inspected (no recursive descent), so wrapper shapes like
`{"metadata": [...]}` fail closed. EVERY row is schema-validated BEFORE any
version filtering (version: string, platform: string, promoted_at present
and null-or-RFC3339-string) — a malformed row for any version fails closed
instead of silently dropping out of the filter as "no matching release".
Platform slugs are compared as JSON values against the canonical six
(empty strings, whitespace padding, and unknown slugs are rejected
explicitly — never a textual sort-and-join that would normalize padding
away). The mode-decision table (`release_decide_mode`), the draft
retarget/never-demote decision (`release_draft_action`), and the
post-promote `/sha256` spot check (`release_spot_check_sha256`) are also
factored into `scripts/release-state.sh` so the workflow's decisions are
regression-tested directly. `promoted` = exactly the six expected
platforms, each exactly once, all with a valid RFC3339-shaped `promoted_at`;
`unpromoted` = no rows for the version, or rows with NONE promoted (staging
in progress). Anything else — malformed JSON, bad envelopes,
schema-violating rows, wrong promoted_at types
(number/object/array/boolean/empty string), invalid slugs, duplicates,
mixed staged/promoted, promoted-but-wrong-set — is INVALID and fails the
run closed before any mutation, printing what was found. The predicate, the
staging rollback handling, the CAS payload construction, and the
post-promotion confirmation gate are covered by repo-resident regression
tests: `make test-release-scripts` (scripts/tests/release-state-test.sh; no
network, needs jq + python3) — run it after touching any of the release
shell.

| GitHub release | oak.space (predicate) | Mode | What runs |
| -------------- | --------------------- | ---- | --------- |
| absent         | unpromoted | `fresh` | build + sign → draft → stage → verify (vs this run's build) → promote (verified hash map) → flip |
| draft          | unpromoted | `fresh` | same; the draft is re-clobbered in place |
| draft          | promoted   | `resume-post-promotion` | NO rebuild/re-sign/clobber; verify the EXISTING draft assets cross-channel against oak.space; strict structural promotion confirmation; flip |
| published      | promoted   | `already-published` | never demote; cross-channel verify; confirm promotion; re-assert `--latest`; success |
| published      | unpromoted | (fail) | inconsistent — the flip only runs after promote; promotion state was lost or channels diverged; operator action |
| absent         | promoted   | (fail) | inconsistent — promote only ever runs after the draft exists; operator action |
| any            | invalid    | (fail) | the predicate failed closed; operator action |

Preflight probe matrix (ADVISORY fast-fail UX — the staging/promote
endpoints are the structural guard — but still STRICT: only the exact
expected codes count; anything else, e.g. 302 or 500, fails the preflight
with the observed code):

| Probe | Request | Present | Absent | Anything else |
| ----- | ------- | ------- | ------ | ------------- |
| Listing (staging-capable, B2+) | `GET /api/releases` with the ADMIN Bearer key (`release_listing_probe`) | exactly 200 AND rows carry `promoted_at` | exactly 401 → fail with "server not staging-capable yet" (pre-B2 the listing accepts only a user session — the admin key is a write-endpoint secret there and yields no user; B2 amends the listing to also accept it, read-only) | 200 without `promoted_at`, or any other code → fail with observed code |
| B1 (readers + verification) | none — no probeable surface (the listing probe cannot distinguish A/B1 from each other) | asserted operationally (step 0 checklist: current AND `.prev`) | — | — |
| B2 promote | unauthenticated `POST /api/releases/{bogus}/promote` | exactly 400 or 401 | exactly 404 or 405 → fail with deploy message | fail with observed code |
| B2 stage | unauthenticated `POST /api/releases/stage` | exactly 400 or 401 | exactly 404 or 410 → fail with deploy message | fail with observed code |

The workflow is dispatchable only post-B2 by construction: the listing probe
401s on pre-B2 servers (hard fail), and even a skipped probe cannot publish —
the `/stage` endpoint guard fails structurally on any pre-B2 binary.
- The verification is byte-level, not just presence, and runs BEFORE
  promote (the version-pinned GETs serve staged releases exactly for this
  purpose). In every mode: the GitHub asset set must EXACTLY equal the
  expected 13-asset set (nothing missing, nothing unexpected), SHA256SUMS
  must list all six client-installed binaries and every line must verify
  against the assets. In `fresh` mode additionally: every GitHub asset is
  downloaded and byte-compared against this run's build, and for each of
  the six oak.space platforms the served binary's sha256, the published
  `/sha256` endpoint (the exact value install.sh consumes), and the served
  `/minisig` bytes must all match the built artifacts. In the resume/
  published modes the comparison is cross-channel instead (existing GitHub
  assets vs oak.space) — nothing was rebuilt, so nothing is compared to a
  rebuild.
- Signing fails closed end-to-end: `_build-cli.yml` requires the Developer
  ID cert on stable runs and asserts post-build that both darwin binaries
  actually carry a `Developer ID Application` signature (ad-hoc is fatal;
  canary and dry runs keep the warning path), and `make release-proof`
  now proves KEY AGREEMENT — the minisign smoke signature must verify
  against the `RELEASE_PUBKEY` baked into `cli/src/commands/upgrade.rs`
  (extracted from source as the single source of truth), so a wrong seckey
  is caught before any artifact is signed with it.
- `make upload-release` stages only, via `POST /api/releases/stage` — a
  404/410 from it means the server does not speak the staging protocol
  (rolled back?) and fails with exactly that message; any other non-2xx
  aborts with the server body printed. Promotion is a separate
  `make promote-release`, which POSTs the CAS hash map — the workflow
  passes `PROMOTE_PAYLOAD=$RUNNER_TEMP/promote-payload.json`, written by
  the VERIFICATION step from the hashes it attested, so the server compares
  exactly what was verified; a 409 surfaces the server's
  platform/expected/actual detail verbatim. BOTH payload modes validate
  pre-POST that the payload keys are EXACTLY the canonical six platforms
  (defined once in the Makefile as `RELEASE_PLATFORMS`), naming any
  missing/extra slug and refusing to POST otherwise — a five-platform
  promotion is impossible from either path. `make release-all` chains
  build → `release-preflight-artifacts` (all six artifacts + signatures
  must exist on disk BEFORE the first staging request — a missing mounter
  fails with zero requests made) → stage (REQUIRE_MOUNTER forced) →
  promote; the publish half is exposed as `make release-publish` so the
  whole sequence is regression-tested against a stub. Cross-step ordering
  (build → sign → preflight → stage → promote) is encoded in RECIPE BODIES,
  not prerequisite lists, so `make -j` cannot reorder the steps and stage a
  stale `target/releases/` — regression-tested with a `-j8` stub run that
  requires zero requests before the build-completion sentinel (and a
  kept reproduction showing the old prerequisite shape failing it). (No
  independent verification pass in the local path — prefer the workflow.)
  After the workflow's promote + strict confirmation, a defense-in-depth
  spot check re-GETs each platform's `/sha256` endpoint and requires it to
  equal the attested payload hash before the GitHub flip (not a
  re-verification — staged bytes are content-addressed and immutable; it
  closes the gap between "rows say promoted" and "promoted rows serve the
  attested hashes").

Retry matrix — where a stable run can fail and what a re-run does (re-runs
are always end-to-end; preflight re-detects the mode from server state, so
every retry converges without a reproducible rebuild and a live release is
never demoted):

| Failure point                       | Public state afterwards                        | Retry (mode the re-run detects, and what happens) |
| ----------------------------------- | ---------------------------------------------- | ----- |
| preflight / build / sign            | nothing published                              | `fresh`; nothing to clean up |
| draft GH release / asset upload     | GH draft only (invisible)                      | `fresh`; the draft is re-clobbered in place — and first RETARGETED to the re-run's commit if its targetCommitish differs, so the tag created at flip time can never point at an older commit than the assets were built from |
| oak.space staging upload            | GH draft; oak.space staged partial (unselectable) | `fresh`; staged re-uploads are allowed (content-addressed) |
| byte-level verification             | GH draft; oak.space staged (unselectable)      | `fresh` after fixing the cause; nothing went public |
| promote                             | GH draft; oak.space staged (unselectable)      | `fresh`; promote validates set-equality against staging |
| GH flip (only post-promote step)    | oak.space PROMOTED (selectable); GH still draft | `resume-post-promotion`: NO rebuild/re-sign/clobber (a fresh minisign signature could never byte-match the immutable promoted sidecar); the existing draft assets are verified cross-channel against oak.space, promotion is confirmed structurally (`promoted_at`), and the flip is retried. Brief window where install.sh serves 0.102.0 but GitHub `latest` still says 0.101.0 — benign (both complete, verified) and closed by the re-run. |
| after the flip (ambiguous failure post-publication) | BOTH channels public and complete | `already-published`: never demoted, nothing re-uploaded; cross-channel verification + structural promotion confirmation + re-assert `--latest`, then success. |
| mid-run race: version promoted by another actor while a `fresh` run is staging | per the racing run | the stage step re-checks the STRUCTURED promotion state (`promoted_at` via the authenticated list — no prose matching) and skips to verification; verification then compares this run's build to the promoted bytes and fails closed with an explicit "already promoted" error if they differ — re-dispatch, and preflight will route the new run to the correct mode. |
- **OakMount decision: fail closed.** v0.101.0 shipped `OakMount.zip`
  notarized and stapled (verified against the published asset:
  `stapler validate` passes, `spctl` reports "Notarized Developer ID"), so a
  stable release that silently self-skips the app or ships it un-notarized
  would be a regression. `_build-macos-app.yml` now exposes a `notarized`
  output and `release-staging.yml` requires `built == true`, `OakMount.zip` +
  `.minisig` present, and `notarized == true` before publishing. CLI-only or
  un-notarized builds remain possible via `dry_run: true` (and canary is
  unaffected — it tolerates the self-skip as before).

Also on `main` since v0.101.0:

- `oak mount --branch`: mount an existing remote branch; mount pull fix;
  mount-aware `oak switch` (`8e5ec2ee`).
- Client-side awareness of path-permission-restricted content (`8df81e1b`).
- Installable oak-vcs agent skill + `oak skill install` (`2e6dd9c0`).
- fb-55 FSKit directory enumeration dropping root entries (`adc6afc6`).
- fb-56 review recommendations no longer suggest merge on high-risk rebuild
  branches (`9c06e2c0`).
- fb-44..fb-53 CLI friction batch: mount-aware remote branch inspection,
  `merge --force/--json`, free-form close reasons, checkout-free branch
  hunks/path filters, mount finish push recovery, fail-fast after main fetch
  integrity failure (`2a86ab95`).
- Pull/fetch backfill merge-parent ancestor verification fix (`407f4288`).
- fb-26/fb-29 CI surface (`oak ci status/runs/logs/rerun`) and merge riding
  out the CI gate (`12fd8dd3`).
- fb-28 merge-preview modify/modify misreported as deletions (`c41fd18c`).
- fb-25 status/reset/restore agreement on tracked-but-newly-ignored paths
  (`fe64e2c1`).

## Merge-day resolution (prescribed — do not improvise)

This branch (mrmrs-f7edae) WILL conflict with post-guard `main` (the guard
branch mrmrs-925c46 has landed — oak/oak main e45188cc) in `Makefile`
(~5 hunks: the upload loops, promote targets, and sequencing), and the
release-workflow change is an identity migration, not an in-place edit.
Expect `oak pull` to require hand-resolution. The resolution is:

1. Take THIS branch's version of `Makefile` wholesale — the staging-capable
   shape supersedes the guard branch's legacy upload guard.
2. Workflow identity migration: DELETE `.github/workflows/release.yml`
   entirely (main's copy carries the guard's freeze job protecting the
   LEGACY pipeline — there is no merged version of that file) and ADD this
   branch's `.github/workflows/release-staging.yml` (workflow name
   "Release (staging)"). The legacy "Release" workflow identity
   (ID 287206482) stays `disabled_manually` on the mirror FOREVER — never
   re-enable it: its pre-freeze runs remain rerunnable for GitHub's ~30-day
   window (the 2026-07-07 runs until ~2026-08-06), and a rerun executes its
   ORIGINAL SHA — the old publish-first pipeline, bypassing everything.
3. DELETE `scripts/tests/upload-guard-test.sh` and the `test-upload-guard`
   make target (guard-branch additions): that suite asserts the legacy
   `POST /api/releases` endpoint and the exact "Upload complete!" success
   string, both gone after this branch's Makefile lands — keeping it means
   keeping a guaranteed-red suite. Its coverage (staging rollback, 410
   `release_writes_retired`, first-failure abort) is superseded by
   `scripts/tests/release-state-test.sh`.
4. Verify the resolution with `make test-release-scripts` (must be fully
   green) before pushing the merge.

## Remaining steps to publish (do these after the bump merges)

0. **Wait for ALL oakspace promotion changesets to deploy, A → B1 → B2**
   (see the HOLD note at the top). The workflow's preflight probes what is
   probeable and fails fast, but run this checklist with the server team
   before dispatching:
   - [x] A: deployed (oakspace main 9c4077d2).
   - [x] B1: deployed (oakspace main 9c4077d2); confirm it also occupies
         the `.prev` rollback slot before B2 ships (no probeable surface —
         an operational assertion against the deploy history).
   - [ ] B2: `POST /api/releases/{version}/promote` is routed (also probed:
         unauthenticated POST returns 400/401, not 404/405; the listing
         probe with the admin key returns 200 + `promoted_at` rather than
         the pre-B2 401).

1. **Preflight** on a checkout of post-merge `main`:

   ```sh
   make release-proof
   make test-release-scripts
   ```

   For the signing gate: `REQUIRE_RELEASE_SIGNING=1 MINISIGN_SECKEY=... MINISIGN_PASSWORD=... make release-proof`.
   `test-release-scripts` regression-tests the release shell layer (strict
   promotion predicate incl. promoted_at type matrix, staging rollback +
   `release_writes_retired` handling, CAS payload construction, and the
   post-promotion confirmation gate) against stub servers — no network.

2. **Binary release** — enable (first time) and dispatch the GitHub Actions
   **Release (staging)** workflow (`.github/workflows/release-staging.yml`,
   workflow_dispatch) on the mirror at the post-merge commit. Never touch
   the legacy disabled "Release" identity (see the FREEZE note):
   - `version`: `v0.102.0` (or blank — it derives `v0.102.0` from
     `[workspace.package].version` and fails on mismatch).
   - Optionally run once with `dry_run: true` first (builds + signs, uploads
     the `oak-release-signed` artifact, publishes nothing).
   - The workflow builds all five CLI targets — including **darwin-arm64**
     (`_build-cli.yml` macOS job / `make build-release-macos`) and
     **linux-x86_64** (linux job / `make build-release-linux`,
     cargo-zigbuild) — plus the OakMount app, minisigns every artifact, and
     writes SHA256SUMS. The tag name **must** equal `v` + CARGO_PKG_VERSION;
     that is what `oak upgrade` compares.
   - A non-dry-run runs fail-closed and in order: secret + strict capability
     preflight AND release-mode detection before any build (see the state
     machine above — builds run only in `fresh` mode); notarized OakMount
     app required; darwin CLI binaries must end up Developer-ID signed;
     GitHub Release created as a DRAFT; assets STAGED to
     `oak.space/api/releases` via `make upload-release VERSION=v0.102.0`
     (OAK_ADMIN_API_KEY is required — this is the channel
     `curl -fsSL oak.space/install | sh` reads, and nothing is selectable
     while staged); byte-level verification of every staged asset on both
     channels against the run's own build, exact asset-set equality, and
     SHA256SUMS line validation (including the per-platform `/sha256`
     endpoint install.sh consumes); PROMOTE via `make promote-release`
     posting the verification step's hash map verbatim (the moment
     oak.space can serve v0.102.0 as latest); and only then the
     draft is flipped live and marked `--latest`. A pre-promote failure
     leaves both channels invisible; see the retry matrix above for every
     failure point and the mode a re-run detects.
   - There is no manual `git tag` step: the `v0.102.0` tag is created on the
     mirror when the workflow's final step publishes the draft (GitHub only
     materializes a draft release's tag at publish time). Oak itself is
     never tagged.

3. **Crates release** — dispatch **Publish to crates.io**
   (`.github/workflows/publish-crates.yml`, workflow_dispatch):
   - `version`: `0.102.0` (no leading `v`), `dry_run: false` (default is
     true; run the dry run first if desired).
   - Order is enforced by the workflow: `oakvcs-core` first, then
     `oakvcs-cli` (the CLI resolves core by version from the index).

4. **Verify distribution**:
   - `curl -fsSL https://oak.space/install | sh` on a clean machine installs
     0.102.0 (checks the /api/releases sync).
   - `oak upgrade` from a 0.101.0 install fetches v0.102.0 via GitHub
     /releases/latest and the minisign verification passes.
   - `oak --version` reports 0.102.0; `cargo install oakvcs-cli` resolves
     0.102.0.

Follow-up (not release-blocking): the agent-skill command reference
(`cli/src/commands/skill/oak-vcs/references/commands.md`) states it was
compiled from v0.101 `--help` output; regenerate it against 0.102.0 in a
separate change so its provenance line stays honest.
