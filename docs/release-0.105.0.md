# Oak 0.105.0

Oak 0.105.0 is a minor release of the `oak` CLI. It adds new commands:

- remote log
- remote endpoint diff
- fenced close of many branches
- verified local workspace inventory
- clone seeded from a local checkout

It also fixes two ways a command could drop local-only commits: `oak pull`
against a loopback `oak serve`, and `oak close --remote`. JSON changes are
additive within `schema_version` 1. There is no protocol change. Every client
change works against the currently deployed oak.space.

Released source: oak/oak main at the version-bump commit (notes cover main
through `6b0fbcbe`). The previous stable release, v0.104.0, was built from
oak/oak main `b1675ea6` (GitHub mirror commit `2d298131`).

## Local commits are never silently discarded

### `oak pull` (fb-529)

Plain `oak pull` against a loopback `oak serve` could throw away unpushed
local commits. The sequence was: push, commit locally, then pull. The pull
exited 0, reset the branch to the server head, deleted the committed file from
the worktree, and parked nothing. This affected 0.103.0 and 0.104.0 and was
0.104.0's known issue. **Hosted oak.space was never affected**: it answers such
pulls with 409 and the client converges.

- **Containment check before any write.** Pull now checks that the new head of
  the selected branch contains the local tip, and it does this before writing
  to the worktree or any ref.
- **If the new head doesn't contain the local tip,** pull converges exactly as
  for a hosted 409:
  - **Already anchored:** nothing moves ("local commits not pushed yet").
  - **Diverged:** one re-parent commit, with the old tip as its merge parent.
  - **Overlapping edits:** the existing `--continue` / `--abort` conflict flow.
  - **Detached HEAD or not the current branch:** refused with exit 5.
- **Missing local ancestry** counts as "would strand local work", so the check
  fails toward keeping local commits.
- **Other branch heads carried by a response never move backwards.** They move
  only when the new head contains their local head.
- **`--force`** is still the only way to replace the tip. It first parks the
  old tip as `<branch>.orphaned-<ts>`, in the same transaction.
- **Dirty tree.** A diverged pull over uncommitted edits refuses with exit 4
  before any ref or file write. This applies to both the 200 path and the
  hosted-409 path. Before, the 409 path converged over a stale tree, so the
  next commit could silently revert the remote side. An already-anchored pull
  with a dirty tree still succeeds, because nothing moves.
- **Crash-safe re-parent.** The converged tree is written first and the branch
  ref is published last.
- **Merge-parent-only containment.** When the server head contains the local
  tip only through a merge parent, the client proves it with hash-verified
  ancestor rows. It then fast-forwards instead of minting an empty re-parent
  commit, and reports `local_commits: contained_by_remote`. Unverifiable rows
  or an exhausted budget fall back to the safe re-parent.
  - **Cost:** until batching lands, this walk can make **up to 256 sequential
    single-hash `commits/info` requests**, about 47 s in the worst case on
    hosted. Its bound is 20,000 visits. It is always safe: when the budget runs
    out, it falls back to a re-parent.
- **`oak pull --json` receipt** (append-only):
  - `local_commits`: `kept_ahead`, `reparented`, `conflict`, `parked` or
    `contained_by_remote`
  - `divergent_remote_head`
  - `parked_ref` (under `--force`)
  - `branch_update` / `convergence: kept_local`
  - next-command suggestions: `oak push --json` for kept-local commits, and
    `oak diff <parked> --stat` after `--force`

### `oak close --remote` (pre-existing in 0.104.0 and earlier)

Closing a remote branch mirrored the remote metadata locally and reset the
local branch pointer to the remote head. A local branch that was ahead of the
remote lost its local-only commits from the branch, and their files showed up
as uncommitted additions. This affected plain `oak close --remote NAME` too.

- The local head is now only fast-forwarded, after its ancestry is proven with
  a bounded walk.
- A local head that is ahead or diverged is kept, and the close updates
  metadata only.
- Receipts report `local_head.relation`, `local_ahead: N` and
  `local_only_commits: true`.

## Loopback `oak serve` parity with hosted

