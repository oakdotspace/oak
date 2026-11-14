# CI and merging

Merging in Oak is a server-side squash of the current branch into its parent
(normally `main`). The branch description becomes the merge message.

**CI runs on `main`, after merging — never on branch pushes, and it never
holds a merge back.** The flow:

1. Test locally (the project's own test commands).
2. Merge as soon as the branch is up to date with main (`oak pull`, then
   `oak merge`).
3. CI runs each service on main. A **service** is one workflow file under
   `.oak/workflows/`; services pass and fail on their own.
4. If a service breaks on main, an agent fixes it on main: start a new task,
   fix, test locally, merge.
5. Publish workflows publish the newest commit that is good for the
   services they wait for, so a broken main doesn't ship.

## oak merge

```bash
oak merge [BRANCH]        # merge (defaults to current branch) into its parent
oak merge --dry-run --json  # merge prediction certified against remote main; no push or file changes
                            # (one head request; fetches main only if it moved; --offline: no request, exit 7)
oak merge --json          # machine-readable server merge result
oak merge --continue      # after resolving conflicts
oak merge --abort         # abandon an in-progress merge
oak merge --force         # org admins: merge without required path-rule approvals (?force=1)
```

Path rules that need approval refuse the merge until an owner approves (see
[Approvals](commands.md#approvals)). `--force` gets past that only for an org
admin, and only where the repo's approval policy lets a credential do it;
the refusal says when it doesn't. Use it only when the user asked to land
without the approval. `--wait` is still accepted so older scripts keep
working, but it does nothing: there is no CI to wait for before a merge.

**Agent norm: don't merge your own branch unless the user explicitly asked
you to land it.** `oak push` / `oak finish` make work reviewable; `oak merge`
changes main.

## oak ci — the visibility and recovery surface

```bash
oak ci status             # each service on main: passing / failing since <commit> / running
oak ci status --head      # the newest run for the current checkout head (branch pushes usually have none)
oak ci status --run RUN_ID --commit HASH --json # inspect an exact run, independent of checkout head
oak ci wait RUN_ID...     # wait read-only for those exact runs to finish
oak ci wait RUN_ID --commit HASH --json  # also fail closed on head mismatch
oak ci wait RUN_ID --commit HASH --summary --json # compact, log-free exact-run observations
oak ci wait --current --json --summary # bind to the checkout head's runs, then wait
oak ci wait --current --json --events  # same, with JSONL step transitions on stderr
oak ci runs [--limit N]   # recent runs: id, branch, commit, status, duration
oak ci logs RUN_ID        # step-by-step logs, with each job's and step's duration
oak ci logs RUN_ID --summary --json # metadata (incl. started_at/finished_at), no logs or script bodies
oak ci logs RUN_ID --failed --max-bytes 65536 --json # failed steps with bounded log text
oak ci trigger --expected-commit HASH --idempotency-key KEY --branch BRANCH --json
oak ci rerun RUN_ID       # legacy request; returns IDs, not an exact-commit guarantee
oak ci cancel RUN_ID --commit HASH --json # exact ordinary push/merge run only
oak ci cancel --superseded --json      # dry run: older in-flight push runs on this branch
oak ci cancel --superseded --yes --json # cancel those, each via the exact-run path
oak ci approve RUN_ID --commit HASH --json # release a run held for approval (path permissions)
oak ci publish WORKFLOW --dry-run --json  # preview a publish; never authorizes
oak ci publish WORKFLOW --yes --json      # authorize exactly the previewed values (add --wait[=MIN])
oak ci publish WORKFLOW --commit HASH --pause-auto-publish --yes  # republish an older verified commit
```

All take `--json`.

`oak ci status` with no flags reads the newest page of runs (200) and groups
the runs on `main` by workflow file. Each service's state is its newest run:
`running`, `queued` or `awaiting_approval` while in flight (with `was`, what
the run before it said), otherwise `passing`, `failing` (with
`failing_since`, the commit and run where it started failing since its last
pass) or `cancelled`. Publish runs and skipped runs say nothing about a
service. Its JSON has `subject: "main_services"`, `branch: "main"`,
`state` (`success` / `failure` / `running` / `no_runs`), `services`, and
`runs_scanned`; a service with no run in that page isn't listed.

Status and logs JSON include the canonical `run_url`. Exact-run status rejects
a mismatched returned run identity or supplied commit; it reports the observed
run's branch/commit, not the checkout's. `--commit` requires `--run`.

`--head` and `--run` status JSON names its subject: `subject` is
`current_head` (the checkout head) or `exact_run` (the `--run` argument, which may be
another branch or an older head). `checkout_branch`/`checkout_head` are the
local checkout identity when it resolves, and `checkout_matches_observed` is
true only when the observed branch and commit equal them. Bare `oak merge`
acts on the current checkout, so success recommends it only when that flag is
true. A successful run elsewhere is an observation about that run, not a merge
prediction or authorization: success on an older head of the current branch
recommends `oak ci status --head --json` (the checkout head's own run); success on
another branch recommends `oak branch show BRANCH --remote --json` (always
with `--json`, which that command requires; a name that is not a plain token
is shell-quoted after `--`, never interpolated raw); an unresolved checkout
recommends `oak status --json`, as does a run the server reports without a
branch. Text-mode status prints the same follow-ups
and never claims `oak merge` is unblocked for a non-checkout subject. Failure
and running advice stay bound to the run id in every case. This flag is
client-side identity, not approval.

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
- `0` — every service passing (`--head`/`--run`: the run concluded success)
- `1` — a service failing or cancelled, or no runs found
- `3` — a service still running and none failing (retry later)

`oak ci wait` uses `0` for passed, `1` for a completed failure, and `3` for a
timeout. It never dispatches or re-runs work. An absent requested run, a
different returned run id, or a head mismatch fails closed as a server error
(`6`). Transient read failures (connection and body-read errors, HTTP 408, 429, 500, 502, 503, 504) are
retried with bounded backoff inside the original `--timeout`; auth failures,
a missing run, malformed responses and fence mismatches are never retried.
`6` with `category=...` means the latest read of a requested run was still
failing at the deadline, so that run's state is unknown: repeat the printed
`oak ci wait` command. A wait's JSON names the requested run ids plus every observed run id and
commit. Legacy (non-summary) JSON success recommends `oak merge` only when
every observed run is at the checkout's own branch and head, read at decision
time (after the poll, not before it); otherwise it gives the same read-only
follow-ups as exact-run status. Text-mode wait prints no next commands. On
failure the plain payload names `oak ci logs ID`; the bounded
`--failed --max-bytes` form is summary mode's advice. Use `--timeout SECONDS` to
bound all polling and response I/O; retry commands retain the supplied
`--commit` fence when it is a full 64-hex hash (no other value is ever
echoed into an advised command). `--timeout 0` is a single probe whose
request has a one-second I/O ceiling.

`oak ci wait --current` removes the separate `ci status` lookup. It resolves
the checkout branch head **once**, then binds to the newest run per workflow
for exactly that commit **recorded on the current branch**, and waits on those
run ids with the head as the `--commit` fence. It never re-resolves the head
and never binds to a run for another commit. Same-commit runs on other
branches (an older publication, or main's merge run for a branch cut from
main) are ignored while this branch's own run may still be dispatched; only
after `--dispatch-timeout` are they bound, labelled
`binding: "other_branch_same_commit"` with `bound_branches`. Otherwise
`binding` is `latest_run_per_workflow_for_exact_commit_on_current_branch`.

Limits: the server knows when a republished head's CI delivery is still
pending and the client cannot see that, so a republish of the same commit on
this branch may bind the earlier publication's run. The server exposes no
expected-workflow set, so with several workflows the command binds whichever
runs exist at binding time; a workflow dispatched later is not included and
success can be reported early. Success is an observation about the bound
runs, nothing more.

If no run for the head appears within `--dispatch-timeout SECS` (default 120,
bounded by `--timeout`), it reports state `dispatch_pending` with exit `3`
(not a CI result; retry later) plus `head_published` when the remote branch
head could be read within the remaining `--timeout` budget (at most 10 s;
`--timeout 0` stays a one-second probe), recommending `oak push` first when
the head is unpublished. JSON adds `subject` (`current_head`),
`current_branch`, `binding`, `bound_branches` (when other-branch runs were
bound), `dispatch_waited_ms`, `runs_scanned`, and `scan_limit`; exit codes are
otherwise identical to an exact-run wait.

`--progress` prints bounded progress lines on stderr while waiting; `--events`
prints the same events as JSON lines on stderr instead. Stdout still carries
only the single final document. Events come from run/job/step status and
conclusion metadata already polled: `dispatch_pending`, `bound`, `run`, `step`
(one per status/conclusion transition, with 1-based `step_index`/`step_count`),
and a `heartbeat` after 60 quiet seconds. They never contain log text, scripts,
or test counts (the server reports none). At most 200 events are printed, then
one `progress_truncated` marker.

Every wait observation, and every run in `oak ci runs --json`, carries
`run_url`: the canonical web page, built locally from the configured remote
with credentials, query, and fragment stripped.

For compact agent gates use `oak ci wait RUN_ID... --summary --json`.
This opt-in projection preserves the legacy wait payload when omitted. Summary
mode requires full 64-digit source hashes (including `--commit`, if supplied);
missing or malformed observed source identity fails closed without echoing the
remote value. It reports the `oak_ci` protocol provider, exact run/source identity,
per-run `observed_at` timestamps, gate state, and failure counts. Runner/backend
evidence is explicitly unavailable in this projection; the provider field does
not identify a cloud or execution host.

No logs, commands, arbitrary error text, names, server-supplied URLs, or unknown
server fields are embedded (the locally derived `run_url` is the one URL). At most eight failed job IDs and eight failed step identities appear
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
server-side; this command does not issue release authorizations
(`oak ci publish` does).

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

The receipt's `recommended_next_commands` observe exactly the returned ids.
When `requested_commit_confirmed` is true and the hash is a full 64 digits,
the one follow-up is `oak ci wait ID --commit HASH --json --summary`. Otherwise
(unconfirmed, multi-id, or a short legacy hash) it names a one-shot probe
`oak ci wait ID... --json --timeout 0` for immediate inspection of what
actually ran, then the same wait without `--timeout 0`; `--summary` is not
advised there because it fails closed on runs that omit a full hash. It does
not recommend branch-latest `oak ci status --head` (the checkout head may not be
what was re-run), eager `oak ci logs` for runs that have only just been
queued, or another dispatch.

`oak ci cancel` is deliberately narrower than the server endpoint. It requires
one explicit positive run ID and its full lowercase commit hash, reads that
exact immutable run record, and only sends the existing cancellation POST when
the ID and commit match, the event is exactly `push` or `merge`, and the run is
known to be queued or running. It never guesses the latest branch run, performs
bulk cancellation, or cancels `manual` runs (including protected/release work).

`oak ci cancel --superseded` is the only multi-run form, and it is a dry run
unless `--yes` is given. It lists in-flight (`queued`/`running`) `push` runs on
the **current branch** whose commit is not the checkout head and that are older
than the newest run observed for the head; other branches, `merge`/`manual`
runs, and newer runs are never candidates. It sends nothing unless the
checkout head is exactly the published remote branch head and a run for that
head is observed (`blocked_reason` says why otherwise). First-push runs with
event `branch.created` are never candidates (only `push` is), which errs on
the side of not cancelling. With `--yes`, each
eligible run goes through the same exact-run `cancel RUN_ID --commit HASH`
preflight and POST described below, one at a time, never retried. Each
candidate's `action` is `cancelled`, `not_cancelled` (refused before or by the
server; this request cancelled nothing), `outcome_unknown` (network loss,
408/5xx/redirect, or an unconfirmed 409 after the POST: the cancellation may
have been recorded; inspect read-only, never retry blindly), `skipped`, or
`would_cancel` (dry run). Any `not_cancelled` or `outcome_unknown` exits `1`. Cancelling a
run another reviewer is waiting on turns their wait into `cancelled`, so use it
only for your own branch.

### Runs held for approval

In a repo whose ownership boundaries restrict paths, CI checks out the whole tree with the
repo's secrets. So when a run's author (the pusher, or whoever triggered it)
can't read every path, the server creates the run but **holds** it: no
snapshot, no runner, no secrets. The run reads `queued` with
`"awaiting_approval": true`, and it counts as running: `oak ci status` shows
the service as `awaiting_approval` and exits `3` until the run is approved and
passes.

`oak ci status` says when a run is held. `oak ci wait`
progress reports it too. Neither can make it move: a writer who can read the
whole repository has to release it, with
`oak ci approve RUN_ID --commit HASH` or the run page's "Approve and run"
button. The run's author can never approve it themselves. Approval works like
`cancel`: it reads the exact run first and sends nothing unless the id and full
commit match and the run is still held. The server re-checks the approver and
audit-logs the approval. A 403 means you lack full read access, or you are the
run's author.

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

## oak ci publish — publishing with a workflow

```bash
oak ci publish WORKFLOW [--commit HASH [--pause-auto-publish]] [--yes|--dry-run] [--wait[=MIN]] [--json]
```

A publish workflow is a workflow file the repo has set up to publish (Settings
→ CI): its secrets, runners, reviewers and mode belong to the file, on the
server. It runs only through a release authorization, never on a push, and
only from `main`. It can also auto-publish on its own, after each verified
commit or on a daily timer; this command is for manual-publish and for
rollback. WORKFLOW is its name, file name or path (`release`, `release.yml`,
`.oak/workflows/release.yml`); the older `-e NAME` still works. `oak ci
deploy` is an alias. `oak ci publish` first fetches the server's release
preview — `main`'s head commit, the workflow's blob hash, the event, the
settings' policy version, its reviewers, whether a bound runner is online,
and any `problems` — then, after confirmation, POSTs those exact values. The
server refuses (409) if the head, the file, or the settings moved since the
preview, so what was shown is what runs. API keys need the explicit `release`
scope (the server's refusal is surfaced as-is).

- Confirmation is an interactive `[y/N]`. Without a TTY, and always under
  `--json`, pass `--yes`; otherwise it refuses (exit `2`, outcome
  `confirmation_required`). `--dry-run` only previews and never POSTs.
- `--wait[=MIN]` (bare: 30 min) then runs the exact-run, commit-fenced
  `oak ci wait` on the created run and exits with its status.
- JSON is one document: `outcome` (`authorized`, `dry_run`, `not_ready`,
  `cannot_authorize`, `confirmation_required`), the server's `preview`
  verbatim, `authorization` (`authorization_id`, `run_id`, `run_url`, the
  exact `request` sent), and with `--wait` the embedded `wait` summary
  document (or `wait_error`; the authorization still happened).
- Exit codes: `0` authorized (or a dry run that would proceed); `1` not
  ready (see `problems`), declined, or the waited run failed; `2`
  confirmation needed; `3` wait timed out; `6` server/auth refusal, including
  `can_authorize: false` (the caller is not one of the reviewers).
- The POST is not idempotent and never retried. A network loss, 408, 5xx
  (other than the deliberate 503 "no runner online"), redirect, or unreadable
  receipt is an unconfirmed outcome: inspect `oak ci runs` before deploying
  again.

**Agent norm: never deploy unless the user explicitly asked for that
release.** Use `--dry-run --json` to report what would ship.

## Typical landing sequence (when asked to land)

```bash
# test locally first: CI won't run on the branch
oak finish --desc-file /tmp/desc.txt --json   # describe + publish
oak pull                                       # up to date with main
oak merge --json                               # land it; nothing waits on CI
oak ci status                                  # afterwards: each service on main (0 passing, 3 running, 1 failing)
```

When a service fails on main: `oak ci logs <run-id>` → start a task → fix →
test locally → merge; or `oak ci rerun <run-id>` if the failure was
infrastructure. If push or merge reports an unconfirmed publication, inspect
the exact remote branch with the emitted read-only command before deciding
whether to retry.

## Did this branch land?

A server merge closes the remote branch and creates a new squash commit on its
parent. Its hash normally differs from the branch head. A checkout can still
show the old branch as open until refreshed; local `oak branch list` is not
proof of the remote state. `needs_pull` means the cached parent moved, which
can include this branch landing; it does not mean "unmerged".

For a read-only answer, retain the branch name and published head, then read:

```bash
oak branch show BRANCH --remote --json
oak log --remote --branch main --json -n 20
```

Use the actual parent instead of `main` for a stacked branch. The remote
branch's `closed` status confirms closure, not why it closed. Look for the
parent log's `merge_parent` matching the exact published branch head and its
`merge_source_branch` matching the branch. This is squash provenance even
though the new commit hash differs. If the record is outside the page, follow
`next_command` until found or `complete` is true. A truncated page without a
match leaves the answer unconfirmed; a later parent commit does not undo an
earlier landing.

`oak diff BRANCH_HEAD SQUASH_HEAD --json` can additionally verify equal trees.
Tree equality or a dry-run with zero changed files establishes matching
content, not who merged it; changes can arrive independently. Conversely,
current main may have advanced, so a nonempty diff against today's main does
not disprove landing. Prefer the merge provenance for the historical answer.
Do not run `oak close`, repeat the merge, or change the checkout merely to
answer whether an earlier merge happened.

`oak log HASH` treats HASH as a path; use the remote log's pagination command
or `oak log --from HASH` for history from a specific commit.

## Reviewing branches without switching

`oak branch train A B --against main --json` previews independent sibling
contributions cumulatively in the supplied order; `--remote` pins remote heads
and checks for movement afterward. Each candidate has its own tree identity
and still needs validation. It does not publish or supply CI/landing authority.
Stacked/shared-unlanded ancestry is unsupported; missing data stops prediction.
See `docs/branch-train-preview.md` for the contract and limits.

```bash
oak branch review NAME [--merge-preview] [--remote [--repo OWNER/NAME]] [--json]
oak branch diff NAME            # checkout-free diff summary
oak branch diff NAME --remote --repo OWNER/NAME --json [--hunks]  # no checkout at all
oak diff NAME [--json --hunks]  # full contribution diff, checkout-free
oak branch triage [--against main] [--only BUCKET] [--json]
```

`--merge-preview` adds local conflict prediction (no request, so never
certified against `main`); `oak merge --dry-run --json` verifies `main`'s head
against the remote and gives the certified prediction for the current branch.

With `--remote`, `oak branch diff` and `oak branch review` ask the server's
checkout-free review API for the file list, hunks and merge verdict, pinned
to the heads the first response reports (`prediction_method:
"server_review_api"`). Nothing is cloned or downloaded, so `--repo
OWNER/NAME` (or a full `https://host/OWNER/NAME` URL) works from any
directory. Only a server without that API makes the CLI fall back to local
evidence, which needs a checkout. A 429 means slow down; a 503 means the
server is busy — retry after `Retry-After`.

Single-branch review reads `checks` from the server's exact current-publication
CI gate when supported (`source: "server_exact_publication_gate"`). The receipt
is bound to the requested branch and commit, includes pending CI dispatch and
every workflow, and avoids a bounded recent-runs
scan. `no_runs` does not set `known_passed`: it does not prove tests ran. A moved
head, invalid receipt, or server failure leaves checks unavailable. Older
servers (404) fall back to the conservative recent-runs window; batch triage
still uses that shared window and may report `scan_window_incomplete`. CI
success alone never overrides unsafe merge evidence or authorizes landing.

Both carry a four-tree `merge_safety` verdict (see `docs/merge-safety.md`):
`invariant_violations` lists paths whose target-side state the predicted
merge would destroy — a violation makes the dry-run exit 5 and review
recommend `do_not_merge`. From the *local* commands (no `--remote`) that
list is complete for the local classification (no cap, no filtering), so
`[]` really does mean "checked and clean"; absent means the classification
could not run. Do not assume the same of a `--remote` review (whose verdict
now comes from the server) or of any field with that name from a *server* API response —
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
target head backed the check (local `main` that this command did not verify
against the remote — `--offline`, an unreachable remote, or a local review —
or a fetch that covered a different target); the dry-run then exits 7 and
review fails closed (`vcs_merge_safe: false`, no merge recommendation) — run
`oak merge --dry-run --json` online (or review with `--remote`).
`merge_safety.target_head_observed_at` says when the target head was last
verified.

`merge_would_succeed` is the plain answer in the dry-run (and
`merge_preview`) payload: `false` when the prediction found conflicts
(`conflict_files`) or invariant violations — `recommended_next_commands` then
leads with the command that resolves it (`oak pull`) — `true` only when the
prediction is clean and certified against a verified target head, and absent
when no prediction ran or it is clean but uncertified. A conflicted dry-run
still exits 0 (the prediction ran and is certified); read
`merge_would_succeed`, not the exit code, to decide whether to merge.

The head check has a short deadline (5 s; `OAK_PROBE_TIMEOUT_SECS`
overrides): a stalled or unreachable remote yields the uncertified result
(exit 7) instead of hanging. `oak branch review` and `oak branch triage`
without `--remote` stay read-only: no request for `main`, no ref moved, never
certified; their recommendation is `oak merge --dry-run BRANCH --json`. So
local `oak branch triage --only mergeable` no longer lists branches parented
on `main` — use `oak branch triage --remote --only mergeable --json` (the
local output says so). A conflict the local prediction already found is
still reported as such (`merge_conflicts` / `target_risk_unknown`), ahead of
"uncertified". Every recommended merge command names its branch
(`oak merge BRANCH`; `oak switch BRANCH` before `oak pull`), so running the
advice for a branch you are not on never lands the current one.
