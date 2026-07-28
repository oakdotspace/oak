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
