# Merge safety: four-tree semantics (fb-105)

fb-105 / fb-7d2bbb: merge previews reported "clean" merges whose predicted
result deleted current-main-only files the branch never touched. Four branch
recreations during the 0.102.0 release work trace back to this shape. This
document defines the contract that prevents it: every merge prediction is
judged against **four trees**, and a prediction that destroys target-side
state is a **merge invariant violation** — a blocking verdict, not a caveat.

## The four trees

| Tree | Meaning | Authoritative source |
|------|---------|----------------------|
| **FORK BASE** | What the branch forked from | `merge_lineage_evidence.fork_point`: the first common ancestor of the branch head and the target head (`cli/src/commands/review.rs::branch_comparison` → `first_common_ancestor`), resolved independently of the base the merge machinery uses |
| **BRANCH HEAD** | The branch's contribution | `resolve_effective_head(branch)` and its manifest |
| **CURRENT TARGET HEAD** | Where the target is *now* | A live remote fetch (checkout-free `--remote` review) **bound to the exact `(target, head)` pair it fetched** — a fetch of a different `--against` branch or of a different head than the one assessed is an authority mismatch and does not certify — or local data verified fresh (`MetadataKey::MainLastCheckedAt` within 60 s). Anything else is *stale local* data: detection still runs, but it must say so and refuse to certify — it never silently classifies against a stale base |
| **PREDICTED RESULT** | The three-way merge output | `three_way_merge_manifests(base, branch, target)` clean entries + content-merge-resolved entries (`merge_preview_for_branch`) |

## Per-path classification

For every path where the PREDICTED RESULT differs from the CURRENT TARGET
HEAD (predicted conflicts excluded — they are surfaced separately and never
land silently), compare each side against the FORK BASE:

1. **`invariant_violation` — hard fail.** The branch did not touch the path
   since the fork, yet the predicted result does not preserve the target's
   current entry (it disappears or reverts). This is the fb-105 case:
   target-side work would be destroyed by a merge the branch never earned.
   Includes the phantom case where *neither* side changed the path but the
   result differs anyway.
2. **`branch_change` — valid.** Branch-only change (edit, add, or
   intentional deletion) while the target stayed unchanged since the fork.
   No warning.
3. **`overlap` — warning.** Both sides changed the path since the fork and
   the prediction reconciled them (content merge or identical change).
   Reported for acknowledgement; does not block.

The classifier is pure (`cli/src/commands/merge_safety.rs`); it does no I/O
and is a redundant safety net over the whole prediction pipeline: whatever
base the merge machinery resolved, the classification recomputes expectations
from the lineage fork point and verifies the predicted output path by path.
It is a single linear pass — each manifest is indexed once into a path→
signature table, then every distinct path is visited once (O(N) over summed
entry counts; a per-path `Manifest::get` would be quadratic).

## CLI surfaces (detection)

`oak merge --dry-run --json` and `oak branch review [--remote]
--merge-preview --json` gain:

- per-file `merge_safety` on `changed_files` entries
  (`"invariant_violation" | "branch_change" | "overlap"`; absent elsewhere);
