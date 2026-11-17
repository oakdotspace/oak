# Oak 0.114.0

Oak 0.114.0 is a minor release of the `oak` CLI and the `oakvcs-core` library.
CI workflows are now kept on the server and edited with `oak workflow`, and
`oak ci publish` names a workflow instead of a channel. `oak watch`,
`oak config` and `oak release` are gone, and this release fixes a set of
security issues found in an audit of the CLI and core.

Object hashes are unchanged, and there's no SQLite migration.

Released source: oak/oak main at the version-bump commit. The previous release,
0.113.0, was built from oak/oak main `7f7205aa`.

## Security

- In a git-backed checkout, hooks are read from `<git dir>/oak/hooks`. The
  work tree's `.oak/` there is ordinary repository content, so a hook in
  `.oak/hooks/` isn't run, and Oak says where local hooks belong. In a native
  Oak repo that sits beside a `.git`, a hook that git tracks is skipped with a
  warning.
- Oak won't open a `.oak/oak.db` that git tracks, since it names the server
  that commands send your credentials to. The error says how to remove it from
  the repository.
- `.oakroot` is a reserved path component, like `.git`: no commit or checkout
  can contain one.
- `oak root pull` checks every path before it changes anything, makes its
  deletes before its writes, never deletes or writes through a symlink, and
  won't replace a directory that holds files it isn't deleting. It refuses a
  directory that is the top of a checkout.
- When a directory's `.oakroot` names a server other than the checkout's own,
  `oak root pull` and `oak root push` send only a login stored for that
  server, never `OAK_API_KEY` or the checkout's key.
- Branch names, commits, remotes and paths in suggested next commands
  (`recommended_next_commands`, hints and error messages) are shell-quoted.
- Downloads are bounded. A stored chunk never decompresses past the largest
  chunk size before its hash is checked, a chunk download stops once it passes
  the size the server declared, and `oak upgrade` refuses a binary over
  512 MiB.
- Every git call puts `--` before URLs, remotes and refspecs, so none of them
  is ever read as an option.
- On Windows, `oak export --tree-only` refuses a tree path containing `:`.

## Workflows on the server

A repo's CI workflows are stored on the server, each as a name and a GitHub
Actions YAML definition with every saved version kept. Files in
`.oak/workflows/` are no longer read.

- `oak workflow list` lists the repo's workflows and what starts each.
  `oak workflow show NAME` prints the YAML (`--version N` for an older one).
- `oak workflow set NAME -f FILE` saves a file (or stdin, with `-f -`) as the
  next version, creating the workflow if needed. `--expected-version N`
  refuses if someone saved in between, and `--new` refuses if it exists.
- `oak workflow edit NAME` opens it in `$VISUAL` or `$EDITOR` and saves it.
- `oak workflow try NAME -f FILE [--branch B]` runs a definition once on a
  branch's head without saving it. The draft run never counts for a service
  on `main`. `--expected-commit HASH` pins the head.
- `oak workflow history`, `diff` (`--from N --to N`, default the last change),
  `rm` (asks first, or `--yes`) and `export DIR` (writes `NAME.yml` for every
  workflow you can read).
- `oak commit` that adds or changes a file in `.oak/workflows/` or
  `.github/workflows/` says Oak doesn't read it, and suggests the
  `oak workflow set` command that saves it. `oak commit --json` lists those
  files in `workflow_files`.
- `oak ci status` leaves draft runs and publish runs out of each service's
  state.
- `oak mcp` has `oak_workflow_list`, `oak_workflow_show`, `oak_workflow_set`
  and `oak_workflow_try`. The last two are write tools, so they need
  `--allow-write`. The definition goes to the command on stdin.

## Publishing a workflow

Publish settings (secrets, runners, reviewers, mode) belong to one workflow,
and there are no channels.

- `oak ci publish WORKFLOW` names the workflow by its name (`release`), file
  name (`release.yml`) or path. Publishes are always from `main`.
- Naming `main`'s head is an ordinary publish. Only `--commit HASH` for a
  commit behind it is a republish, and `--pause-auto-publish` applies only
  then.
- `-e`/`--channel`, `--workflow` and `--branch` still work for older scripts
  but no longer appear in help.

## Help text

- `oak` and `oak --help` list each command with a one-line description,
  grouped by what you're doing, within 80 columns. Each command's flags are in
  its own `--help`.
- Every command's `--help` starts with a short summary and wraps to the
  terminal, up to 100 columns.

## Other changes

- `oak approve --path` names a boundary by its path, which may be a single
  file. `oak access` and `oak owners` show a file boundary without a trailing
  `/`.
- `oak site show` reports the site's bandwidth this month against its budget,
  and says when files stop being served and when that resets.
- `oak clone --expected-head FULL` of a partial checkout accepts the full
  commit that the checkout's view commit was made from.
- The agent instructions `oak init` writes and the bundled `oak-vcs` skill
  describe `oak pull` for staying current with `main` and `oak workflow` for
  CI, and no longer mention `oak watch`.

## Removed

- `oak watch` and `oak config`, with `oak start --no-watch`, the `OAK_WATCH*`
  environment variables and the global `~/.oak/config.json`. `oak skill
  install` and `oak init` no longer add Claude Code hooks to
  `.claude/settings.json`. `oak start --json` drops its `watch_*` fields and
  `oak agent state --json` drops `watch`.
- `oak release` and its `oak tag` alias. `oak clone <git-url>` and `oak init`
  in a git repo no longer keep git tags, `oak export` no longer writes them,
  and `oak pull` no longer keeps a local tag cache.
- `oak init`'s offer to write a game-engine `.oakignore`.
- The `oak mount` shortcut that reused a cached head commit and manifest on a
  remount. Mounts of one repo still share the file content they fetch.
- From `oakvcs-core`: the `engine` module (`detect_engine`, `GameEngine`),
  `Capabilities` and `Repository::capabilities`, tags (`Tag`, `LocalRelease`,
  `validate_tag_name`, `create_tag`, `list_tags`, `delete_tag`), `FileStatus`,
  `MetadataKey::WatchConfig`, `Repository::atomic_commit`,
  `copy_chunk_to_writer`, `put_blobs` (use `put_blob_sources`),
  `set_remote_url`, and the staged-push validation helpers in `protocol`.
  `chunk_decode_limited` and `chunking::MAX_CHUNK_SIZE` are new.

## Compatibility

This release goes with the current oak.space server. `oak workflow`, the MCP
workflow tools and `oak ci publish WORKFLOW` need it. Against a server that
doesn't keep workflows, `oak workflow` stops with an error that says so.
Claude Code hooks that older versions added run `oak watch hook EVENT || true`,
so they do nothing now and can be removed from `.claude/settings.json`.
