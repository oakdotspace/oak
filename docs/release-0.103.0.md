# Oak 0.103.0

Oak 0.103.0 is an additive client and integrity release. It expands
reproducible acquisition, checkout-free diagnostics, and durable local change
handoff without adding an apply command or a new remote archive protocol.

## Highlights

- `oak clone --branch NAME --expected-head FULL` binds acquisition to one
  authorized open branch and exact head on capable servers. Supported servers
  also materialize an unpinned selected branch directly, including sparse
  clones, without a redundant generic branch switch.
- Publication now walks the complete commit graph and fixed servers reject
  commits whose external parent or merge parent is absent. Sparse publication
  and materialization preserve full out-of-cone identities without treating
  unavailable bytes as empty content.
- `oak change capture --json` records a complete immutable local change
  descriptor and distinct capture provenance without moving refs. `oak change
  export CAPTURE_ID --output FILE` writes a verified, self-contained,
  owner-private archive without overwriting an existing path or contacting a
  remote.
- New read-only workflows improve agent and operator evidence: pinned file
  inspection, workspace inventory, remote branch analysis and merge previews,
  and exact-run CI observation and waiting. Explicit CI trigger, rerun, and
  cancellation commands report exact receipts or uncertainty; cancellation is
  exact-run and exact-commit gated on capable servers, while legacy rerun keeps
  its documented weaker identity contract.
- CLI feedback transitions support explicit revision preconditions on capable
  servers, preserving the caller's token, refusing confirmed stale writes, and
  reporting ambiguous outcomes without automatic retry.

## Compatibility and rollout

Deploy the compatible fixed oak.space server before distributing this client,
following [release readiness](release-readiness.md). Do not infer server support
from a version string: capabilities and request-bound proofs select the new
paths. Legacy and self-hosted servers retain only the bounded compatibility
paths they can prove. Large or sparse operations may stop before mutation and
request a server upgrade when a legacy server cannot prove the required object
or scope invariants.

This release prevents new dangling-parent publication when paired with the
fixed server. It does not repair the previously published missing-parent source
incident or claim customer-repository recovery; that work remains a separate,
explicit operation.

## Release proof

Before dispatching either release workflow, run `make release-proof` from a
complete source checkout. The proof requires workspace/core/dependency version
lockstep, a release build, workspace tests, all-target clippy, and a core crate
publish dry run. Until the matching core version is published, it uses a local
release CLI build as the publish-workflow proxy; once core is index-visible, it
runs the CLI publish dry run too. The stable CI workflow separately requires
artifact signing, macOS signing and notarization, exact staged-byte
verification, and promotion before the GitHub draft becomes public.

Publish crates in dependency order: `oakvcs-core` first, then `oakvcs-cli` after
the matching core version is visible on the crates.io index.
