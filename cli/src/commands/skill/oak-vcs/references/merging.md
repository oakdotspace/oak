# Merging and marks

Merging in Oak is a server-side squash of the current branch into `main`. The
branch description becomes the merge message.

**Oak runs none of your code on the server, before or after a merge.** The
flow:

1. Test locally (the project's own test commands).
2. Merge as soon as the branch is up to date with main (`oak pull`, then
   `oak merge`).
3. Whatever checks the project runs (its own CI, a person, a bot) record what
   passed by moving a mark: `oak mark move server-good <commit>`. A deploy is
   a mark a machine follows (`oak mark follow`). See "Marks" below.
4. If something breaks on main, fix it on main: start a new task
   (`oak start "..."`, which branches from main's head), fix, test locally,
   merge.

`oak ci` and `oak workflow` were removed with Oak CI; they print one line
saying so and exit non-zero.

## oak merge

```bash
oak merge [BRANCH]        # merge (defaults to current branch) into main
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
working, but it does nothing: nothing on the server holds a merge back.

**Agent norm: don't merge your own branch unless the user explicitly asked
you to land it.** `oak push` / `oak finish` make work reviewable; `oak merge`
changes main.

## Marks

A mark is a named pointer to a commit on `main` (or a root) that people and
bots move to record what has passed what (`server-good`, `server-prod`,
`cli-released`). It moves forward along main only, and only once every mark
it `requires` is at or past the commit; rolling back takes a repo admin (or
the owners, with `allow_group_rollback`). Its owner group's members move it;
org agents on that group count.

```bash
oak mark list --json                          # each mark, its commit, how far behind main
oak mark show server-prod --json
oak mark move server-good main --note "tests pass" --json
oak mark log server-prod -n 20 --json         # moves with their reports
oak report main --name tests --body "412 passed"   # `oak report main` lists them
```

**Agent norm: never move a mark that deploys (`server-prod`, `cli-released`)
unless the user explicitly asked for that release.** Moving `server-good`
after the tests passed is fine when the user asked you to record it.

## Typical landing sequence (when asked to land)

```bash
# test locally first: nothing runs on the server
oak finish --desc-file /tmp/desc.txt --json   # describe + publish
oak pull                                       # up to date with main
oak merge --json                               # land it
```

If push or merge reports an unconfirmed publication, inspect the exact remote
branch with the emitted read-only command before deciding whether to retry.

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

The remote branch's `closed` status confirms closure, not why it closed. Look
for the parent log's `merge_parent` matching the exact published branch head and its
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
or `oak log --remote --json --from FULL_HASH` for history from a specific
commit (`--from` works only with `--remote`).

## Reviewing branches without switching

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

Reviews and triage rows keep a `checks` object for the JSON contract. Oak
runs no checks on the server any more, so it is always `required: false`,
`state: "not_queried"`, `reason: "ci_removed"`, and `merge_allowed` rests on
the VCS gates alone: a clean, certified merge prediction that recommends
merging, from a clean worktree. Run the project's tests yourself before you
merge.

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

### Reading review evidence

`--json --hunks --line-numbers` adds structured `hunks` (`old_line`/`new_line`
per line), and `--print --line-numbers` numbers the patch. Branch diff and
review JSON says what its rows prove with `evidence_kind` (`snapshot`,
`contribution`, `predicted_merge`). In snapshot rows,
`basis: "snapshot_absence"` means "absent on the branch", not "the branch
deleted it". `ancestry_diagnostic` names missing ancestry edges and whether
they can matter for the merge base (`below_proven_base: true|"unknown"`).
When a verified straight ancestry path proves one compared tip is the base,
older ancestry diagnostics are bounded. `residual_scan_complete: false` with
`truncated: true` means gap counts cover only the inspected sample; it does
not mean all old history is present. The merge-base proof itself is complete.
Divergent or uncertain ancestry still requires the full certification walk.
