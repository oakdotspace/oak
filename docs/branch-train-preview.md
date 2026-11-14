# Ordered branch train previews

`oak branch train A B C --against main --json` predicts the named branches in
that order. Each step uses the actual manifest/text merge engine against the
preceding candidate, including merged text from earlier steps. Individually
clean pairwise reviews do not establish that their combination is clean.

This version supports independent sibling contributions. Every source ancestry
edge must reach a verified commit in the initial target's ancestry or an actual
root. These complete source-exclusive walks must share no commit. Stacked
branches and shared unlanded ancestors return `unsupported` in either order.
Missing source-exclusive ancestry returns `incomplete`, never independence or
an invented empty base. Missing older target history is tolerated only when
the separate exact merge-base resolver proves it cannot hide a nearer base;
missing vertices never count as target membership. Corrupt target history
still fails. Proven declared-root bases are separately labeled and require
complete target history so a hidden common ancestor cannot become an empty base.

Local mode uses one SQLite read snapshot of committed objects and refs. It
ignores dirty/untracked files and sparse materialization. Missing out-of-cone
blobs stop the preview; reconstructible empty blobs need no stored row.

`--remote` pins all requested heads/parents from one bounded branch list. It
can populate the existing verified immutable object cache, then computes in
a fresh read snapshot. Local refs, descriptions, checkout files and stat
caches stay unchanged. A final branch-list observation detects moved heads
or parents and reports `stale`. Unchanged means unchanged only at that
observation, not a lock or future guarantee. Missing required cached ancestry
is incomplete; it is not replaced by guessed lineage.

JSON includes repository origin (without URL credentials/query/fragment),
owner/name, observation time/origin, ordered source heads/parents, initial
target head/tree, and each attempted step's fork and input/candidate tree
hashes. Candidates use canonical v1 tree hashes. The preview stops at the
first conflict or incomplete step. Stopped/stale trains have no successful
final candidate. Overlapping binary content requires explicit resolution.

States: `predicted`, `conflict`, `incomplete`, `unsupported`, `stale`. Exit 0
means a complete prediction; stopped previews exit 6 with one JSON document.
Failures before pins are established use the usual JSON error envelope.
Human output has the same scope and does not run a background update check.

Limits: 1–32 unique sources; 10,000 unique ancestry commits; 128 MiB of unique
logical blob content including synthesized results; a shared 30-second
deadline checked between work units; 1 MiB per remote branch-list response.
Existing review tree-expansion bounds also apply. Redirects are not followed;
malformed metadata/parser errors do not echo remote content.

Candidate trees and merged blobs exist only in memory. The command neither
materializes a test checkout nor stores synthetic commits, publishes,
dispatches CI, merges, or supersedes branches. Every candidate needs its own
validation. Its hash is not semantic-equivalence evidence or CI/landing
authority. Execution testing requires separately recreating and verifying
the candidate tree in an isolated workspace.