- `GET /pull` now returns **409 like hosted** in two cases, when the target
  branch has a head: an unpublished `since`, and a rewritten branch. `force`
  skips both checks.
  - Documented deviation: for a branch the server has never seen, serve
    answers 200 with no commits. Hosted answers 409. This is safe because
    nothing ships and the client's head doesn't move.
- **New hosted-shaped `POST /blobs/info`.** It returns published blobs only,
  and unknown hashes are omitted. Re-parent seeds, mounts and remote review
  now hydrate through the normal bounded path against a loopback Serve. That
  includes released clients, which previously failed with "legacy server
  requires a branch-scoped pull". The client-side whole-branch fallback is
  removed, so hosted pays no extra request.
- **`oak serve --port 0`** binds a free port and prints it.

## New commands and flags

### Remote inspection (lane W4a)

- **`oak log --remote [--branch B] --json [-n N] [--from HASH]`.** A bounded,
  read-only first-parent walk from one pinned head.
  - Page size is 20 by default, 200 at most. The walk is limited to 64 MiB
    and 60 s.
  - Each row is hash-verified before it is reported. Merge commits carry
    `merge_source_branch`, and rows carry parsed `feedback_refs` (fb-N).
  - Truncation is explicit, and `next_command` continues exactly with
    `--from`. Nothing is persisted.
  - An unknown starting commit, or a budget exhausted before it is read, is a
    typed error (exit 6).
  - Cost: one serial `commits/info` round trip per commit.
- **`oak diff --remote OLD NEW (--json [--hunks] | --print)`.** Diffs two exact
  remote commits.
  - Both must be full hashes; prefixes and names are refused before any
    request.
  - Only those two commits' manifests are fetched. With hunks or print, the
    returned files' content is also fetched through the verified raw path.
  - The result is document kind `remote_endpoint_diff` with `evidence_kind:
    "snapshot"`, an acquisition receipt, and per-file omission reasons. No refs
    are written.
  - `--max-bytes` is not accepted with `--print`.

### Evidence labelling (lane W4a)

- **`--line-numbers`** on `oak diff` and `oak branch diff`.
  - With `--json --hunks`, each file adds structured `hunks[]` with old/new
    line numbers per line.
  - With `--print` (including `oak branch diff --remote --print`), each line
    is prefixed with its line numbers.
  - The hunks are parsed from the already-budgeted patch, so they never
    disagree with it.
- **`evidence_kind` and `basis`.** Branch, remote-branch, endpoint and
  `--branch` diff JSON carry a document-level `evidence_kind`: `snapshot`,
  `contribution`, `predicted_merge` or `unavailable`.
  - Snapshot rows carry a `basis`: `snapshot_absence`, `snapshot_presence` or
    `snapshot_difference`. A file that exists only on main is no longer
    presented as a contribution deletion.
  - Branch review reports `snapshot`; its merge preview reports
    `predicted_merge` or `unavailable`.
- **`ancestry_diagnostic`.** Branch diff and review JSON, local and remote,
  explain ancestry gaps or disjoint histories. The fields are `resolution`,
  `proven_base`, `missing`, `link_gaps` and `below_proven_base` (`true`,
  `"unknown"` or `false`). It only reads the existing certification result;
  the resolver is unchanged. Cost: the local diagnostic adds a second ancestry
  walk.

### Branch operations and workspace inventory (lane W4b)

- **`oak close --remote A@FULL_HEAD B@FULL_HEAD ... --reason R --json`.**
  Closes up to 100 remote branches without a checkout.
  - Every target must pin a full 64-hex head. Mixed fenced/unfenced targets,
    duplicates and `main` are refused before any request.
  - Each branch closes on its own, so a close is atomic per branch. There is
    no batch rollback.
  - **The fence is read-then-close, with no server precondition.** The branch
    listing is re-read just before each close. On head drift the row is
    `refused_head_moved` and nothing is sent for that branch.
  - Neither hosted nor loopback close takes an expected head. So a push that
    lands between the read and the close is **not prevented**. It is
    **detected** by a final listing and reported as `closed_after_head_moved`
    (`fence_verified: false`), with exit 5 and advice to inspect and reopen.
  - The receipt says so: `fence.kind: read_then_close`, `server_precondition:
    false`. A true compare-and-swap needs a server change.
  - Row outcomes: `closed`, `closed_after_head_moved`, `already_closed`,
    `refused_head_moved`, `not_found`, `rejected`, `read_failed`,
    `unknown_outcome`. An ambiguous outcome is never retried.
  - Exit codes:
    - 0 only when every branch is confirmed closed at its pin, or was
      already closed.
    - 5 on refusals, rejections, or a lost race.
    - 6 when any outcome or fence re-check is unconfirmed.
  - A single `oak close NAME` is unchanged.
