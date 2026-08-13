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
oak ci runs [--limit N]   # recent runs: id, branch, commit, status, duration
oak ci logs RUN_ID        # step-by-step logs for a run
oak ci rerun RUN_ID       # re-dispatch at the same branch/commit; prints new run id
```

All take `--json`.

`oak ci status` exit codes are script-friendly without parsing:
- `0` — CI concluded success (merge gate open)
- `1` — concluded failure, or no runs found
- `3` — still running (retry later)

`oak ci rerun` is for **infra flakes** — a run that failed for reasons
unrelated to the code. If the code is wrong, fix it and push; a new head gets
a new run.

## Typical landing sequence (when asked to land)

```bash
oak finish --desc-file /tmp/desc.txt --json   # describe + publish
oak ci status                                  # 0 open, 3 running, 1 failed
oak merge --wait --json                        # rides out a running gate
```

On failure: `oak ci logs <run-id>` → fix → `oak commit` → `oak push` → retry,
or `oak ci rerun <run-id>` if the failure was infrastructure.

## Reviewing branches without switching

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