- top-level `invariant_violations: [paths]` — the **canonical** field for
  this payload and, for a CLI-local classification, the **complete** list of
  paths that classification judged violations (`[]` = checked and clean;
  absent = classification could not run). Completeness is a property of the
  local path, not of the field name — the server API's field of the same
  name is capped and filtered; see
  [Same field name, different guarantee](#same-field-name-different-guarantee);
- a `merge_safety` block: `certified`, `verdict`
  (`"safe" | "do_not_merge" | "uncertified"`), `uncertified_cause`
  (`"stale_local_target" | "fork_base_unavailable" |
  "target_authority_mismatch"`), `target_head_source`
  (`"live_remote_fetch" | "fresh_local" | "local_branch_head" |
  "stale_local" | "live_fetch_mismatch"`), `assessed_target` plus the
  fetched `(target, head)` binding when a live fetch happened, the three
  head hashes as evidence, and bounded reporting: `invariant_violation_count`
  / `invariant_violations_sample`, `overlap_count` / `overlap_paths_sample`,
  and `reasons` (each capped at 5 entries plus a count — the full path list
  appears exactly once, in the top-level `invariant_violations`).

### Same field name, different guarantee

`invariant_violations` is complete **for the classification that produced
it**, and the two producers do not offer the same guarantee:

- **This CLI (local classification).** `oak` classifies the whole predicted
  tree from local data, caps nothing and filters nothing, so every path it
  judged a violation is in the list. Reading it as exhaustive is correct.
- **oakspace's server-side merge-preview / checkout-free review API**
  (`docs/checkout-free-review-api.md`). The same-named response field is
  explicitly **not** complete: it is capped (10,000 paths) and filtered by
  path policy, so paths the caller may not read are omitted. It carries an
  `invariant_violations_truncated` flag saying when the list is short of the
  real set, and a server-issued `violations_digest` that binds the
  **complete** pre-cap, pre-filter sets (see
  [Server-side enforcement](#server-side-enforcement-design--not-in-this-changeset)
  below).

So: never treat a *server* response's `invariant_violations` as exhaustive,
and never derive anything binding from it — the digest, not the list, is the
artifact that covers the whole classified set. A client that saw an empty or
short list has not seen proof there are no violations; the verdict fields and
the truncation flag are what it must gate on.

Verdicts are enforced fail-closed:

- **Violation** — blocking. `oak merge --dry-run` exits **5**
  (`MergeInvariantViolation`) after printing its JSON; review triage returns
  `recommended_action: "do_not_merge"` with `vcs_merge_safe: false` and a
  bounded reason naming the files and tree evidence.
- **Uncertified** — never silently treated as safe. `oak merge --dry-run`
  exits **7** (`MergePredictionUncertified`, a distinct code so automation
  can tell "certified safe" from "unable to certify" without parsing JSON;
  the JSON payload still prints first, and dry-run errors go to stderr so
  stdout stays a single JSON document). Review triage returns
  `vcs_merge_safe: false` with reason `merge_safety_uncertified: <cause>`,
  never `validate_then_merge`, and recommendations steer through `oak fetch`
  / `--remote` review — never to `oak merge`.
  **"Uncertified" includes "no prediction at all."** When no prediction
  could be produced from local data (`prediction_available: false` — no
  local parent head, incomplete ancestry, missing manifests or blobs; the
  ordinary state of a partially-fetched repo), the four-tree classification
  never ran, so there is no `merge_safety` block and no
  `invariant_violations`. That is the *weakest* possible evidence, not the
  strongest: `oak merge --dry-run --json` exits **7** with cause
  `prediction_unavailable`. Exit 0 is reserved for a positively certified
  prediction and nothing else.
- **Safe** — certified against an authoritative target head; exit 0.

Everything that consumes a preview gates on **positive certification** —
`merge_safety` present *and* `certified: true` *and* `invariant_violations`
present and empty — rather than on the absence of an explicit uncertified
verdict. Absence of a verdict is absence of evidence; only an affirmative
certification unlocks a merge-executing recommendation.

`--json` commands always emit exactly one JSON document. The merge dry-run
prints its verdict payload and then exits non-zero with the error text on
stderr, so no error envelope follows it; a dry-run that fails *before*
printing a payload (not a repository, unresolvable branch, rejected flag
combination) still emits the standard JSON error envelope on stdout.

## Server-side enforcement (design — not in this changeset)

Detection in the CLI is advisory by construction: it runs against whatever
target head the client can see. Enforcement must live where the merge
actually happens, in oakspace's squash-merge path (`finalize_squash_merge`),
and ships as a **separate deployable** from CLI detection so either side can
roll forward or back independently.

At squash-merge time the server recomputes the same four-tree classification
against **the server's own current target head** — the one head that cannot
be stale — using the branch's fork point from the server-side commit graph
and the same pure classifier (`oak_core`-adjacent code shared or ported), with
**full privileges and no path-policy term** (see
[The privileged classification model](#the-privileged-classification-model-restricted-paths)):

- Any `invariant_violation` → reject the merge (409-family, naming the paths
  and the three head hashes), regardless of what any client preview said.
- `overlap` paths → require an acknowledgement. At **classification
  granularity, not per path**: the acknowledgement is over the whole
  classified set (which `violations_digest` binds), never a per-path opt-in
  the client assembles — response overlap lists are sampled (≤ 5) and there
  is no full overlap array to opt in against.

### The acknowledgement contract

To make overlap/violation overrides race-safe, an acknowledgement is bound to
the exact `(branch_head, target_head)` pair it was computed for *and* to a
**keyed** digest of what was classified at that pair. The values are stated
byte-exactly because the server mints and verifies the digest while the
client derives the token: two repositories must produce identical bytes.

**oakspace's `docs/checkout-free-review-api.md` §C is normative** for this
contract. Where this document states any of it loosely, it defers to that
text; nothing here may be read as a variant of it.

**Canonical framing (`F`) — how `‖` is defined in every preimage below.**
Each preimage is a concatenation of **length-prefixed** fields, never a bare
string join:

```
F(x)  = LE_u32(byte_length(x)) followed by the bytes of x
a ‖ b = F(a) followed by F(b)
```

The 4-byte little-endian length in front of every field makes the
concatenation injective: no choice of field values can make two different
term lists produce one preimage. Scalar terms (`schema_version`,
`classifier_version`) are **ASCII decimal with no padding**; an absent/null
term is the **empty byte string** (`F("")` = four zero bytes), distinct from
any present value. Commit hashes and `violations_digest` enter preimages as
**32 RAW bytes (hex-decoded), never their hex text** — hashing the hex text
is the single easiest way for two implementations to disagree, so it is
called out rather than left to taste.

**The named hashes.** `H` is not a specification and is not used here. The
outer hash is **unkeyed BLAKE3-256** with the default 32-byte output;
`violations_digest` and `ack_key_id` are **BLAKE3 in keyed mode**, same
32-byte output. Wire encodings: `violations_digest` and
`acknowledgement_token` are those 32 bytes as **64 lowercase hex**
characters (`[0-9a-f]`); `ack_key_id` is 8 bytes as **16 lowercase hex**
(`""` when no key is configured). Implementations MUST NOT emit uppercase,
base64, or a truncation.

```
violations_digest     = BLAKE3_keyed_256(K_ack,
                            "oak.merge-ack.v1"          // ASCII domain string
                          ‖ schema_version              // ASCII decimal
                          ‖ classifier_version          // ASCII decimal
                          ‖ repo_id                     // ASCII, canonical repos.id text
                          ‖ branch_head                 // 32 raw bytes (hex-decoded)
                          ‖ target_head                 // 32 raw bytes; empty if none
                          ‖ classified_sets)            // canonical text, below

ack_key_id            = hex(BLAKE3_keyed_256(K_ack, F("oak.merge-ack-key-id.v1"))[0..8])
                                                        // framed like every other preimage,
                                                        //   even though it is a single field

acknowledgement_token = BLAKE3_256(
                            "oak.merge-ack-token.v1"
                          ‖ branch_head                 // 32 raw bytes
                          ‖ target_head                 // 32 raw bytes; empty if none
                          ‖ violations_digest)          // 32 RAW bytes, not the hex text
```

**Canonical wire field name: `acknowledgement_token`.** This document
previously spelled the field `ack`. That spelling is **prose shorthand
only** and must not appear as a wire field name, a query parameter, or a
JSON key. The abbreviation stays legitimate everywhere it is *not* that
field: `ack_key_id`, `K_ack`, `OAK_MERGE_ACK_KEY` and the domain strings
`"oak.merge-ack.v1"` / `"oak.merge-ack-token.v1"` are unchanged — they name
a key epoch, a secret, an env var and two constants, none of which is the
token on the wire.

**`classified_sets` — canonical serialization.** The normative definition
lives in `docs/checkout-free-review-api.md` §C and **this document defers to
it**. Summary, for orientation:

```
classified_sets = concat over paths, ascending by raw escaped-path bytes, of
                  <path> 0x09 <class> 0x0A
                  class ∈ { "invariant_violation", "overlap" }   // never branch_change
                  path escapes: "\" → "\\", TAB → "\t", LF → "\n"
                  trailing 0x0A on every line, including the last
                  empty set ⇒ the empty byte string (a digest is still computed)
```

One line per classified path, and a path appears at most once (a path has
exactly one class). The line is plain byte concatenation — `<path>`, `0x09`
(TAB), `<class>`, `0x0A` (LF) — **not** the framed `‖`; the whole
serialization enters the digest preimage as one `F()`-framed field.
`<class>` is the classifier's own snake_case ASCII spelling and only ever
`invariant_violation` or `overlap`: `branch_change` is **excluded**, because
it is the "valid, nothing to acknowledge" class and including it would make
every ordinary branch change alter the digest. `<path>` is the manifest path
as UTF-8 bytes with **exactly three escapes** (`\` → `\\`, TAB → `\t`,
LF → `\n`), no other byte escaped and no quoting. Ordering is `memcmp` over
the raw bytes of the *escaped* path — not locale collation, not code-point
order after decoding, not case-folded.

The remaining terms and rules:

- `classified_sets` covers the **COMPLETE pre-sampling, pre-cap violation and
  overlap sets, including restricted paths** — computed with **full server
  privileges and no path-policy term** (see
  [The privileged classification model](#the-privileged-classification-model-restricted-paths)
  below; a policy-filtered classification is a *different function*, not a
  redacted view of the same one, and its digests could not agree). It is
  deliberately *not* the
  path list a client saw: every response path list is bounded — the samples
  (`invariant_violations_sample`, `overlap_paths_sample` and `reasons`) are
  capped at 5 (see "CLI surfaces (detection)" above), and the server's own
  `invariant_violations` is capped at 10,000 paths and path-policy filtered,
  flagging the shortfall with `invariant_violations_truncated` (see "Same
  field name, different guarantee" above). Any client-visible list may
  therefore be a truncation of the real set, and hashing it would let an
  acknowledgement cover strictly less than it appears to. The earlier
  `ack = H(branch_head ‖ target_head ‖ paths)` shape is **superseded**: a
  path list is truncatable, policy-filtered, and forgeable by a caller who
  cannot see the restricted part of the set.
- **`repo_id` is a MAC preimage term** — and deliberately **not** repeated
  in the token preimage. `K_ack` is deployment-wide, so without a repo term
  a public fork and its private original sitting at the same head pair would
  produce equal digests exactly when their classified sets match: a
  cross-repository equality oracle. `repo_id` closes it, so digests are
  incomparable across repositories and one minted in repo A can never
  acknowledge a merge in repo B. The token **inherits that repo scoping
  transitively through the digest**, so repeating the term there would buy
  nothing and would widen the surface the two documents must keep identical.
- **`classifier_version` is a distinct term from `schema_version`.**
  `schema_version` covers the **wire shape**; `classifier_version` covers
  the **semantics** of `classify_predicted_merge`. A change to what the
  classifier decides — fb-117's commit-touch evidence is the one already on
  the roadmap — bumps **`classifier_version`, not `schema_version`**, so a
  preview taken under the old rules can never acknowledge a classification
  made under the new ones. Overloading one scalar would force a
  wire-compatibility break every time a classifier heuristic improved, and
  would equally let a wire-only field addition invalidate acknowledgements
  for no semantic reason. Both are terms of the digest, and either one
  moving is sufficient to invalidate outstanding acknowledgements.
- **Zero-violation case — the digest is computed normally, never `null`.**
  When both classified sets are empty, `classified_sets` is the empty byte
  string, the MAC runs over the preimage unchanged, and the field carries a
  perfectly ordinary 64-hex value. Implementations MUST NOT return `null`,
  omit the field, or substitute a sentinel for the empty case: that would
  reintroduce the oracle at 1-bit resolution — any caller could read "does
  this branch have *any* invariant violation or overlap?" straight off the
  field's nullity — and it keeps the ack flow uniform, since a `safe`
  verdict still yields a bindable token. **The `null` rule is a property with
  two permitted causes, not one cause** — this document's earlier "the one
  and only reason the field is `null` is that no key is configured" wording
  is **superseded**; see **What `null` means downstream** below. What matters
  here is only that an empty classified set is not one of those causes.
- **`ack_key_id` is DERIVED from `K_ack`, never an operator-set label.**
  `hex(BLAKE3_keyed_256(K_ack, F("oak.merge-ack-key-id.v1"))[0..8])`, 16
  lowercase hex, and `""` when `OAK_MERGE_ACK_KEY` is unset. Being a pure
  function of the key, a rotation *cannot* keep the old id — an operator-set
  label rotated without a bump would leave outstanding preview `ETag`s
  validating bodies the server can no longer produce, which is the exact
  failure the term exists to prevent. It uses a **different domain string**
  from the digest, so a key id can never collide with, or be substituted
  for, a digest preimage, and it is safe to publish: eight bytes of a MAC
  over a fixed public constant reveals nothing about `K_ack` and cannot test
  a guessed classified set. `""` is not a valid 16-hex value, so "no key" is
  distinguishable from every real epoch — which is exactly what makes
  `ack_key_id` **the discriminator between the two permitted causes of a
  `null` digest** (below): `""` for "no key configured", a real 16-hex epoch
  for "the classification did not complete".
- `K_ack` is a 32-byte **server-side secret** (`OAK_MERGE_ACK_KEY`, 64 hex
  chars in the server env). It never leaves the server. **Why keyed rather
  than a bare hash:** the set deliberately includes restricted paths, so an
  *unkeyed* digest would be an offline oracle — a caller knows its own
  visible violations, guesses a candidate restricted path, hashes
  `visible ∪ {guess}` and compares, leaking exactly the path names path
  policy exists to hide. Keying makes the value unguessable and
  non-invertible without `K_ack`. When the key is unset, `violations_digest`
  is `null` and **no acknowledgement can be minted at all** — the override
  path fails closed rather than degrading to an unkeyed hash anyone could
  compute. Rotating the key invalidates outstanding acknowledgements; the
  caller re-previews and re-acks.
- The pins (`schema_version`, `classifier_version`, `repo_id`,
  `branch_head`, `target_head`) live **inside** the MAC preimage, so they
  are authenticated rather than merely concatenated alongside an
  independently-computable hash. Two different head pairs never share a
  digest even when their classified sets are identical, and neither do two
  repositories.
- The outer `acknowledgement_token` hash is unkeyed and therefore
  client-derivable — but only from a `violations_digest` the server issued.
  **No client can mint a digest**, so no client can mint an
  acknowledgement. The token is a bearer value over a *specific*
  classification, not an authorization: overriding still requires merge
  rights, checked independently.

**What `null` means downstream — a property, not a single cause.** This
document previously said `violations_digest: null` ⇔ `ack_key_id == ""` ⇔ no
key configured. That wording is **superseded**. The invariant that matters is
not "null has one cause"; it is:

> **The nullity of `violations_digest` never encodes anything about the
> classified sets.**

Two causes satisfy it, and no third is permitted.

1. **No key is configured** (`ack_key_id == ""`) — a deployment-wide,
   principal-independent, branch-independent fact that reveals nothing about
   any branch.
2. **The classification did not complete** because a blob was unfetchable, so
   the server cannot honestly claim the digest covers a complete set. This
   state has exactly one shape and implementations must not invent another:
   `ack_key_id != ""`, `prediction: "unavailable"`, `reason:
   "blob_unavailable"` or `"conflicts_undetermined_unreadable"`,
   `violations_digest: null`, `certified: false`, `uncertified_cause:
   "classification_incomplete"`, and **no `ETag`**. This is an
   infrastructure fact about blob storage, not a fact about the branch: it
   fires on transient unavailability regardless of what the classification
   would have said, and the identical branch state yields a real digest on the
   next attempt. Emitting a digest over a set known to be incomplete would be
   strictly worse — it would bind an acknowledgement to a classification the
   server never finished, and merge-time reclassification would refuse it
   anyway, having recomputed the complete set.

The two are **distinguishable without ambiguity**: `ack_key_id` is `""` in
case (1) and a real 16-hex epoch in case (2), and case (2) always carries a
naming `reason`. A caller that sees `null` therefore always knows which world
it is in — "this server cannot mint acknowledgements at all" versus "retry
this request".

Under **either** cause the rest of the preview is **unaffected** — the
four-tree classification, `verdict`, counts, samples and `certified` are
computed and returned as usual for everything the server did classify, because
none of them depend on the key; only the acknowledgement path is closed. The
CLI must therefore render the verdict, counts and samples normally, store no
digest, and branch on `ack_key_id` when an override is attempted:
**`ack_key_id == ""`** — **fail fast**, not retryably, naming the server-side
cause ("this server has no acknowledgement key configured; overrides are
unavailable"); **`ack_key_id != ""`** — report the verdict as uncertified and
fail with the **retry-shaped** message instead. In neither case may it send a
merge request with a missing or invented token. Under **neither** cause is
`null` "no violations", and under neither is it a reason to fall back to a
locally computed digest.

**Verification at merge time.** `finalize_squash_merge` reclassifies,
recomputes the digest from its own complete sets with the same `K_ack`, and
compares; a token matches only if the merge-time classification is identical
to the previewed one. It reclassifies **with the same full privileges and no
path-policy term** — the merge-side computation is not principal-scoped (see
[The privileged classification model](#the-privileged-classification-model-restricted-paths)
below), which is what makes "identical to the previewed one" a well-defined
claim at all. Anything that moved — a new violation, a resolved one, a head
advance, a key rotation, a classifier bump — changes a term and the
acknowledgement is refused. And if the merge-time classification **cannot be
completed** — the same unfetchable-blob case that nulls the digest on the read
side — the acknowledgement is **refused, not waived**: an incomplete set can
neither be compared nor overridden.

**The CLI cannot mint an acknowledgement token.** Because `K_ack` is
server-side only, a local preview — however certified, including a
`live_remote_fetch` one — carries no minting authority. The CLI can only
**relay** a `violations_digest` the server issued, so an override path
requires calling the server's merge-preview endpoint at the **exact heads
being merged** and passing the digest it returns straight through with the
merge request. A local four-tree assessment is evidence for a human or an
agent; it is never an input to the token. The CLI stores a digest with its
pins **and the `ack_key_id` it came with**; a stored digest whose
`ack_key_id` no longer matches the server's is from a rotated epoch and must
be discarded and re-fetched rather than sent.

If the target advances between preview and merge — or if the violation or
overlap set changes for any other reason — the digest no longer matches the
server's recomputation inputs, the token fails to verify, and the server
re-runs classification fresh. A stale acknowledgement can never wave
through a merge against a newer target.

### The privileged classification model (restricted paths)

This is what makes `classified_sets` complete and the digest
principal-independent — the reason the digest can safely bind paths the caller
may never see. oakspace's `docs/checkout-free-review-api.md` §2 is normative;
the rule has two halves and both are stated here because the merge side must
state them identically.

- **READ — privileged, principal-independent.** While computing the four-tree
  classification the server may read **any** blob in the repository *as
  itself*, including blobs under prefixes restricted from the caller. Those
  reads happen inside the server, emit no per-path output, and are the same
  reads it already performs to serve a granted caller and — decisively — to
  **perform** the merge the preview previews. There is **no path-policy term**
  in the classification; path policy constrains only what a *response*
  returns.
- **RETURN — absolute.** No restricted path name, no byte of restricted
  content, and no per-path fact about a restricted path ever reaches a caller
  without a grant: not in a listing, not in a `*_sample`, not in a `reasons`
  string, not in an error body, not in a header. This is enforced at the
  serialization boundary rather than by convention.

**Why the prohibition cannot be on reading.** The four-tree classification is
*not* pure hash comparison. Three of the trees are manifests, but PREDICTED
RESULT contains **content-merged entries**: for a path both sides edited,
whether it lands in PREDICTED with merged bytes (⇒ `overlap`) or is excluded
as a predicted conflict is decided by *running the text merge*. Manifest
signatures answer "both sides changed this path"; they cannot answer "and the
merge reconciled it". A server that refused to read restricted blobs therefore
could not place a restricted double-edit into `classified_sets` at all — and
`classified_sets` is exactly the set the acknowledgement digest is required to
cover **completely** and **principal-independently**. "Never reads a
restricted blob" and "complete, principal-independent digest" are not jointly
satisfiable; this contract takes the read. (The classifier itself stays pure
and does no I/O, as described above — the I/O and the privilege live one level
up, in materializing PREDICTED RESULT.)

**What an ungranted caller sees instead.** Restricted findings are reported as
**counts without paths**, never as undetermined values
(`invariant_violations_restricted`, `conflicts_restricted`,
`restricted_paths_omitted`). Structural and content conflicts on restricted
paths are folded into the same counts, and `clean` is **`false`** whenever any
conflict exists, restricted or not — never `null` for them — because a merge
gate must not undercount, and after the privileged read there is nothing left
to be undetermined about. `clean: null` survives for exactly one case and it
has nothing to do with policy: a **storage failure** left a double-edit
undecided (`reason: "conflicts_undetermined_unreadable"`), the same transient
case that nulls the digest with a non-empty `ack_key_id` above.

**Two merge-side consequences**, stated the same way in both documents:

1. `finalize_squash_merge` **reclassifies the same way** — full privileges, no
   path-policy term, not under the merging principal's view — so preview and
   merge compute one function of the same four trees and "identical to the
   previewed classification" is well defined. It could not sensibly be
   otherwise: `finalize_squash_merge` is already reading and merging every one
   of those blobs to produce the squash commit.
2. When that classification **cannot be completed** (unfetchable blob), the
   acknowledgement is **refused, never waived**, and no digest is minted over
   a knowingly incomplete set.

The rejected alternative — never read restricted blobs, classify restricted
double-edits `undetermined`, digest only the determinable paths — is not
merely a different choice: "determinable" is principal-relative, so it breaks
the principal-independence the keyed MAC's security argument rests on (two
callers at the same heads would receive different digests, and a caller could
narrow their own view to obtain an acknowledgement covering a smaller set).
§2 of the oakspace document records the full comparison.

Rollout: detection (this changeset) first, observe `invariant_violations`
in the wild; then server-side rejection; acknowledgement flow last.
