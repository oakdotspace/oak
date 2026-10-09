# Release Readiness

Run the non-mutating release proof before dispatching release or crate-publish
workflows:

```sh
make release-proof
```

The proof runs the source gates (`cargo build --release`, workspace tests,
clippy), verifies `oakvcs-core` with `cargo publish --dry-run`, and then checks
the CLI the same way the `publish-crates` workflow does.

`oakvcs-cli` depends on `oakvcs-core` by version. Before a real release,
`oakvcs-core <version>` is not yet visible on crates.io, so a local
`cargo publish --dry-run --package oakvcs-cli` fails even when the workflow is
correct. The proof therefore uses `cargo build --release --package oakvcs-cli`
as the dry-run proxy until the matching core crate exists on crates.io. After
core is published, the same proof automatically runs the real CLI publish dry
run.

For a launch/signing gate, make the minisign smoke mandatory:

```sh
REQUIRE_RELEASE_SIGNING=1 \
MINISIGN_SECKEY=/path/to/minisign.key \
MINISIGN_PASSWORD=... \
make release-proof
```

The signing smoke also proves KEY AGREEMENT: the smoke signature must verify
against the `RELEASE_PUBKEY` baked into `cli/src/commands/upgrade.rs`
(extracted from source as the single source of truth). A seckey that signs
fine but is the wrong keypair fails the proof — artifacts it signed would be
refused by `oak upgrade` and first-run `oak mount` installs.

Without `REQUIRE_RELEASE_SIGNING=1`, the proof warns and skips the signing
smoke when no key is available. Release CI still fails closed: `release-staging.yml`
requires `MINISIGN_SECKEY`, signs every client-installed artifact, and uploads
the signed bundle as a workflow artifact on dry runs.

Publish order for crates.io remains:

1. Publish `oakvcs-core`.
2. Wait for cargo's publish command to finish index propagation.
3. Publish `oakvcs-cli`.

Do not change the CLI dependency to avoid this ordering. The versioned
dependency is what makes the published CLI resolve the exact matching core
crate outside the workspace.

## Content-integrity protocol rollout

Protocol-hardening releases must deploy the compatible fixed server before the
new client. Do not advertise or release this client as a universal
client-first bridge: a released server without `/commits/info` cannot prove
external commit edges, and its full-branch fallback for missing `/blobs/info`
is deliberately bounded. A large legacy sparse repository can therefore stop
before mutation and ask for the server upgrade. The released v0.102.1 server
also returns `branch: null` for the first push of a new branch, so it cannot
hydrate a locally missing, repo-deduplicated blob through that branch; this is
another explicit fixed-server-first, no-mutation stop.

Use this order:

1. Deploy the fixed server with enforcement off and new capabilities closed.
2. Verify `/commits/info`, `/blobs/info`, ordinary old-client push/pull, and all
   receipt/publication readiness gates on the deployed server.
3. Release the new client. Its legacy paths remain available only inside their
   explicit safe resource and atomicity envelopes.
4. Advertise staged/receipt capabilities only after their complete readiness
   predicates pass, then enable strict enforcement last.

While staged capabilities are closed, the fixed server's exact phase-one
`ordinary_bootstrap_protocol: headless_preload_v1` capability permits one
narrow onboarding bridge: a self-contained first publication into an empty
repository may preload immutable blobs and trees in
bounded headless ordinary requests, then publish its complete commit graph and
only head in one ordinary request. An unknown legacy server, a non-empty
repository, or a graph with external parents still stops before mutation. This
keeps 500+ commit imports and large initial snapshots available at minute zero
without exposing intermediate history. Routine large working trees with small
publication deltas remain ordinary; staged planning and upload selection use
metadata presence. Locally missing ordinary content follows the server's
explicit `content_receipt_enforcement_required` capability. Once staged-v1 is
selected, every blob reuse query sets `require_verified_receipts`, the same
strict mapping/blob/child-receipt predicate used by finalization. Neither path
spends generic live byte-verification quota; receiptless staged content enters
the bounded upload/proof path before publication.

If either exact edge proof or bounded legacy hydration is unavailable, the
client must return actionable upgrade guidance and must not create a repo,
upload content, stage objects, or advance a branch head.

Do not infer remote commit closure merely because a cross-branch parent is in
the local ancestry. The pinned v0.102.1 executable gate demonstrates that the
released server accepts a commit whose external parent is absent. Until the
fixed server answers `/commits/info`, these external-edge pushes intentionally
fail closed rather than risk publishing a corrupt graph.
