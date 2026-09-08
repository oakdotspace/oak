# Interrupted publication reconciliation

Status: design for the next implementation slice. The current CLI reports
uncertain publication outcomes but does not persist or reconcile operations
across process restarts.

## Boundary

The existing checkout observation in `work_state.rs` is
`.oak/LAST_PUSHED_HEAD.json` schema 2: one identity-bound, atomically written,
overwritten record of a successful push or remote refresh. It is a positive
head/cache observation, not a multi-operation intent log. Its absence cannot be
interpreted as rejection, and its one-row lifecycle cannot safely represent
multiple in-flight publications. Finish-phase errors currently exist only in
the process response; they do not create a restart record. Mounts use their
separate state adapter and `unpushed_commit_count` rather than the checkout
receipt, so their lifecycle must be measured and designed explicitly rather
than assumed equivalent.

The follow-on may deepen these existing boundaries. Before one mutation is
sent, persist a bounded intent containing:

- a locally generated operation ID;
- normalized remote and repository identity, branch, expected predecessor, and
  submitted target;
- operation kind and request identity; and
- state `before_send`.

Never store credentials, response bodies, or content already held by Oak's
content-addressed store. A local operation ID correlates local evidence; it is
not a server idempotency key and does not make replay safe.

The initial state machine is:

```
before_send -> sent_unconfirmed -> confirmed
                             \-> rejected
                             \-> superseded_unknown
```

Write `sent_unconfirmed` immediately before handing the request to the HTTP
client. A valid request-shaped acknowledgement may produce `confirmed`.
An explicit rejection may produce `rejected`. Disconnects, timeouts, ambiguous
statuses, invalid receipts, and interruption before receipt persistence remain
`sent_unconfirmed`.

## Restart reconciliation

Reconciliation is read-only. Query the exact repository and branch recorded in
the intent, under current authorization:

- observed head equals target: `confirmed_current_state`; this proves current
  state, not which actor caused it;
- observed head equals expected predecessor: retain `sent_unconfirmed` unless
  the retained evidence proves the mutation was rejected;
- observed head is different: `superseded_unknown`;
- missing, unauthorized, unavailable, or insufficient evidence: retain
  `sent_unconfirmed` with the specific observation failure.

Reconciliation never resends a mutation, rewinds a branch, overwrites later
work, or converts a later advance into proof that this operation succeeded.
The next slice must choose a bounded per-operation storage location, maximum
record count or byte budget, terminal-record retention, and eviction behavior
for both checkouts and mounts. It must not overload the single checkout receipt
or silently discard unresolved intents. No schema or journal is introduced by
this design slice.

## Executable boundary fixture

`http::tests::accepted_request_with_lost_reply_is_unconfirmed_and_not_replayed`
accepts an entire publication request and closes the connection before response
headers. `request_never_accepted_is_still_reported_as_unconfirmed` covers a
connection refused before server acceptance. The push and finish integration
tests separately prove that uncertainty preserves local work and suppresses
blind retry advice. A follow-on implementation must additionally inject process
termination in `before_send`, after acceptance, and after acknowledgement but
before local receipt persistence, then verify the persisted transitions above.
