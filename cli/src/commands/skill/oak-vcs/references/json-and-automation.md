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
| `oak clone` (picker) | `oak clone ORG/REPO` |
| `oak split` (editor) | `oak split --plan FILE` (or `-` for stdin) |
| `oak reset` / `oak restore` (confirm) | add `-f` |
| first `oak push` of a new repo (org picker) | `oak push --repo ORG/REPO` |

## Scripting without parsing

- `oak diff --exit-code` — exit 1 when differences exist, 0 when none (like
  `git diff --exit-code`); predicted conflicts count as differences.
- `oak ci status` — exit 0 passed / 1 failed-or-none / 3 still running.
- Global exit codes: 0 success; 1 generic; 2 usage; 3 locked; 4 dirty tree;
  5 conflicts; 6 network/server/auth.
- `oak status --porcelain` (alias `-s`) — stable compact changed-path rows.
- `oak commit --json --quiet` — machine-readable checkpoint, silent no-op.

## Environment variables

- `OAK_REMOTE` — override the stored remote URL for one invocation (same as
  `-r`).
- `OAK_REPO` — `ORG/REPO` for `oak push --repo` (first-push linking without
  a TTY).
- `OAK_DIFF_TOOL` — replace the interactive diff browser with your own tool
  over two materialized trees. The tool must block until done (the trees are
  temporary), e.g. `OAK_DIFF_TOOL="code --wait --diff"`.

## Paging large diffs

Progressive disclosure — summary first, then hunks, scoped as needed:

```bash
oak diff <branch> --json                          # per-file summary
oak diff <branch> --json --hunks --max-bytes 60000  # bounded patches
oak diff <branch> --json --hunks -- path/to/file    # one file's full patch
oak diff --json --changed-files-limit 50 --changed-files-offset 50  # page summaries
```
