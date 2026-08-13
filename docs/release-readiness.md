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
