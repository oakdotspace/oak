# Oak 0.104.0

Oak 0.104.0 is a correctness, security, and agent-evidence release of the
`oak` CLI and `oakvcs-core`. It fixes a merge-base soundness Blocker present
in 0.103.0, stops a checkout's repository API key from being sent to a
different server, and makes many commands return exact, honest JSON evidence.
It needs no protocol or server change: every client change works against the
currently deployed oak.space. A few features only light up on a newer server,
listed under [Compatibility](#compatibility-and-behaviour-changes).

Released source: oak/oak main at the version-bump commit (notes cover main through `20136f43`). The previous stable
release, v0.103.0, was built from oak/oak main `51572f8e437d` (GitHub mirror
commit `00e3b118`).

## Integrity, merge base and clone

- **Merge-base / fork-point soundness on partial graphs (Blocker fix).** On a
  partial local commit graph (shallow workers, or an ancestry backfill that hit
  one transient server error), 0.103.0 could certify a too-old merge base. Seen
  effects: `oak pull` exited 0 after writing a sync commit that re-applied a
  change main had reverted, and `oak merge --dry-run --json` / `oak branch
  review --remote --merge-preview` reported a clean prediction against the wrong
  base. The merge path and the review path now share one certification rule. A
  base computed on a partial graph either equals the complete-graph result or
  is reported unavailable (`IncompleteAncestry`, exit 5, naming the missing
  commits). It is never a different base.
- **Faster first branch creation in a fresh shallow clone.** `oak switch -c`
  and `oak agent state --refresh` skip the history-sized ancestry walk **only
  when the server's main head is already the local main head**: a fresh
  shallow clone, or no movement on main since the last refresh. On hosted
  oak/oak this took about 176 s before and under 1 s now. If main has moved,
  they do the same complete backfill as 0.103.0. **The first `oak pull` /
  `oak fetch` in a shallow worker, and any refresh after main moves, still
  cost O(history) round trips** (about 3 minutes on oak/oak). That is not
  addressed in this release.
- Main ancestry is repaired through cached shallow boundaries. Both verified
  parent edges are followed, with a bound of 100,000 unique visits.
- Pinned remote review hydrates only the missing ancestry it needs, within
  shared, bounded budgets.
- **Clone integrity admission.** A full-clone integrity proof that runs out of
  the server's budget is now reported as *inconclusive*, not as missing
  content. This is new exit code 8, JSON code `integrity_inconclusive`. The
  error recommends the exact `oak clone … --shallow` retry, and suggests the
  `--allow-unverified-integrity` retry only when the server issued a snapshot
  pin. Clone never narrows scope or falls back on its own. `oak doctor --json`
  adds `outcome: verified | failed | inconclusive` (exit 0 / 6 / 8). Admission
  429s that carry `Retry-After` are retried within a 60 s budget. Other 429s
  fail immediately with `integrity_admission_busy`.
- `oak clone ORG/REPO --detached` lands on a detached HEAD at the default
  branch head without creating a personal branch.
- Legacy selected-branch clones no longer rewrite the worktree twice.
- Equal-tree refreshes preserve proved-equal Unix regular and executable
  files, which keeps compiler caches warm.
- New local evidence records interrupted ordinary and staged-final branch-head
  publication.
- Local `oak serve` honours `depth` on `GET /pull`, so `oak clone --shallow`
  against a loopback Serve is really shallow. This matches the hosted server.

## Security: credential scoping

- **The repository API key is no longer sent to other servers.** In 0.103.0
  and earlier, commands running in a checkout used its stored repository key
  for whatever remote they targeted. This included `OAK_REMOTE=<other> oak
  push`, `oak push -r <other>`, `oak pull/fetch -r <other>` and parent refresh.
  So the key issued by one server was sent to another. `push -r` also
  persisted the new remote first, so later commands in that checkout kept
  sending the key there. Now:
  - A repository key is bound to the origin that issued it
    (scheme://host[:port]).
  - The key is only sent to that origin.
  - Legacy checkouts bind the key to their stored remote.
  - A key with no parseable binding is never used.
  - Trusted host moves carry the binding over.
  - Tokens are redacted in Debug output.

  `OAK_API_KEY` is unchanged. It is an explicit credential and is still sent
  to whatever server the command targets. If you ever ran a push or pull with a
  different remote from a checkout, consider rotating that repository's key.
- `oak finish --json` no longer reports `description_synced: true` when the
  server kept a different description.
- `oak finish` / `oak mount finish` without a stored credential run a
  preflight. They now finalize only against an open loopback `oak serve`
  (loopback host, and an anonymous `GET /api/whoami` that is not rejected
  with 401/403). Every other case keeps the 0.103.0 `auth_missing` refusal,
  and it fires before any description, commit, push or repository creation.
- Emitted retry commands redact URL userinfo.

## Local locking

- **Concurrent logins no longer lose saved credentials.** On macOS, the
  credentials-file lock used to run `kill -0` as a subprocess to check whether
  the lock owner was alive, and treated a failed spawn as "owner dead". On
  other non-Linux platforms, including Windows, the check always said
  "dead". Either way, a contender could steal a live owner's lock and drop a
  saved entry. Liveness is now checked in-process, and only a definite "no
  such process" permits a steal.
- **Stale-lock reaping is serialized.** Reapers of the credentials lock and
  the working-directory lock serialize on an OS advisory lock. Two
  contenders can no longer both acquire a dead owner's lock, and a lock that
  was just taken over is never deleted.
- **The publication allocation lock waits up to 30 s** (was 2 s) before
  reporting `RepoLocked`. Many concurrent processes in one repository no
  longer fail spuriously.
- **New permanent companion files.** The reaper mutex adds one small file
  next to each lock, and these files are never deleted: `.oak/wdlock.reap`,
  `publication-attempts/wdlock.reap` and `~/.oak/.credentials.lock.reap`.
  - Liveness is pid-based: an owner in another pid namespace (containers
    sharing the directory) or on another host (NFS) reads as dead.
  - Mixed old/new clients keep the old reaping race between them.

## Review and diff

- Remote review is honest about what it could not show. Each omitted patch
  carries `patch_omitted_reason` (`byte_budget`, `missing_blob`,
  `remote_content_budget`, `remote_content_unavailable`, `fork_unavailable`,
  `merge_prediction_unavailable`). Missing content affects only its own file.
  Recommended next commands no longer loop back to the same command or suggest
  a mutating `oak fetch` by default.
- `oak branch diff X --remote --hunks` fetches the content of the files it
  returns, using bounded, hash-verified raw reads. `oak branch diff X --remote
  --print` prints the unified patch. An additive `acquisition` block reports
  what was fetched.
- Remote review acquires only the pinned content it needs, verifies it, and
  fails closed when content is unavailable.
- `oak branch review` / `oak branch triage` isolate unreadable branch rows as
  per-branch errors instead of failing the whole listing.

## CI

- `oak ci wait --current [--dispatch-timeout S]` resolves the checkout head
  once and waits on that exact commit's runs. If no run appears, it reports
  `dispatch_pending` with exit 3.
- `oak ci wait --progress` / `--events` emit bounded progress on stderr, as
  text or JSONL. Stdout stays a single document.
- `run_url` appears in `ci wait --json` and `ci runs --json`.
- `oak ci cancel --superseded` lists older in-flight push runs on the current
  branch. It only cancels them with `--yes`.
- CI advice is bound to the checkout's identity. Rerun advice recommends
  exact bounded waits. The `no_runs` advice matches the runs listing.
- Review and triage now read exact-head CI evidence. See the `merge_allowed`
  note under Compatibility.

## Patch fidelity

- `oak diff --print` and working-tree `--json --hunks` use git-compatible
  add/delete headers (`new file mode`, `--- /dev/null`, `deleted file mode`,
  `+++ /dev/null`) and git's empty-range hunk numbering. Patches of added or
  deleted files now apply and reverse-apply cleanly with `git apply`.
- `\ No newline at end of file` markers are emitted, and CRLF bytes are kept,
  so patches apply exactly.
- New `oak diff --check [--json]` flags whitespace errors and conflict markers
  on added lines, and exits 1 when it finds any.
- `oak diff --json --fingerprint` / `oak status --json --fingerprint` append
  a read-only `change_id` equal to `oak change capture`'s id.
  `--verify-fingerprint ID` exits 1 unless the working tree still has that id.
- `--name-only` now combines with `--print`.
- Known limit: the store-backed branch JSON renderer (`oak diff <branch> --json
  --hunks`, `oak branch diff --json --hunks`, `branch diff --remote --print`)
  still prints `a/`/`b/` headers for added/deleted files. Also, `diff --oak`
  extended headers are not applied by `git apply`, so mode changes and
  empty-file creation or deletion don't round-trip through git. Use `oak change
  export` for lossless handoff.

## Local inspection

- `oak refs inspect [--json]` gives a consistent, single-transaction view of
  branch rows, heads, parent inheritance and effective HEAD, and flags
  disagreements.
- `oak tree inspect --at REV --json` lists the complete verified path set of
  one commit.
- `oak file inspect --at … PATH --output FILE` writes verified bytes only,
  with no clobbering. `oak file inspect --remote --at <branch|hash> PATH` reads
  one remote file, checked against its pinned manifest hash.
- `oak export --tree-only --at REV DEST` materializes one pinned tree
  atomically, with no history replay.
- `oak environment [--json]` (alias `oak env`) lists the environment variables
  Oak reads, with secrets shown as presence only and URLs redacted, plus its
  background network behaviour.
- `oak mount list` labels entries `[live]`, `[stale]` or `[orphaned]`.
  `oak mount forget --orphaned` removes dead registrations.
- `oak restore` accepts a literal `HEAD` as the source.
- Feedback `GET` 405 responses are identified without guessing about
  credentials or deployment.

## JSON receipts and identity

- `oak desc … --json` returns a receipt: `local_saved`, and `remote_synced` as
  `true` / `false` / `"unknown"` / `"not_linked"`. It includes exact,
  injection-safe retry commands.
- `oak status --json`, `oak info --json` and `oak branch show [--remote] --json`
  add `repo_owner`, `repo_name`, `remote_url`, `repository_root`, `web_url` and
  `review_url`.
- `oak pull --json` prints one receipt on stdout, with human progress on
  stderr. The receipt covers head and parent-head before and after, the
  branch-update and parent-sync outcome, convergence, the description outcome,
  conflicts, and unknowns. On a conflict it still prints the receipt and exits
  5.
- `oak pull --branch-only` refreshes only the current branch and its
  description. It records a per-branch `parent_sync_deferred` marker, which
  `oak status` and `oak agent state` surface until a full `oak pull`.
- `oak desc --append [TEXT | --file F] [--json]` appends after a blank line,
  keeping the previous text byte for byte. It first probes the remote
  description, and refuses (`description_stale`, exit 5, or exit 6 when the
  remote can't be read) if the local and remote descriptions have diverged.
  The probe-then-write is not a server-side compare-and-swap.
- `oak push --plan --json` is a read-only preview of what `oak push` would
  send. It reports outgoing commits and edges, tree and blob counts, and
  server blob presence, and lists paths it can't model under `incomplete[]`.
- `oak open --print` / `oak open --json` resolve the web URL without launching
  a browser.
- `oak repo list [--json]` lists visible repositories. It is bounded, and
  returns `auth_required` / `auth_denied` with exit 6.
- `oak auth status [--json]` reports the credential source (never the secret),
  identity, and admin reachability.
- Messages and hints are more truthful:
  - `oak pull` no longer prints a false "Retained newer local description"
    warning on every pull.
  - `commit -m` hints lead with `oak desc`.
  - `oak merge --help` explains hosted squash-merge.
  - `oak merge --wait <branch>` is a usage error that names the correct
    spelling.
  - Inline descriptions containing a literal `\n` trigger a warning.
  - `oak log <hash>` explains that it takes paths.
  - `branch show|diff|review NAME` suggests the `--remote` variant.
  - `oak switch NAME --remote` is accepted.
  - Commit-hash-mismatch errors list the hashed fields.

## Compatibility and behaviour changes

- **New exit code 8**: integrity proof inconclusive. Scripts that treated every
  non-zero clone/doctor exit as data loss should treat 8 as "narrow the scope
  and retry", not as corruption. Exit codes 0–7 are unchanged.
- **Fail-closed merge base.** On partial graphs, `oak pull`, `merge --dry-run`
  and remote review now stop with `IncompleteAncestry` (exit 5, or "prediction
  unavailable" naming the missing commits) where 0.103.0 silently used a stale
  base. Recovery: rerun `oak pull`, or `oak fetch` then `oak pull`. On complete
  criss-cross or stale-sync shapes, the chosen base can differ from 0.103.0. It
  is now a best common ancestor.
- **`oak commit` on a detached HEAD refuses** with "HEAD is detached … Create a
  branch first: oak switch -c" and `recommended_next_commands`, instead of a
  bare "branch not found".
- **`merge_allowed` in `branch review` / `branch triage` is now based on CI
  evidence and is conservative.** It was hard-coded `false`. It is now `true`
  only when:
  - the recommended action is validate-then-merge,
  - the merge prediction is certified, and
  - exact-head CI is known to have passed.

  The client claims CI success only when the runs listing is provably complete
  (fewer than one page). On busy repositories such as oak/oak it therefore
  usually reports `checks.state: "unavailable"`, `reason:
  "scan_window_incomplete"`, and `merge_allowed` stays `false`. The server merge
  gate remains the authority.
- **Credential scoping**: an override remote (`-r`, `OAK_REMOTE`) with no login
  and no `OAK_API_KEY` is now contacted anonymously. Log in to that server
  (`oak login -r <remote>`) or set `OAK_API_KEY` explicitly.
- **`oak finish` auth preflight**: without a stored credential, finish refuses
  with `auth_missing` before any mutation, except against an open loopback
  Serve.
- **Repository discovery**: a `.oak/` directory without `oak.db` is no longer
  treated as a repository. Read-only commands return `repo_not_found` instead
  of silently creating `oak.db`, and `oak init` completes such a directory in
  place. `$HOME` is never a repository root: `~/.oak` is global state, and
  `oak init` refuses to run there.
- JSON changes are additive within `schema_version` 1.

### Server-dependent features

These client features need a newer oak.space deploy to take effect. Against the
currently deployed server they degrade explicitly, not silently:

- **Exact-head `oak ci trigger`** requires the server's `ordinary_trigger_v1`
  capability.
- **Selected-branch clone** (`oak clone --branch NAME --expected-head FULL`
  with a direct selected-branch integrity proof, and a waived clone of a
  branch) requires the fixed server.
- **Inconclusive-proof waiver token.** `--allow-unverified-integrity` can only
  proceed when the server issues a snapshot pin for an inconclusive proof. The
  deployed server issues none, so the client exits 8 with an honest "this
  server issued no snapshot token … (upgrade the server)" and recommends
  `--shallow`. The server also needs per-repository proof slots and typed 429s.

**Not in this release:** the fix for concurrent hosted merges that could return
success without landing, or drop a push that arrived during a merge, is a
**server-side (oakspace) change**. It is not a client feature and ships only
with an oakspace deploy.

## Known issues

- **Plain `oak pull` against a local `oak serve` can discard unpushed local
  commits.** This affects a loopback or self-run Serve, not hosted oak.space,
  which was verified safe. When the server returns full history, a pull can
  drop commits that exist only locally. This is also present in 0.103.0, and
  the fix is planned for 0.104.1. Workaround: `oak push` before `oak pull`
  from a loopback Serve.

## Known limitations carried forward

- When several best common ancestors exist, the client tie-break (walk order)
  and the hosted merge (newest timestamp) can pick different bases. This is
  pre-existing.
- `oak pull` continues after an incomplete ancestry backfill (it now stops at
  the resolver instead of writing a wrong commit). Transient ancestor-fetch
  errors are still reported as "incomplete".
- `oak merge --json` can print progress lines on stdout before the JSON when it
  refreshes main.

## Release proof and publication

Run `make release-proof` from a complete source checkout of the release commit
(see [release readiness](release-readiness.md)). Then:

1. Dispatch the "Release (staging)" workflow. It builds, signs, stages,
   verifies, promotes, and flips the GitHub draft live.
2. Dispatch "Publish to crates.io": `oakvcs-core` first, then `oakvcs-cli`
   after the matching core version is visible on the index.

Note: crates.io is at 0.102.1. 0.103.0 was not published there, so this is
the first crates.io update since 0.102.1.
