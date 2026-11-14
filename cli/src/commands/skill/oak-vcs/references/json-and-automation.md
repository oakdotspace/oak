# Machine-readable output and automation

Oak's `--json` output is a stable surface agents may build durable habits on.

## The contract

- **Schemas are append-only within a `schema_version`.** New fields may
  appear at any time; existing fields are never removed, renamed, or changed
  in meaning without a `schema_version` bump. Parse leniently: ignore fields
  you don't recognize.
- **Absent means default.** Optional fields are omitted at their default
  value (e.g. missing `category` means `"source"`, missing
  `binary_or_large` means `false`).
- **Every payload is self-describing.** `recommended_next_commands` contains
  exact invocations for the natural next step — prefer running one of those
  over guessing flags. Paged payloads carry a `changed_files_page` block with
  `next_offset` and a ready-to-run `next_page_command`.
- **Budgets are explicit.** Hunk-emitting output honors `--max-bytes`; when
  truncated the payload says so (`hunks_truncated: true`, per-file
  `patch_omitted: true`) and names the command that fetches the rest.
  `patch_omitted_reason` separates `byte_budget` (rerun that path without the
  cap) from `missing_blob` (local bytes absent; use the `--remote` variant),
  `remote_content_budget`, `remote_content_unavailable`, `fork_unavailable`
  and `merge_prediction_unavailable`. Only omissions a rerun can fix get a
  per-file command; one missing file never hides later patches.
- **Unavailable content is explicit.** A file summary with
  `content_unavailable_reason: "missing_blob"` and `stats_available: false`
  has unreadable object bytes. It does not imply binary or large content.
  Branch hunks use historical objects; missing bytes never come from the
  current checkout as a substitute.
- **Kept spellings are explicit.** Where a volume treats two spellings as
  one name but will not respell a file in place (NFC/NFD on case-sensitive
  APFS), an update leaves the tracked file under its old spelling. Any JSON
  document of a command that updated the tree then carries
  `kept_spellings: [{"on_disk", "tracked"}]`, listing only the pairs that
  update kept; `oak status --json` lists the complete set while they last
  (those `renamed`, or, once edited, `added` + `deleted`, entries are not your
  renames). Absent means none, for `status --json` only: an update receipt
  omits pairs kept by earlier updates. A plain `oak commit` and
  `oak commit --push` refuse (`dirty_worktree`, with `kept_spellings`) rather
  than record the old spelling back; `oak finish` reports
  `error.code: "finish_phase_failed"` with `kept_spellings` at top level.
  First respell with two `mv`s via a temporary name (the refusal and
  `oak status` print one per kept entry, up to 20). `oak commit <paths>` is
  safe only for paths outside the kept spelling.
- **Comparison identity matters.** Unknown `--against` branches error.
  Missing or disconnected ancestry produces `contribution_unavailable`
  lineage evidence (or an incomplete-data error), never invented additions
  against an empty tree. Use explicit tree mode for a snapshot comparison
  without a contribution claim. A complete initial local history against
  unborn main can legitimately use an empty baseline.

## Failures under --json

`--json` is a property of the invocation. Whatever goes wrong — including a
usage error the argument parser rejects before the command runs, and
including commands that have no `--json` flag of their own — a failing
`--json` invocation leaves **exactly one JSON document on stdout** and exits
non-zero. (The error is not repeated on stderr; progress and warnings may
still appear there.)

```json
{"schema_version":1,"error":{"code":"branch_not_found","message":"Branch not found: nope","conflict_paths":[],"recommended_next_commands":["oak branch list --json"]}}
```

- `error.code` is one of a closed, append-only set of specific codes — never
  `error`, `generic` or `unknown`. Key on it, not on `message`.
  `remote_error` is the one coarse code: a remote/transport failure that has
  not been classified further.
- Usage errors (unknown flag, missing argument, conflicting flags,
  `--json=true`) are `invalid_argument`, exit 2, `retryable: false`;
  `message` is the parser's explanation.
- `error.retryable` is present only where the answer follows from what the
  failure is: `true` for `repo_locked`, `integrity_admission_busy`,
  `worktree_capture_raced` and `description_stale` with state
  `remote_unverified`; `false` for parser-rejected usage errors, approval
  refusals, the other `description_stale` states and a failed `oak feedback
  link|unlink`. Absent means not known — in particular an `invalid_argument`
  raised by a command (not the parser) says nothing about retrying.
