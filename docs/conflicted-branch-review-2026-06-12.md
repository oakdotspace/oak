# Conflicted Branch Review: 2026-06-12

## Context

Four open branches had server-side merge conflicts and were candidates to merge
into `main` if they passed code review:

- `mount-structured--ef858466`
- `agent-state--ec27f1a0`
- `conflict-json--3d4701c6`
- `finish-json--594e52be`

Current `main` was `d9ef7aca510083421feba5ad0dc17f904c06e33ea3558358f3f4e82b1b26592a`.

## Process Used

1. Confirmed the current branch was clean with `oak status --json`.
2. Listed open branches with `oak branch --json`.
3. Inspected each candidate with `oak branch show <branch>`.
4. Saved current-main copies of the shared files into
   `/private/tmp/oak-conflict-review/main`.
5. Switched to each candidate with `oak switch <branch> --clean`.
6. Copied the candidate version of its touched files into
   `/private/tmp/oak-conflict-review/<branch>`.
7. Compared each candidate against current main with `diff -u`.
8. Switched back to the clean main-derived working branch.
9. Ran the focused test suites that cover the intended behavior.

## Review Decisions

None of the four branches should be merged as-is.

`mount-structured--ef858466` originally added structured mount status, info, log,
and agent-state behavior. Current main already contains that behavior. The
branch's conflict-resolution commit would remove newer `agent state`,
`mount finish --json`, and `progress_state` work now present on main.

`agent-state--ec27f1a0` originally added a compact `oak agent state --json`
preflight command. Current main already contains `oak agent state --json` through
the status/mount command path. The branch's conflict-resolution commit would
replace the current schema and drop newer mount JSON behavior.

`conflict-json--3d4701c6` originally added `progress_state` to status/info JSON.
Current main already contains the compatible `progress_state` support. The
branch's conflict-resolution commit would remove the current `AgentStateJson`
path and regress agent preflight support.

`finish-json--594e52be` originally added `oak mount finish --json`. Current main
already contains a richer `mount finish --json` shape with repo, remote, base,
head, dirty overlay, and before/after unpushed counts. The branch's
conflict-resolution commit would remove current structured mount and agent-state
support.

## Verification

The behavior these branches were intended to add is already covered on current
main:

```text
cargo test -p oakvcs-cli --test agent_json -- --nocapture
8 passed

cargo test -p oakvcs-cli --test mount_lifecycle -- --nocapture
33 passed
```

## Ergonomics Notes

This review required manually switching branches and copying files to a temp
directory because there was no direct branch-to-branch diff workflow. A future
Oak workflow would be smoother with:

- `oak diff <branch> main` or `oak branch diff <branch> --against main`.
- A branch review command that reports changed files, conflicted files, and
  whether the branch patch appears already present on main.
- A machine-readable "superseded by main" signal for branches whose intended
  patch has already landed through another conflict resolution.
- A first-class stale-branch close flow, such as
  `oak close <branch> --reason superseded`.
- Better conflict-review JSON that includes branch description, head, merge
  base, changed files, conflict files, and recommended next commands.
- A way to test a merge result without mutating local branch state, for example
  `oak merge --dry-run --json <branch>`.
- Clear separation between installed CLI behavior and just-built CLI behavior
  when validating fixes to Oak itself. In this review cycle, the installed
  `oak` was older than `target/debug/oak`, which made branch-close validation
  less obvious.

