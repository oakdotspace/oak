# CI and merging

Merging in Oak is a server-side squash of the current branch into its parent
(normally `main`). The branch description becomes the merge message. Merges
onto main are **CI-gated**: the server refuses the merge (HTTP 412) while CI
for the branch head is still running or after it failed.

## oak merge

```bash
oak merge [BRANCH]        # merge (defaults to current branch) into its parent
oak merge --wait          # if CI is still running, poll (~20s) up to 30 min, then merge
oak merge --wait=90       # same, custom timeout in minutes
oak merge --dry-run --json  # local merge prediction; no push/fetch/file changes
oak merge --json          # machine-readable server merge result
oak merge --continue      # after resolving conflicts
oak merge --abort         # abandon an in-progress merge
oak merge --force         # bypass the CI gate (maps to ?force=1)
```

If CI concludes failure under `--wait`, the merge errors and names the
`oak ci logs` / `oak ci rerun` follow-ups.

`--force` bypasses a failed or stuck gate. Only use it after actually
inspecting the failed run (`oak ci logs`) and only when the user has asked to
land despite CI — never as a first response to a 412.

**Agent norm: don't merge your own branch unless the user explicitly asked
you to land it.** `oak push` / `oak finish` make work reviewable; `oak merge`
changes main.

## oak ci — the visibility and recovery surface

```bash
oak ci status             # CI state for the current branch head (what the gate checks)
oak ci status --run RUN_ID --commit HASH --json # inspect an exact run, independent of checkout head
oak ci wait RUN_ID...     # wait read-only for those exact runs to finish
oak ci wait RUN_ID --commit HASH --json  # also fail closed on head mismatch
oak ci wait RUN_ID --commit HASH --summary --json # compact, log-free exact-run observations
oak ci runs [--limit N]   # recent runs: id, branch, commit, status, duration
oak ci logs RUN_ID        # step-by-step logs for a run
oak ci logs RUN_ID --summary --json # metadata only, no logs or script bodies
oak ci logs RUN_ID --failed --max-bytes 65536 --json # failed steps with bounded log text
oak ci trigger --expected-commit HASH --idempotency-key KEY --branch BRANCH --json
oak ci rerun RUN_ID       # legacy request; returns IDs, not an exact-commit guarantee
oak ci cancel RUN_ID --commit HASH --json # exact ordinary push/merge run only
```

All take `--json`.

Status and logs JSON include the canonical `run_url`. Exact-run status rejects
a mismatched returned run identity or supplied commit; it reports the observed
run's branch/commit, not the checkout's. `--commit` requires `--run`.

Log projections are opt-in: plain logs retains its full legacy payload.
`--summary` omits logs, script bodies, and unknown fields; `--failed` selects
failed/cancelled/timed-out steps and defaults to 64 KiB of total log text.
`--max-bytes` limits returned UTF-8 log text across all selected steps.
JSON reports `projection`, `logs_truncated`, and `log_bytes_returned`;
metadata and JSON escaping do not count toward this text budget. Older servers
still send full run details over HTTP before projection; this is an output
budget, not a promise about server work or download size. A projection is not a
complete diagnostic archive. For the full context use the run URL or increase
the byte budget. New retry logic must not infer safe retries from log filtering.

`oak ci status` exit codes are script-friendly without parsing:
- `0` — CI concluded success (merge gate open)
- `1` — concluded failure, or no runs found
- `3` — still running (retry later)

`oak ci wait` uses `0` for passed, `1` for a completed failure, and `3` for a
timeout. It never dispatches or re-runs work. An absent requested run, a
different returned run id, or a head mismatch fails closed as a server error
(`6`). Its JSON names the requested run ids plus every observed run id and
commit. Use `--timeout SECONDS` to bound all polling and response I/O; retry
commands retain the supplied `--commit` fence. `--timeout 0` is a one-shot
probe whose single request has a one-second I/O ceiling.

For compact agent gates use `oak ci wait RUN_ID... --summary --json`.
This opt-in projection preserves the legacy wait payload when omitted. Summary
mode requires full 64-digit source hashes (including `--commit`, if supplied);
missing or malformed observed source identity fails closed without echoing the
remote value. It reports the `oak_ci` protocol provider, exact run/source identity,
per-run `observed_at` timestamps, gate state, and failure counts. Runner/backend
evidence is explicitly unavailable in this projection; the provider field does
not identify a cloud or execution host.

No logs, commands, arbitrary error text, names, URLs, or unknown server fields are
embedded. At most eight failed job IDs and eight failed step identities appear
per run; totals and omitted counts are explicit. `details_command` retrieves the
complete step metadata using the existing `ci logs --summary --json` surface.
Failure advice retrieves logs only on explicit request with a 64 KiB log-text
budget. No reported failure details is not proof of successful steps; failures
with no details say `unavailable`.

Output grows with the number of requested run IDs, not with logs/jobs/steps:
the serialized summary is bounded by 1 KiB plus 8 KiB per requested ID. The
existing full-detail HTTP endpoint is still used (`transfer_scope` records
this), so output savings are not network, server-work, or memory-budget claims.
Read time remains governed by the wait deadline. Summary errors omit arbitrary
remote/parser diagnostics. Pending advice preserves summary mode and the commit
fence. Success does not recommend merging: these runs are observations, not
human approval or proof that a later merge result was tested.

`oak ci trigger` requires the server's exact `ordinary_trigger_v1` capability.
It never falls back to legacy dispatch. Supply a full lowercase commit hash and
a stable 1–128 non-space ASCII key; keep the key **and the entire request**
unchanged on retry. `--branch` defaults to the current branch, so durable agent
workflows should pass it explicitly. Optional `--workflow STEM` narrows selection;
omitted selects all ordinary workflows. Protected deployment workflows are excluded
server-side; this command does not issue release authorizations.

