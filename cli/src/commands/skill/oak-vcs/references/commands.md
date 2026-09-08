# Oak command reference

Compiled from `oak <command> --help` (v0.101). Global: `oak [--verbose] <command>`.

## Contents

- [Start a repo](#start-a-repo): init, clone, login/logout/whoami
- [Snapshot changes](#snapshot-changes): status, info, diff, commit, restore, reset
- [History](#history): log, hash
- [Branches](#branches): branch, switch, desc, finish, close, split, merge
- [Sync](#sync): push, pull, fetch
- [Sparse checkouts](#sparse-checkouts)
- [More](#more): export, archive, open, feedback, completions, upgrade

## Pinned local file evidence

### oak file inspect --at HEAD|FULL_HASH --json PATH [--max-bytes N]

Inspect one committed file in a native local SQLite checkout without changing
refs, database contents, or the working tree. `PATH` is repository-relative
(even from a subdirectory), slash-separated, and cannot contain `..`, dot,
empty or control components. Tracked `.oak/workflows` files are supported;
paths are looked up only in committed trees, never read from the filesystem.
`--at` is required: `HEAD` resolves once
inside a pinned read snapshot; an explicit revision must be 64 lowercase hex
characters. Branch names, prefixes, remote repositories, Git backends and mounts
are unsupported in this slice. No hydration or working-copy fallback occurs.

The metadata-only JSON binds the local repository root, resolved commit, root
tree, containing tree, path, blob hash, file mode and declared/verified logical
size. `verified` means the V1 commit header binds the root, visited tree objects
are canonical and hash-verified, and the decoded file bytes match their hash
and declared size. It does **not** verify the unhashed V1 change list, ancestor
closure, unrelated files, or remote durability. Symlink content means its
recorded target bytes; no target is followed. Binary bytes are hashed normally.
The canonical empty blob requires no stored payload.
`evidence.proof_scope` repeats the local snapshot source and the `commit_files`,
ancestor-closure, and remote-durability exclusions for machine consumers.

`status` distinguishes `verified`, `path_missing` (proven against visited trees),
`object_missing`, `corrupt`, `budget_exceeded`, `unsupported` and `unavailable`
(e.g. database I/O). Only verified exits 0; reported non-verified evidence exits
1. Usage/backend/open errors use the standard JSON error envelope/exit contract.
No file content is emitted or exported.

The logical blob budget defaults to 16 MiB and accepts 1 byte through 256 MiB;
stored blob bytes are capped at that budget plus 1 MiB. Trees are capped at
16 MiB each / 64 MiB total along the selected path, commit headers at 64 KiB,
reference fields at 64 KiB, HEAD parent walks at 128 branches, paths at 4096
bytes / 128 components, and zstd decoder windows at 8 MiB.
Exhaustion is inconclusive, never proof of corruption. SQLite raw and zstd blob
codecs are handled explicitly; unknown codecs/formats are not skipped. Existing
legacy tree-entry rows are supported; a schema without codec/content columns is
reported unsupported, never auto-migrated. SQLite may use WAL coordination
sidecars while keeping its normal locking and read-snapshot semantics.

## Immutable working-tree change capture

### oak change capture --json [PATHS...]

Capture the working tree's selected changes as immutable blobs and a derived
tree in the native local SQLite repository without creating a commit, advancing
HEAD or another ref, changing working-tree files, contacting the network, or
using/updating the stat cache. With no paths the scope is full; path arguments
use exact-or-descendant matching and are resolved lexically, so selecting a
symlink selects the link rather than its target. On POSIX, literal backslashes
in physical paths or path arguments are rejected before file bodies are stored,
because Oak's canonical path format cannot round-trip those filename bytes.
Windows path separators remain supported.

Base paths outside the sparse cone are preserved. Restricted and declared
known-loss base paths whose marked blobs are missing locally are also preserved,
even when replacement paths are present in the working tree. Whole-tree capture
reports these exclusions in JSON `warnings` and effective-scope counts, and in
human warnings; present excluded bytes are explicitly reported as not captured.
An explicit path scope containing a present unavailable base path fails with
`capture_scope_excluded` before storing any selected bytes. Restriction remedies
require access from an org admin; known-loss exclusions concern recorded missing
content. Capture never grants access, clears either marker, or recovers content.

The versioned receipt binds repository authority (stored origin, owner, name),
the current branch's effective base commit/tree, requested scope, and a portable
`CanonicalChangeSetV1` identity. That identity is BLAKE3 over domain-separated,
length-framed fields: verified derived base/result trees and canonical ordered
path before/after blob+mode states. It never hashes JSON, rendered hunks,
timestamps, repository names, or commit/ref names.

Receipt path and change samples are capped at 64 items. Their full counts,
omitted counts, and completeness flags make truncation explicit; the identity
still covers the complete canonical scope and change set. The receipt reports
requested scope separately from the effective intersection with the sparse cone
and unavailable-base exclusions. Stored origins are normalized and omit URL
userinfo, query, and fragment data.

Capture reads back and verifies every selected new blob row and each canonical
node identity in the stored result-tree closure, including legacy tree rows.
It does not hydrate or byte-verify unchanged
inherited blobs, so the receipt says that inherited missing objects may remain.
Selected regular-file bodies are currently read whole; backend scheduling
limits aggregate in-flight work, but capture does not claim a strict per-file
memory bound.

Oak's workdir lock excludes cooperative Oak writers during capture. Ordinary
filesystem writers do not honor that lock, so capture is explicitly non-atomic:
per-file before/after metadata plus a second inventory detect observed races and
return `worktree_capture_raced`, but success is not a claim that the filesystem
provided an atomic point-in-time snapshot. Missing base commit/tree objects are
reported as typed incomplete-local-data errors before working bytes are stored.
Only the current v1 Oak object format is supported in this initial slice.

### oak change export CAPTURE_ID --output FILE [--json]

Export one durable local capture occurrence as a self-contained ZIP64 archive.
The archive contains the complete canonical change descriptor plus deduplicated
logical bytes for every referenced before/after blob. Export opens the SQLite
repository read-only, never scans the current worktree, contacts no remote, and
does not hydrate missing content. It creates a private temporary file in the
output's existing parent and atomically publishes without replacing an existing
file, symlink, or directory. Missing, restricted, known-lost, or corrupt local
content fails before publication. Repositories predating durable capture tables
must be opened with a compatible current Oak binary and recaptured; export does
not migrate or backfill them.

## Start a repo

### oak init [PATH]
Initialize a repository in the current (or given) directory.

### oak clone [ORG/REPO] [DEST]
Clone from the server. Bare `repo` defaults to your personal org. Omitting
the repo opens an interactive picker — always pass it when unattended.
- `--branch NAME` — select that open remote branch; capable servers bind it to
  the pull and materialize it directly, while legacy servers retain the
  clone-then-fetch behavior
- `--expected-head FULL` — with `--branch`, require the server to prove that
  exact open branch/head before creating the destination, bind it to the pull,
  and materialize that branch directly. The pin never falls back or retries at
  a different head. It does not narrow history; add `--shallow` independently.
- `--shallow` — only the newest commit on the selected branch (the default
  branch when `--branch` is omitted). Its working tree remains complete, but
  local history plus integrity-proof and recovery scope are narrower.
- `--path PREFIX` — sparse clone: check out only files under these prefixes
  (repeatable or comma-separated); manage later with `oak sparse`
- Clone first negotiates a bounded metadata proof. Authentication grants repo
  access but never silently upgrades this to physical object probing; pull
  independently verifies every transferred hash.
- `--allow-unverified-integrity` — proceed only when that bounded proof stops
  on its typed history/tree/path budget. It never overrides corruption.
- `--allow-legacy-scope` — when an accessible legacy server lacks proof
  capabilities, waive only the pre-download proof for `--shallow`, `--branch`,
  or `--path`; the pull request still carries the scope and verifies hashes.
- `-r, --remote URL` — server URL

### oak login / logout / whoami [-r URL]
Authenticate against an Oak server; `whoami` prints the logged-in username.

### oak doctor --repo ORG/REPO [-r URL] --verify metadata [--json]
Inspect remote commit/tree/blob/mapping integrity. `--verify metadata` is the
cheap structural mode. Physical `existence`/`bytes` modes require login;
`--verify bytes` additionally requires `--depth` and an explicit
`--max-chunks` or `--max-bytes`. JSON may include non-fatal operator
`advisories` for safely readable legacy trees above current write policy.

### oak blob info HASH --repo ORG/REPO [-r URL] [--depth N [--branch NAME]] [--json]
Platform-admin, target-only byte evidence. It is deliberately bounded at
100,000 chunks / 256 MiB and reports truncation instead of adding an unbounded
bypass; unrelated reachable blobs do not consume the target byte budget.
Target history is bounded by the server's advertised profile (modern servers
cover up to 100,000 reachable commits). If an older server's smaller history
window is the only truncation, the command succeeds and prints the proven
scope; pass `--depth N` (and optionally `--branch NAME`) to choose an explicit
bounded primary-chain scope.

## Snapshot changes

### oak status
- `--json` — machine-readable; `--compact` — bounded JSON with recall metadata
- `--porcelain` / `-s, --short` — stable compact changed-path rows (like
  `git status --short`)
- `--reconcile` — apply pending remote-merge branch reconciliation first

Full JSON retains `changes` for compatibility and also separates
`working_changes` (uncommitted, relative to current HEAD) from
`branch_changes` (committed contribution, exact fork base to branch head).

### oak info [--json]
Repo/branch metadata (org/repo link, remote, current branch).

### oak agent state [--json] [--compact] [--refresh]
One compact JSON document with current state and the next useful agent
actions. `--refresh` also refreshes remote freshness fields.

### oak diff [REVS] [PATHS]
Up to two leading args naming a branch/commit (unique hash prefix ≥ 4 hex
chars) select endpoints; remaining args (or after `--`) filter paths.
- Bare `oak diff` = working tree vs HEAD, **interactive browser** — use
  `--print`, `--stat`, `--name-only`, or `--json` when unattended
- `oak diff <branch>` — branch contribution vs fork point, checkout-free
- `oak diff <rev> <rev>` — two revisions (default mode `tree`)
- `--branch` — whole current branch (commits + dirty files) vs fork point
- `--mode contribution|tree|net-merge`; `--against BASE` for branch endpoints
- `--json` with `--hunks` adds patches; `--max-bytes N` bounds them
  (truncation is flagged: `hunks_truncated`, per-file `patch_omitted`);
  `--changed-files-limit/-offset` page the summaries
- `--exit-code` — exit 1 when differences exist (predicted conflicts count)
- `-U N` context lines; `--word-diff`; `-a/--text` for oversized text files

### oak commit [PATHS]
Local checkpoint of the whole tree, or only the given paths. Never pushes.
- No `-m` — commits are messageless; the branch description is the narrative
- `--push` — checkpoint, then publish pending branch commits
- `--push` uses the same optional credentials and server authorization as
  `oak push`, including open loopback servers. A rejected publication keeps
  the local checkpoint; JSON reports the completed commit and retry command.
- `--no-verify` — skip pre-/post-commit hooks
- `--json --quiet` — machine-readable, no human text

### oak restore [PATHS] [-s COMMIT] [-f]
Restore files to HEAD (or `--source` commit) state. `-f` skips confirmation.

### oak reset [PATH] [-f]
Discard uncommitted changes (whole tree, or one path). `-f` skips
confirmation.

## History

### oak log [PATHS]
- `-n N`, `--oneline`, `-v` (changed files), `--json`
- `-S TERM` — commits changing occurrence count of a literal (git log -S)
- `-G PATTERN` — commits whose changed lines match a regex (git log -G)

### oak hash
Print the current HEAD commit hash.

## Branches

### oak branch [list|show|diff|review|triage|rename] [--json]
- bare / `list` — list branches; `--show-current` prints the current name
- `show NAME` — one branch
- `diff NAME` — checkout-free diff summary
- `review NAME` — checkout-free review evidence; `--merge-preview` adds local
  conflict prediction; `--remote` reviews the remote branch without switching
- `triage` — batch analysis over many branches (`--against`, `--status`,
  `--only BUCKET`, `--limit`, `--analysis-depth`)
- `rename OLD NEW`

Remote review and triage prepare source/target trees and the exact verified
merge-base tree closure through the existing commit-info endpoint. Shared bases
are deduplicated within a triage request. Each fetch accepts at most 256 commit
identities. Source/target and base preparation share 128 MiB of response bytes
and a 30-second cooperative deadline. A memoized structural check runs on cached
and fetched trees before any manifest expansion: distinct prepared roots share
a budget of one million expanded entries and 64 MiB expanded file-path bytes,
with maximum tree depth 256 and 100,000 unique trees. Missing trees are fetched;
budget exhaustion is incomplete evidence, not corruption. Cached complete
manifests need no network fetch. Deadline checks surround parsing and occur in
validation/storage loops; the async timer does not preempt synchronous work or
promise a hard wall-clock limit for the entire review command. These are review
bounds, not new push/pull caps.
Preparation may cache verified objects and refresh remote metadata, but does not
switch branches, move the checkout, or scan/store dirty file bytes. It does not
fetch arbitrary ancestry or all base blob contents: missing ancestry, denied or
missing objects, budget exhaustion, and missing merge-content bytes remain
explicit failures/unavailable evidence, never a clean merge certification.

### oak switch [NAME]
- `NAME` — switch to a branch (fetched from remote when not local), or with
  `-d/--detach`, detach HEAD at a commit hash
- `-c, --create [NAME]` — create off latest available main (generated name if
  omitted); keeps dirty files; falls back to local main if remote unreachable
- `--clean` — discard working-tree changes and start from fresh main
- Bare `oak switch` is an interactive picker — avoid unattended

### oak desc [TEXT] | --file FILE
Set the current branch description (`-` for stdin). This becomes the
squash-merge message.

### oak finish [--desc TEXT | --desc-file FILE] [--json]
Finalize the branch: preflight, save description, checkpoint dirty work,
publish. Retryable saga — on failure the JSON names the completed phase and
the next manual command.

### oak close [NAME]
Close a branch.

### oak split [--from BRANCH] [--plan FILE|-] [--dry-run]
Reorder/drop/split a branch's commits (git rebase -i + histedit split).
Interactive editor by default; `--plan` drives it headless. Plan directives,
one per line (`#` comments); every commit appears exactly once:

```
pick <hash>      # replay (unique prefix ok)
drop <hash>      # omit
split <branch>   # start a new branch off main; later picks go there
```

Picks before the first `split` rewrite the source branch in place. Branches
are flat, so each split segment must stand alone on main — a dependent
segment conflicts and **nothing is written**. Preview with `--dry-run`.

### oak merge [BRANCH]
Squash-merge into the parent, server-side, CI-gated. See
[ci-and-merging.md](ci-and-merging.md) for the gate, `--wait`, `--force`,
`--dry-run --json`, and `--continue`/`--abort`.

## Sync

### oak push
- `-f` — force: overwrite diverged remote history
- `--json` — publish and emit the exact branch, pushed head, remote, review
  URL, and next commands as one append-only result (regular checkouts)
- `--repo ORG/REPO` (env `OAK_REPO`) — link a fresh repo on first push
  without the interactive org picker (repo is created if missing)
- `-r URL` remote override (env `OAK_REMOTE`)

### oak pull
Pull refreshes remote branch descriptions while retaining unsynced local edits.
Existing checkouts upgraded from versions without description tracking preserve
their old text when it differs from the remote and print a warning. Inspect
`oak branch list --remote --json`, then run `oak desc` on the affected branch
with the chosen description. Successful publication or a matching remote pull
establishes a baseline; subsequent remote edits refresh automatically.
Responses that started before a newer local metadata update cannot overwrite
or acknowledge that update. Retry `oak pull` for a fresh description snapshot.

Fetch the current branch, then merge in the parent branch.
- `-f` — discard local commits not on remote; sync to remote HEAD
- `--continue` / `--abort` — resume/abandon a conflicted parent-sync

### oak fetch
Refresh local `main` only; never touches the working tree or merges.

## Sparse checkouts

### oak sparse [list|set|add|disable] [--json]
Scope the working tree to path prefixes (the "cone"). Out-of-cone files stay
in the repo and ride along in commits, but aren't downloaded or written.
`set` replaces the cone, `add` extends it (both re-sync), `disable` returns
to a full checkout (`oak pull` hydrates never-downloaded files).

## More

### oak export DEST [-b BRANCH] [--git-branch NAME] [-f]
Replay the branch's linear history into a fresh **git** repo, preserving
author + timestamp. The documented escape hatch off Oak.

### oak archive [-o OUTPUT]
Zip archive of the tree.

### oak open
Open the project on the server in a browser.

### oak feedback [-m TEXT|--file FILE]
Send feedback to the Oak team. Non-interactive callers must pass `-m` or
`--file` (`-` for stdin); otherwise the command exits 2. The
`feature-request` alias, optional attribution flags, and submit JSON
`{"id", "ref", "status"}` remain unchanged.

### oak feedback list / oak feedback show ITEM

Read the platform-admin feedback queue with reporter metadata omitted.
`list --status nonterminal --limit 50 --json` returns one bounded page;
use its `recommended_next_commands` or pass `--cursor` for the next page.
`--status all` includes terminal tickets; an exact lifecycle status filters
to that state. Limits are 1–100. `show fb-165 --json` reads one report directly.
Both commands accept `--remote` and `--json` before or after the subcommand;
the child remote wins. Credentials follow the same isolation as link/unlink.

Responses identify `platform_admin` scope and `live_keyset` consistency:
new insertions do not shift page offsets, but status edits may change
membership. This is not a snapshot or an incremental change feed;
`updated_since` is intentionally unsupported. Contact/identity and internal
metadata are omitted; authored report text can still contain private data.
An old-server or unauthorized 404 is explicit and never falls back to the
unbounded export. This requires the server inventory API to be deployed;
releasing the CLI alone does not activate it.

Newer inventory responses include an opaque per-ticket `revision`; older
responses may omit it. `show` also prints it in human output when available.

### oak feedback transition ITEM

Platform-admin conditional edit, never a legacy PATCH fallback:

```bash
oak feedback show fb-165 --json
oak feedback transition fb-165 --expected-revision TOKEN --status in_progress --json
oak feedback transition fb-165 --expected-revision TOKEN --notes "Investigating" --json
oak feedback transition fb-165 --expected-revision TOKEN --clear-notes --json
```

At least one edit is required. Status and notes may be edited together;
omitted fields are preserved. `--notes` conflicts with `--clear-notes`;
empty notes also clear them. Notes allow 20,000 Unicode characters without
NUL; revision tokens allow 128 bytes without control characters. The ten
lifecycle statuses are unchanged (`nonterminal`/`all` are list filters only).
`--remote` and `--json` work before or after the subcommand, child values win.

The command requires schema1 and numeric `feedback_transition_v1: 1` from
the server capability endpoint. Unsupported/disabled/unauthorized/malformed
capabilities cause no mutation. Resolution uses bounded redacted detail,
never the legacy private export. Credentials remain scoped to that remote;
redirects are not followed. Reads and mutation responses have 30-second
request deadlines; capability/receipt bodies are capped at1MiB, detail at4MiB.

The exact caller-supplied token is sent even if the detail lookup observes
a newer revision. Success JSON contains `confirmed`, `changed`, feedback ID/ref,
status, previous/new revision, schema and protocol—not notes or reporter fields.
A no-op confirms unchanged state without inventing a new revision. Receipts
must match the requested ticket, token and edits before success is printed.

412 `revision_conflict` requires refresh and reconciliation. A transport error,
5xx, interrupted/oversized/malformed success or inconsistent receipt is
`UNCONFIRMED`: it may have committed. Neither case is automatically retried;
never blindly substitute a refreshed token. The command prints the exact
read-only refresh invocation. Exit codes are0 confirmed,1 remote/unconfirmed
failure,2 invalid arguments or missing credentials. CLI release does not
activate the globally gated server feature. No batch or incremental feed is implied.

### oak feedback link ITEM / oak feedback unlink ITEM

Admin-only manual links between a feedback item and a branch or commit.
`ITEM` is `fb-165`, `165`, or its raw id.

```bash
oak feedback link fb-165                         # current repo and branch
oak feedback link fb-165 --commit HASH            # commit-only link
oak feedback link fb-165 --branch NAME --commit HASH
oak feedback unlink fb-165 --link-id ID           # unambiguous row
oak feedback unlink fb-165 --commit HASH
oak feedback unlink fb-165 --branch NAME
```

- `--repo ORG/REPO` defaults to the current checkout's repository.
- `link --commit` without `--branch` does not infer the current branch.
- `unlink` accepts only one of `--link-id`, `--commit`, or `--branch`.
  With no selector it uses the current branch. `--link-id` needs no repo;
  commit matching includes branch-plus-commit links. Ambiguity deletes
  nothing and lists exact link IDs to choose from.
- `--remote URL` and `--json` work before or after `link`/`unlink`.
  Child `--remote` takes precedence if both positions are supplied.
- JSON schema v1 includes `status`, `feedback_id`, optional `feedback_ref`,
  `link` (or `removed` and `removed_link_id`), and
  `recommended_next_commands`. Unknown link fields and optional `source`
  are preserved. A link's undo recommendation names its exact link ID.
  Undo/relink recommendations and ambiguity commands retain the effective
  remote, so an override cannot silently revert to the checkout origin.
  Every argument is POSIX-shell quoted when needed; response-derived names
  are data, never executable shell syntax. Parse-failure diagnostics do not
  include response values (including serde type-error excerpts).

Credentials are explicit `OAK_API_KEY`, then the login saved for the effective
remote, never a checkout repository key or another server's login. Missing
credentials fail before network access. URL userinfo, query, and fragment
are stripped before lookup or network use; URL basic-auth is not supported.

Number lookup requires the updated server's `GET /api/feedback?status=all`
so spam-marked items remain addressable. Older servers rejecting that query
fail clearly before any write; there is no incomplete default-list fallback.
A raw item ID bypasses number lookup, but link/unlink still requires the
deployed admin feedback-links API. This command does not activate rollout
flags or prove that production has deployed that API.

Exit codes follow feedback's narrow contract: 0 success, 1 server/network or
ambiguity, 2 usage. Server failures remain on stderr (no success JSON).

### oak completions SHELL / oak upgrade [-f] [--canary]
Shell completions; self-upgrade.

Mount and space commands: see [mounts-and-spaces.md](mounts-and-spaces.md).
CI commands: see [ci-and-merging.md](ci-and-merging.md).
