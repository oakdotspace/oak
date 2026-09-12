# Interrupted publication reconciliation

Status: implemented for ordinary and staged-final branch-head publication from
checkouts and mounts. Metadata-only publication, merge-created targets, and
repository creation are not yet covered by durable restart records.

## Durable boundary

The existing checkout observation `.oak/LAST_PUSHED_HEAD.json` schema 2 is one
identity-bound, atomically written successful-push or remote-refresh cache row.
Mounts have a separate pushed-head observation in their mount state. Neither is
an intent log, and absence of either receipt is not evidence that a request was
rejected.

Before a covered mutation is handed to the HTTP client, Oak creates a private
record under `.oak/publication-attempts/` or the mount's equivalent state
directory. Each record has a non-reused UUID and contains only bounded,
redacted evidence: normalized destination identity and its fingerprint,
repository and branch, expected predecessor, submitted target and desired
branch-state digest, transport, request digest, timestamps, and state. It does
not store credentials, response bodies, descriptions, or content objects.
The request digest hashes the already serialized body; it does not make a
second body copy or add an HTTP request.

The state machine is:

```
before_send -> sent_unconfirmed -> acknowledged -> downstream receipt -> remove
                             \-> rejected -> remove
```

Disconnects, timeouts, ambiguous statuses, invalid receipts, and interruption
before downstream receipt persistence retain the record. Acknowledged records
are not allocator-reclaimable: the owning operation removes one only after the
existing positive pushed-head receipt is durable. Separate processes reserve
separate UUID records under a short local allocation lock; no lock is held over
the network.

On Unix, Oak syncs a newly created attempt directory through its parent before
the first reservation and syncs each private record and directory transition.
Windows uses the platform's available file flushes but cannot provide Unix
directory-fsync semantics. No client can preserve evidence across `SIGKILL`,
power loss, or storage hardware that does not honor completed flushes with an
absolute guarantee.

Each checkout or mount has a hard capacity of 4096 records, with 8 KiB per
record. Capacity failure occurs before remote mutation and does not evict
unresolved or retained evidence.

Malformed, oversized, identity-mismatched, and newer-schema UUID records still
count against that capacity and keep publication blocked. Agent state reports
bounded diagnostics with the exact local operation ID and unknown owner
liveness; Oak neither refreshes nor automatically deletes an invalid record.

## Read-only reconciliation

Inspect the first bounded page after a restart:

```bash
oak agent state --refresh --json --compact
```

The document reports `pending_publication_count`, bounded
`pending_publications`, `pending_publications_has_more`, and an executable
`pending_publications_next_after` continuation. A page contains at most 50 rows
and 48 KiB of encoded record data. Continue with:

```bash
oak agent state --json --compact --publication-after OPERATION_ID
```

Refresh makes at most one read-only branch request per displayed operation,
within one 30-second budget for the whole displayed page. Header wait and
incremental body reads share that deadline, and streamed bytes are capped at
64 KiB even when the response omits `Content-Length`. It
uses credentials only when they are bound to the recorded normalized
destination and repository identity, never follows redirects, and bounds the
response. Missing required branch fields remain insufficient evidence. The
live producer's omitted `close_reason` is the defined `None` representation.
Self-hosted `oak serve` currently returns only the branch head. Refresh may
report that head as an observation, but it classifies a metadata-bearing
attempt as `target_head_current_insufficient_branch_fields`; it does not
fabricate the missing branch state or upgrade the observation to full-state or
acknowledgement evidence.

Observations distinguish:

- `current_state_satisfied`: the exact head and branch state are current;
- `predecessor_current`: the recorded predecessor is current;
- `superseded_unknown`: another head is current;
- `absent_unknown`: the branch is absent; and
- unavailable, malformed, oversized, or unauthorized evidence.

Even `current_state_satisfied` proves only current state. It does not prove an
acknowledgement or which actor caused the state, and refresh never deletes a
record. While any record remains, `publication_blocked` is true,
`finish_eligible` is false, and agent state suppresses bare push/finish advice.
The independent `needs_push` fact remains honest, including when new local work
was committed after the unresolved attempt.

## Explicit local retirement

After separately deciding to accept that one operation's outcome remains
unknown, remove exactly that local record with:

```bash
oak agent publication forget OPERATION_ID --acknowledge-unknown --json
```

This command performs no network request, never runs automatically, requires a
canonical UUID, and refuses a record still owned by a live process. Non-force
mount teardown and `oak space clean` preserve mounts with retained publication
evidence. Force teardown remains the explicit destructive override.

## Evidence limits

These records are local client evidence, not server idempotency keys,
attestations, or proof of actor causality. Oak never automatically replays a
mutation from them. `oak agent state --refresh` is observation, not a write or
an acknowledgement. Later slices may cover other mutation kinds, but must keep
that distinction and must not reinterpret old pushed-head cache absence as
rejection.

For staged publication, cancellation before final handoff may abort an
incomplete session. Once the final envelope may have been handed off, a lost
reply or Ctrl-C remains `publication_unconfirmed`, retains the operation ID,
and sends no follow-on abort or replay that could contradict the unknown
outcome.