The server compares the expected head atomically with run creation. JSON reports
`protocol`, `branch`, `commit_hash`, every `run_ids` value, and `replayed`, plus an
exact `oak ci wait ... --commit HASH` follow-up. A 412 means review the moved head;
a 409 means the key is bound to another request. Unsupported capability causes no
POST. Requests have a 60-second I/O timeout and capability/error/receipt bodies
are bounded to 1 MiB. A timeout, oversized response, or malformed receipt after
POST is an **uncertain outcome**, not success or proof nothing ran. Retry with
the same key; the CLI neither generates keys nor automatically retries dispatch.

`oak ci rerun` remains the legacy infra-flake request and is **not idempotent**.
Some servers ignore its requested commit and use the current branch head. JSON
preserves `run` as the first actual returned ID and adds all `run_ids`,
`requested_branch`, `requested_commit`, and `requested_commit_confirmed`. An
ID-only receipt never confirms a commit; the last flag is true only for one
returned run whose server-provided branch/commit match the request. Missing or
invalid IDs fail with an uncertain-outcome error—never a fabricated run 0.
Inspect `oak ci runs` before retrying a legacy request with an uncertain outcome.
If the code is wrong, fix it and push; a new head gets a new run.

`oak ci cancel` is deliberately narrower than the server endpoint. It requires
one explicit positive run ID and its full lowercase commit hash, reads that
exact immutable run record, and only sends the existing cancellation POST when
the ID and commit match, the event is exactly `push` or `merge`, and the run is
known to be queued or running. It never guesses the latest branch run, performs
bulk cancellation, or cancels `manual` runs (including protected/release work).

The preflight response is limited to 1 MiB and 60 seconds. An older server may
embed logs in run detail; if that makes the response exceed the limit, the
command fails before mutation and recommends `oak ci logs RUN_ID --summary
--json` for read-only inspection. It does not fall back to an unbounded read.
The HTTP client never follows redirects or replays the POST.

HTTP 204 means only that the control-plane cancellation was recorded; backend
execution stop is best effort and remains explicitly unconfirmed. On HTTP 409,
one bounded read-only re-fetch may confirm that the exact run became terminal;
otherwise the outcome is `cancellation_not_confirmed` and must be inspected,
not blindly retried. Network errors, HTTP 408, 5xx, redirects, and malformed success
responses are likewise unconfirmed. Diagnostics never echo response bodies or
redirect targets because those can contain credentials or private logs.

## Typical landing sequence (when asked to land)

```bash
oak finish --desc-file /tmp/desc.txt --json   # describe + publish
oak ci status                                  # 0 open, 3 running, 1 failed
oak merge --wait --json                        # rides out a running gate
```

On a confirmed CI failure: `oak ci logs <run-id>` → fix → `oak commit` →
`oak push`, or `oak ci rerun <run-id>` if the failure was infrastructure. If
push or merge reports an unconfirmed publication, inspect the exact remote
branch with the emitted read-only command before deciding whether to retry.

## Reviewing branches without switching

`oak branch train A B --against main --json` previews independent sibling
contributions cumulatively in the supplied order; `--remote` pins remote heads
and checks for movement afterward. Each candidate has its own tree identity
and still needs validation. It does not publish or supply CI/landing authority.
Stacked/shared-unlanded ancestry is unsupported; missing data stops prediction.
See `docs/branch-train-preview.md` for the contract and limits.

```bash
oak branch review NAME [--merge-preview] [--remote] [--json]
oak branch diff NAME            # checkout-free diff summary
oak diff NAME [--json --hunks]  # full contribution diff, checkout-free
oak branch triage [--against main] [--only BUCKET] [--json]
```

`--merge-preview` adds local conflict prediction; `oak merge --dry-run
--json` gives the fullest local prediction for the current branch.

Both carry a four-tree `merge_safety` verdict (see `docs/merge-safety.md`):
`invariant_violations` lists paths whose target-side state the predicted
merge would destroy — a violation makes the dry-run exit 5 and review
recommend `do_not_merge`. From these CLI commands that list is complete for
the local classification (no cap, no filtering), so `[]` really does mean
"checked and clean"; absent means the classification could not run. Do not
assume the same of a field with that name from a *server* API response —
oakspace's checkout-free review API caps it (10,000 paths) and filters it by
path policy, and reports the shortfall in `invariant_violations_truncated`
alongside a `violations_digest` that covers the complete set. Gate on the
verdict and those flags, never on the list looking short or empty. That
digest is a keyed MAC under a server-side secret (it covers the complete sets
including paths path policy hides from you, which is why you can neither
recompute nor verify it): relay it verbatim, never recompute or synthesize
one, and never read `violations_digest: null` as "no violations" — its nullity
says nothing about the classified sets. `null` has two causes, told apart by
`ack_key_id`: `""` means the server has no acknowledgement key configured, so
overrides are unavailable and an attempt fails fast; a non-empty `ack_key_id`
means the classification itself could not be completed
(`uncertified_cause: "classification_incomplete"` — a transient blob-fetch
failure, so retry). Never send a merge request with a missing or invented
token.
`verdict: "uncertified"` means no authoritative
target head backed the check (stale local data, or a fetch that covered a
different target); the dry-run then exits 7 and review fails closed
(`vcs_merge_safe: false`, no merge recommendation) — run `oak fetch` (or
review with `--remote`) and retry.
