# AGENTS.md

Guidance for AI coding agents working in this repository.

Oak is version control built for you and your agents. This repo holds the
reusable VCS library (`core/`), the `oak` CLI (`cli/`), and the server behind
oak.space (`server/`). Agents are among the CLI's main users, so you're
working on a tool you also use.

> Claude Code reads `CLAUDE.md`, which is a one-line `@AGENTS.md` import, so
> this file is the single source of truth.

## This repo uses Oak, not Git

Do not run `git` commands here. Use `oak`. The full guide is the `oak-vcs`
agent skill: read it with `oak skill show` (or install it with
`oak skill install`). Reference files: `oak skill show commands.md`,
`merging.md`, `json-and-automation.md`, `conflicts-and-recovery.md`.

The short version:

- No staging area: `oak commit` checkpoints every change (or the given paths).
- Commits carry no message (`-m` is refused). The branch description
  (`oak desc`) is the narrative and becomes the squash-merge message on main.
- Commits are local until `oak push`, `oak commit --push` or `oak finish`.
- Prefer `--json`. Every payload lists `recommended_next_commands`, and every
  failure is one JSON document with an `error.code`. Run a recommended
  command rather than guessing flags.
- Use non-interactive forms: `oak diff --print`/`--stat`/`--json`, and always
  give `oak switch` a name.
- Lost? `oak agent state --json --compact`.

```bash
oak start "task summary"         # every task starts here: branch from main's head, described, pushed
oak status --json --compact
oak diff --print                 # working tree vs HEAD
oak diff --branch --stat         # the whole branch vs main
oak commit
oak pull                         # merge in main; on CONFLICT: oak conflict status, fix, oak pull --continue
oak log --remote --json -n 20    # remote main's history
```

## Parallel tasks

Give each task its own fresh checkout, the way you'd use a git worktree, and
never work in a shared or long-lived one:

```bash
oak clone oak/oak --shallow ./oak-<slug>   # newest commit only, full working tree
cd ./oak-<slug> && oak start "<summary>"
```

One task = one checkout = one branch. Report the checkout path with every
branch and validation result.

## Working on the Oak source

- The `oak-vcs` agent skill (what `oak skill install` writes and
  `oak skill show` prints) is canonical in
  [cli/src/commands/skill/oak-vcs/](cli/src/commands/skill/oak-vcs/) and
  `include_str!`'d into the binary. Keep it in sync when CLI flags or
  behaviour change. Keep `SKILL.md` short (a test caps it); detail goes in
  `references/`. Its eval suite lives next to it (`evals/`, not shipped).
- The `--json` contract (append-only schemas, one error document, exit codes)
  is in the skill's `references/json-and-automation.md`. Debug builds check
  that every recommended command parses, so the test suites catch bad advice.
- Build with `cargo build -p oakvcs-cli`; the package is `oakvcs-cli`, not
  `oak-cli`.
- `server/` is a separate Cargo workspace (not a member of the root one): run
  `cargo`/`make` from `server/`, and follow [server/CLAUDE.md](server/CLAUDE.md)
  for changes there. A change to `cli/` or `core/` that touches the protocol
  should also pass the CLI's end-to-end tests,
  `cargo nextest run -p oak-cli-tests` from `server/`.
- A CLI flag or command change also needs the docs' command inventory
  refreshed: see the header of `server/crates/oak-web/src/docs/cli_coverage.rs`.

## Finishing a task

Leave the branch reviewable and server-visible:

```bash
oak diff --branch --stat         # confirm it lists only your files
oak finish --desc-file /tmp/oak-branch-desc.txt --json
```

`oak finish --json` preflights, sets the description, checkpoints, and
publishes once. If a phase stops, its JSON names the next command. If
publication is unconfirmed, run `oak agent state --refresh --json --compact`.
Do not merge your own branch unless you were asked to land it.