- **`oak space inventory --verify-local [--include-ci]`.** Reports per
  checkout:
  - `working_tree` (clean/dirty, with a change count)
  - `unpublished_commits` across **all** local branches, with a per-branch
    basis (`local_push_receipt`, `no_remote_configured` or
    `unmerged_commits_upper_bound`)

  How it inspects:
  - Each checkout is read under a non-waiting inspection lock, from a
    read-only database. The stat cache is not written.
  - It never reaps another process's lock.
  - A held or unavailable lock, a mount, a git checkout, or an in-progress
    clone/merge/sync is reported as `local_verification: "unverified"` with a
    reason.

  `--include-ci` adds exact-head CI state from one bounded runs listing per
  repository: at most 20 repositories, 20 s each, 60 s overall. It never
  claims a checkout is safe to clean up.
- **`oak clone ORG/REPO DEST --from LOCAL_CHECKOUT`.** Reuses content chunks
  from an existing local checkout of the same repository and origin.
  - Every reused chunk is re-hashed.
  - Commits, trees, descriptors, refs and the selected head still come from
    the server. No credentials, refs, metadata or dirty files are copied.
  - Source problems only forfeit the reuse. A source whose identity now names
    another repository stops the clone and removes the partial destination.
  - The receipt adds a `seed` block, and
    `observed.chunks.seeded_from_local_unique`.
  - Only the initial pull phase is seeded. The seed cannot help when the
    server itself lacks a blob's chunk mapping.

## Compatibility and behaviour changes

- `oak pull` can now return:
  - **exit 4** on a diverged pull over a dirty tree;
  - **exit 5** for a detached or non-current branch that would strand
    commits.
  
  It also keeps local commits (`kept_local`) where 0.104.0 against a loopback
  Serve would have replaced them. Scripts that relied on a plain pull
  replacing local history must use `--force`, which parks the old tip.
- A loopback `oak serve` now answers 409 where it used to answer 200 with full
  history. Released clients converge against it.
- `oak close --remote` no longer moves a local branch that is ahead or
  diverged.
- JSON changes are additive within `schema_version` 1.
- No oak.space deploy is needed.

## Known limitations carried forward

- **Push self-heal over a dirty tree.** `oak push`'s self-heal after a server
  reseed can carry a dirty tree across the re-parent, and a later commit can
  revert main's changes. This is pre-existing. It does not discard commits.
  It needs its own change.
- **Close-many has no server compare-and-swap.** Races are detected, not
  prevented (see above).
- **Merge-base tie-break.** When several best common ancestors exist, the
  client tie-break (walk order) and the hosted merge (newest timestamp) can
  pick different bases. This is pre-existing.
- **`oak pull` after an incomplete ancestry backfill.** Pull continues and
  stops at the resolver instead of writing a wrong commit. Transient
  ancestor-fetch errors are still reported as "incomplete".
- **`oak merge --json`** can print progress lines on stdout before the JSON
  when it refreshes main.
- **First pull/fetch cost.** The first `oak pull` / `oak fetch` in a shallow
  worker, and any refresh after main moves, still costs O(history) round trips
  (about 3 minutes on oak/oak).

## Release proof and publication

Run `make release-proof` from a complete source checkout of the release commit
(see [release readiness](release-readiness.md)). Then dispatch the "Release
(staging)" workflow. It builds, signs, stages, verifies, promotes, and flips
the GitHub draft live. This release ships through GitHub Releases, oak.space
and `oak upgrade`.
