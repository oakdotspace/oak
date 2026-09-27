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
- **Comparison identity matters.** Unknown `--against` branches error.
  Missing or disconnected ancestry produces `contribution_unavailable`
  lineage evidence (or an incomplete-data error), never invented additions
  against an empty tree. Use explicit tree mode for a snapshot comparison
  without a contribution claim. A complete initial local history against
  unborn main can legitimately use an empty baseline.

## oak agent state

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
| `oak switch` (picker) | `oak switch NAME` / `oak switch -c [NAME]` |
| `oak clone` (picker) | `oak clone ORG/REPO` (`--json` for one acquisition receipt) |
| `oak split` (editor) | `oak split --plan FILE` (or `-` for stdin) |
| `oak reset` / `oak restore` (confirm) | add `-f` |
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
  5 conflicts; 6 network/server/auth; 7 merge prediction uncertified;
  8 integrity proof inconclusive (a remote proof ran out of budget — not
  evidence of missing content; JSON error code `integrity_inconclusive`, with
  exact `recommended_next_commands`). An integrity proof the server would not
  admit within the bounded wait is exit 6, code `integrity_admission_busy`.
- `oak status --porcelain` (alias `-s`) — stable compact changed-path rows.
- `oak commit --json --quiet` — machine-readable checkpoint, silent no-op.
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
- `OAK_DIFF_TOOL` — replace the interactive diff browser with your own tool
  over two materialized trees. The tool must block until done (the trees are
  temporary), e.g. `OAK_DIFF_TOOL="code --wait --diff"`.
- `OAK_NO_UPDATE_CHECK` — any value disables the update check. Otherwise, at
  most once per 24 h and only after a command succeeds without structured
  output, Oak synchronously asks `https://github.com/<OAK_RELEASE_REPO or
  oakdotspace/oak>/releases/latest` (2 s timeout) and may print an upgrade
  hint on stderr. `--json` commands never run it. This is Oak's only
  background network request.
- `OAK_PROBE_TIMEOUT_SECS` — per-request bound (1..=300, default 20) for
  read-only diagnostics (`oak auth status`, `oak repo list`).
- `OAK_UPLOAD_CONCURRENCY` / `OAK_DOWNLOAD_CONCURRENCY` — chunk transfer
  concurrency for push / clone and pull.
- `OAK_ALLOW_PARTIAL_CLONE` — recovery only: skip blobs a broken server failed
  to ship instead of erroring.
- `OAK_AUTHOR` — commit author override; `OAK_EMAIL` — `oak feedback` contact.
- `OAK_VERBOSE` (timings), `OAK_LOG` (tracing filter), `OAK_PROGRESS`
  (`always`/`never`/auto), `OAK_SPINNER`, `NO_COLOR`, `CLICOLOR_FORCE`, `CI`.
- `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY` (and lowercase
  forms) are honored by Oak's HTTP client for every remote request.
- `OAK_SERVE_TOKEN` (`oak serve` token), `OAK_MOUNTS_ROOT`, `OAK_URL` (macOS
  mounter download base), `OAK_RELEASE_REPO`, `OAK_FEATURES`,
  `OAK_TRUSTED_REMOTES` (tests).

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