- `error.recommended_next_commands` are exact invocations that parse; ones
  that need a checkout are only offered inside one.
- **A document that reports a failing outcome carries the failure in it.**
  When the command's own document is the evidence for the failure, the same
  `error` object is its trailing top-level field and no second document is
  printed:
  - `oak merge --dry-run --json` exit 7 (`merge_prediction_uncertified`) or
    5 (`merge_invariant_violation`): the full preview plus `error`. The
    preview's `recommended_next_commands` and
    `error.recommended_next_commands` are the same list.
  - `oak pull --json` (checkout or mount) exit 5 (`conflict_detected`, with
    the receipt's `conflict_paths`) or any other parent-sync failure: the
    receipt plus `error`. This is new: these receipts used to have no
    `error` key. A parser that branches on the presence of `error` now gets
    a complete error body there (code, message, `conflict_paths`), next to
    the receipt's own fields.
  In both cases the document has one advice list: `error.
  recommended_next_commands` repeats the document's top-level list.
- So for the commands above and every envelope, a non-zero exit has
  `error.code`. Three kinds of document are deliberately different:
  - commands that answer through the exit code (`oak diff --exit-code`,
    `--check`, `--verify-fingerprint`, `oak ci status|wait|publish`,
    `oak file|refs|tree inspect`, `oak branch train`, `oak close --remote`
    with several branches): the document has no `error`; the code is the
    answer;
  - `oak doctor --json` exit 6/8: the report's `outcome` is the answer;
  - a git-backed checkout's `oak pull --json` conflict receipt.
- `oak feedback` keeps its own exit codes (usage 2, failed request 1).
  `oak feedback transition` failures add `error.feedback_transition`.

Codes that replaced the old catch-all `error` (exit codes unchanged):
`branch_not_found`, `branch_already_exists`, `branch_closed`,
`branch_is_server_only` (`oak switch main`), `commit_not_found`,
`blob_not_found`, `manifest_not_found`, `group_not_found`, `no_commits`,
`no_edits`, `repo_already_exists`, `invalid_hash`, `merge_failed`,
`no_merge_in_progress`, `no_sync_in_progress` (`oak pull --continue|--abort`
with nothing paused), `merge_invariant_violation` (5),
`merge_prediction_uncertified` (7), `local_commits_not_in_remote_history`
(5), `remote_commits_not_in_local_history` (5), `remote_moved`,
`restricted_content`, `io_error`, `database_error`, `serialization_error`,
`git_error`. `remote_repo_not_found` and `remote_repo_already_exists`
(exit 6) were previously reported as `remote_error`. `oak whoami` with no
stored credential is `auth_required` (exit 6) with the `oak login` command.

Approval refusals (`oak approve`, `oak merge`) keep the server's reason as
the code: `approvals_required`, `review_key_required`, `branch_moved`,
`branch_closed`, `self_approval`, `not_an_owner`, `nothing_to_approve`,
`boundary_not_touched`, `boundary_outside_view`, `service_account`, `view`,
`not_found`, `internal`. A reason from a newer server that this client does
not know is `approval_refused` with the raw value in `error.server_code`:
when `error.server_code` is present, key on it rather than on `error.code`.
`oak root pull|push` refusals are `root_conflict`, `pull_required`,
`head_moved` (exit 5).

Small receipts (`schema_version`, `kind`, the operation's own result,
`recommended_next_commands`; progress goes to stderr):

| command | `kind` | fields |
|---|---|---|
| `oak init [PATH] --json` | `init` | `path`, `branch`, `imported_commit_count` (never prompts) |
| `oak switch NAME --json` / `oak switch -c [NAME] --json` | `switch` | `branch`, `head`, `previous_branch`, `previous_head` (read back after the switch) |
| `oak fetch --json` | `fetch` | `branch` (`main`), `head_before`, `head_after`, `updated`, `checkout_branch` (read back after the fetch) |
| `oak pull --continue --json` | `pull_continue` | `branch` (whose paused sync was completed), `parent`, `head` (that branch's head after the sync commit), `checkout_branch` |
| `oak reset [PATH] --force --json` | `reset` | `changed`, `reverted`, `deleted` (paths actually written back / removed; up to 200 each), `reverted_count`, `deleted_count`, `paths_truncated` |
| `oak restore [PATHS] --force --json` | `restore` | same as `reset` |
| `oak split --plan FILE --json` | `split` | `branch`, `dry_run`, `written`, `branches` (`name`, `head`) |
| `oak whoami --json` | `whoami` | `remote`, `username` |
| `oak sparse set\|add\|remove\|disable --json` | `sparse` | `sparse`, `paths` (the stored scope, as `oak sparse list --json`) |

`oak status --reconcile --json` reconciles, then prints the ordinary status
document. `oak reset`/`oak restore` cannot prompt under `--json` and refuse
without `--force`; a reset or restore with nothing to do reports
`changed: false` and empty lists. `oak switch --json` needs a name or `-c`.

Human-readable text that changed with this contract (exit codes did not):
`oak close` with no branch name outside a repository now reports
"Repository not found…" instead of "No branch name provided and no current
branch set"; an unreadable current directory is reported as "IO error: …";
`oak feedback` with no message on a non-terminal prints `error: No message
provided…`; `oak whoami` with no credential prints "Not logged in to …"
without the "Server error:" prefix.

## oak agent state

Checkouts report the `oak watch` daemon under `watch`: `running`,
`conflict_paths`, `last_push_error`, `push_blocked` (the error code auto-push
is parked on, e.g. `push_capability_unavailable`), `stop_reason`,
`unseen_notices`. A branch
with no work and no description recommends `oak start "<summary>"`
(`recommended_action.kind: start`). `oak watch status --json` has the detail.

```bash
oak agent state --json --compact [--refresh]
```

One document with the repo's current situation and the next useful agent
actions — the "where am I, what now?" preflight. `--compact` omits
null/default fields and redundant aliases; `--refresh` updates remote
freshness fields first. Run it when entering an unfamiliar repo, after an
error, or when resuming interrupted work.

## Non-interactive discipline

These are interactive without flags and will hang or fail unattended:

| command | non-interactive form |
|---|---|
| `oak diff` (browser UI) | `oak diff --print` / `--stat` / `--name-only` / `--json` |
| `oak switch` (picker) | `oak switch NAME` / `oak switch -c [NAME]` (`--json` for a receipt) |
| `oak clone` (picker) | `oak clone ORG/REPO` (`--json` for one acquisition receipt) |
| `oak split` (editor) | `oak split --plan FILE` (or `-` for stdin) |
| `oak reset` / `oak restore` (confirm) | add `-f` (`-f --json` for a receipt) |
| `oak ci publish` (confirm) | add `--yes` (or `--dry-run` to only preview) |
| first `oak push` of a new repo (org picker) | `oak push --repo ORG/REPO` |

## Scripting without parsing

- `oak diff --exit-code` — exit 1 when differences exist, 0 when none (like
  `git diff --exit-code`); predicted conflicts count as differences.
- `oak diff --check` — exit 1 when added lines carry whitespace errors or
  leftover conflict markers, 0 when clean.
- `oak diff --verify-fingerprint ID` — exit 0 when the working tree's
  change_id (from `oak diff --json --fingerprint` or `oak change capture`)
  still equals ID, 1 when it changed.
- `oak ci status` — exit 0 passed / 1 failed-or-none / 3 still running.
- `oak ci wait RUN_ID... --commit HASH --json` — one bounded, read-only,
  exact-head wait; exit 0 passed / 1 failed / 3 timed out still running.
- Add `--summary` for bounded per-run metadata with no embedded logs, explicit
  omitted-detail counts, and read-only detail commands. Legacy JSON is unchanged
  without it; older servers still transfer full run details. Full source hashes
  are required by summary mode. Successful observations are not landing authority.
- `oak ci wait --current --json [--summary]` — same wait, but it binds once to
  the checkout head's runs (no run-id lookup). No run yet within
  `--dispatch-timeout` is state `dispatch_pending`, exit 3. `--events` adds
  JSONL step transitions on stderr; stdout stays one document. Key on `state`:
  a `dispatch_pending` document (even with `--summary`) has no observations
  and omits the summary's `provider`/`execution_backend`/`transfer_scope`.
  Check `binding`: `other_branch_same_commit` means the runs came from another
  branch at the same commit. Not publication-aware; not landing authority.
- Global exit codes: 0 success; 1 generic; 2 usage; 3 locked; 4 dirty tree;
  5 conflicts; 6 network/server/auth; 7 merge prediction uncertified
  (`merge_prediction_uncertified`, embedded in the dry-run document);
  8 integrity proof inconclusive (a remote proof ran out of budget — not
  evidence of missing content; JSON error code `integrity_inconclusive`, with
  exact `recommended_next_commands`). An integrity proof the server would not
  admit within the bounded wait is exit 6, code `integrity_admission_busy`.
  A full clone whose server-issued admission token cannot fit the pull URL
  (an older server with many branches) is exit 6, code
  `clone_admission_token_too_large`; nothing is downloaded and the token is
  never echoed. Its `recommended_next_commands` is the same invocation plus
  `--shallow`, which is NOT an equivalent retry: it yields the newest commit
  only, with no history. Run it only when a clone without history is
  acceptable; a full clone needs the server upgraded. Never applied
  automatically.
- `oak status --porcelain` (alias `-s`) — stable compact changed-path rows.
- `oak commit --json --quiet` — machine-readable checkpoint, silent no-op.
- After a publish, `oak push`, `oak commit --push`, `oak finish` and their
  mount forms ask the server whether the branch still merges into its parent.
  Receipts carry `merge_check.status`: `clean`, `conflicts` (with
  `conflict_files` and `recommended_next_commands`) or `unknown` (with
  `reason`). Human output warns on stderr. The check is advisory: conflicts
  do not change the exit code, because the push itself succeeded.
- `oak push --json` on a commit with more than 128 new files or 64 MiB of new
  content, against a server that has not activated `staged_v1`, first uploads
  the new blobs in bounded requests (no branch or head changes) and then
  publishes once. When the server offers neither, it exits 6 with
  `error.code: push_capability_unavailable` before mutating anything; the
  message names each measured total and its limit, and
  `oak push --plan --json` reports the same under `publication_blocked` with
  `transport: "unavailable"`. The commits stay local and publish unchanged
  later; nothing needs to be discarded.
- `oak push --json` receipts report what was transferred, from server
  acknowledgements: `chunks_required` (unique chunks), `chunks_already_present`,
  `chunks_uploaded`, `bytes_uploaded` (content bytes of the uploaded chunks).
  A file under 10 MiB is one chunk; larger files are content-defined chunks,
  identical on every transport, so an edit uploads only the chunks it
  touched and an interrupted push resumes with only what the server never
  acknowledged. On an interactive terminal a long transfer prints a progress
  line to stderr every few seconds; like the spinner it is off for `--json`,
  pipes and CI unless `OAK_PROGRESS=always`, and `OAK_PROGRESS=never`
  silences it.
- `oak push --json` — machine-readable publication receipt; the next offline
  `oak agent state --json` uses its repository-bound local pushed-head receipt.
  `--refresh` remains authoritative and durably replaces contradicted receipt
  evidence, including when the remote branch was deleted. Exit 6 with
  `publication_unconfirmed` means the request may have landed. Covered
  branch-head publications also retain a local operation ID across restarts;
  run `oak agent state --refresh --json --compact`. Observation never proves
  actor causality or acknowledges the operation automatically.

## Environment variables

- `OAK_REMOTE` — override the stored remote URL for one invocation (same as
  `-r`). A checkout's stored repository key is only sent to the origin
  (scheme, host, port) that issued it; any other remote gets `OAK_API_KEY`
  or that remote's own `oak login`, never this checkout's key. Oak API
  requests never follow redirects. A key from an older client whose stored
  remote is missing or unparseable is pinned as unbound on the first retarget
  and never sent again; recover with `oak login -r <remote>` or a fresh
  `oak clone`.
- `OAK_API_KEY` — explicit credential for this invocation. It takes
  precedence over every stored credential and is sent to **whatever server
  the command targets**, including `-r` / `--remote` / `OAK_REMOTE`
  overrides. Only set it for the server you intend to authenticate to.
- `OAK_REPO` — `ORG/REPO` for `oak push --repo` (first-push linking without
  a TTY).
- `OAK_WATCH_SHARE_WORKING` — boolean that overrides `watch.share_working`
  (off by default; sends uncommitted text files to the branch page).
- `OAK_WATCH`, `OAK_WATCH_AUTO_PULL`, `OAK_WATCH_AUTO_PUSH` — booleans that
  override `watch.enabled` / `watch.auto_pull` / `watch.auto_push` (see
  `oak config`); e.g. `OAK_WATCH=0` keeps `oak start` and the hooks from
  running `oak watch`. `OAK_WATCH_IDLE_SECS` sets how long watch outlives
  agent activity (default 14400, 0 disables). `OAK_WATCH_POLL_SECS` sets watch's poll interval on
  servers without the event stream.
- `OAK_DIFF_TOOL` — replace the interactive diff browser with your own tool
  over two materialized trees. The tool must block until done (the trees are
  temporary), e.g. `OAK_DIFF_TOOL="code --wait --diff"`.
- `OAK_BROWSER` — command `oak login` runs as `$OAK_BROWSER <url>` instead of
  the system browser; a failing command (e.g. `OAK_BROWSER=false`) switches to
  the manual `username:token` paste flow.
- `OAK_NO_UPDATE_CHECK` — any value disables the update check. Otherwise, at
  most once per 24 h and only after a command succeeds without structured
  output, Oak synchronously asks `<OAK_URL or https://oak.space>` for the
  latest release on the saved update channel (`/api/releases/latest`, or the
  nightly's signature on the nightly channel; 2 s timeout) and may print an
  upgrade hint on stderr. `--json` commands never run it. On the nightly
  channel a new build is also installed in the background by a detached
  `oak upgrade --auto` (log: `~/.oak/auto-update.log`); development builds
  never replace themselves. This is Oak's only background network activity.
- `OAK_NO_AUTO_UPDATE` — any value keeps the nightly channel to the upgrade
  hint instead of installing new builds in the background.
- `OAK_PROBE_TIMEOUT_SECS` — per-request bound (1..=300) for read-only
  diagnostics (`oak auth status`, `oak repo list`; default 20) and for the
  main-head check in `oak start`, `oak switch -c`, `oak merge --dry-run`,
  `oak agent state --refresh` (default 5). An invalid value is ignored and
  each keeps its own default.
- `OAK_UPLOAD_CONCURRENCY` / `OAK_DOWNLOAD_CONCURRENCY` — chunk transfer
  concurrency for push / clone and pull. `OAK_PRELOAD_CONCURRENCY` — blob
  preload requests in flight at once (default 3).
- `OAK_ALLOW_PARTIAL_CLONE` — recovery only: skip blobs a broken server failed
  to ship instead of erroring.
- `OAK_AUTHOR` — commit author override; `OAK_EMAIL` — `oak feedback` contact.
- `OAK_VERBOSE` (timings), `OAK_LOG` (tracing filter), `OAK_PROGRESS`
  (`always`/`never`/auto), `OAK_SPINNER`, `NO_COLOR`, `CLICOLOR_FORCE`, `CI`.
- `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY` (and lowercase
  forms) are honored by Oak's HTTP client for every remote request.
- `OAK_MOUNTS_ROOT`, `OAK_URL` (release
  API base for `oak upgrade`, the update check and the macOS mounter
  download), `OAK_TRUSTED_REMOTES` (tests).

`oak environment --json` prints this registry from the binary itself, with
each variable's current state (secrets as presence only) and the update-check
state — prefer it over this list when versions differ.

## Paging large diffs

Progressive disclosure — summary first, then hunks, scoped as needed:

```bash
oak diff <branch> --json                          # per-file summary
oak diff <branch> --json --hunks --max-bytes 60000  # bounded patches
oak diff <branch> --json --hunks -- path/to/file    # one file's full patch
oak diff --json --changed-files-limit 50 --changed-files-offset 50  # page summaries
```

## Large staged pushes

`oak push` automatically batches a large outgoing closure under one staged
session and publishes its head with one final compare-and-swap. On servers
advertising `staged_budget_protocol: "window_v1"` along with the complete
staged capability set, larger cumulative root-validation and blob budgets are
available; each request still obeys the original bounded admission limits.
The planner includes directory placements and parent-root validation when
splitting commit windows. All other cumulative limits still apply.

Older servers retain the original operation limits. An unsupported window
endpoint fails without advancing the branch; the client does not switch
protocols midway, rotate session IDs, or publish a partial history. A single
snapshot that exceeds a request budget must be reduced; splitting it into
another branch does not reduce that snapshot. An unconfirmed final publication
still requires inspection using the receipt's reconciliation commands before
retrying.
