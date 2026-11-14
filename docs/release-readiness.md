# Release Readiness

Stable releases build and publish on Oak CI's macOS release runner; see
[release-on-oak-ci.md](release-on-oak-ci.md).

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
smoke when no key is available. Release CI still fails closed: `.oak/workflows/release.yml`
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
only head in one ordinary request. For this first-publication bridge, an
unknown legacy server, a non-empty repository, or a graph with external
parents still stops before mutation. This
keeps 500+ commit imports and large initial snapshots available at minute zero
without exposing intermediate history.

The same capability carries one narrower bridge for a repository that already
exists: when the only totals above the ordinary envelope are the
remote-missing blob count (128) or their declared bytes (64 MiB), the client
uploads those blobs first and then publishes through the unchanged ordinary
path. Each preload is an ordinary `/push` with no commits, no trees and no
branch metadata, guarded by the repository head: the server stores verified
content-addressed blobs and touches no branch row, description, head or
publication event. A request holds at most 128 blobs, 64 MiB and 10,000 chunk
references; one blob above 64 MiB travels alone as chunk references after its
chunks went through the chunk endpoints. Trees and commits are never
preloaded, so the single publication request still carries the whole commit
graph and its one head CAS, and a commit count above 500 still requires
staged-v1. An interrupted preload leaves only unreferenced blobs; the retry
asks which blobs the server holds and sends the rest. When staged-v1 is ready
it is always selected instead.

The client treats this request shape as part of `headless_preload_v1`, and the
server must keep accepting it on non-empty repositories for callers with write
access. A server that deterministically refuses a preload request (a 4xx
status such as 403 for an external contributor, who may push only a branch of
their own and so cannot send a branchless request) gets no retry advice and no
login prompt: the push stops with `push_capability_unavailable`, saying that
no branch state was changed and that the blobs already uploaded are
unreferenced. Transient answers are not that: 409, 423 and 425 (the server's
"changed during push; retry" family), 408/429, 5xx, and a repository head that
moves under every attempt are resent within the preload and, when they
persist, reported as an ordinary retryable server error, which `oak watch`
retries with its normal backoff. 401 stays `auth_required`.

A push that needs staged-v1 on a server offering neither stops before mutation
with `push_capability_unavailable` (exit 6), naming each measured total and its
limit. `oak push --plan --json` reports the selected `transport` and the same
refusal under `publication_blocked`, and `oak watch` parks that head
(`push_blocked`) instead of resending it. Routine large working trees with small
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
