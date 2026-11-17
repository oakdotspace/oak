# Oak 0.109.0

Oak 0.109.0 is a minor release of the `oakvcs-core` library and the `oak`
CLI. It adds the core of **Projects**: views and `OAK_PROJECT` files. The
Oak server needs these APIs to serve pruned project clones. Nothing in the
CLI uses them yet, and every existing hash and on-disk format is unchanged.

Released source: oak/oak main at the merge of the Projects P0 branch, which
includes this version bump.

## Views (`oak_core::view`)

A view is a set of included path prefixes, minus excluded prefixes, plus
"spine" file names (`AGENTS.md`, `CLAUDE.md`). Those files are kept in every
directory on the way down to an included prefix.

- `ViewSpec` normalizes a view and has a canonical text form
  (`+prefix` / `-prefix` / `=name`, one per line) and a stable `id()`.
- `prune_tree` removes everything outside a view from a tree. Names outside
  the view are absent, not just their content. Directories wholly inside the
  view keep their hashes, so only the spine directories are rebuilt.
- `graft_tree` is the inverse, used on push. It splices a view tree back into
  the full tree and refuses any entry outside the view.
- `derive_view_commit` builds the view commit for a full commit. It is an
  ordinary `Commit` over the pruned root, verified like any other. A commit
  that doesn't change the view reuses its parent's view commit. When the
  source commit also changed paths the viewer can't read, its message is
  redacted.

## Projects (`oak_core::project`)

- `parse_project_file` parses `OAK_PROJECT` TOML. It covers name, maintainers,
  writers, readers, deps, `self_approve`, `[paths]` rules (kept in file order)
  and `view_always`. It uses the `.oak/PERMISSIONS` principal grammar and
  fails closed.
- `ProjectIndex` answers: the deepest owning project for a path, dependency
  closure (cycles allowed), dependents, touched projects, and `view_for(names)`.

## Protocol (`oak_core::protocol`)

- `ViewQuery` is the opt-in for `pull`, `commits/info` and `push`, sent as
  query parameters (`view_protocol=view_v1`, `view`, `projects`). Request
  bodies are unchanged.
- `PullResponse`, `CommitInfoResponse` and `PushResponse` gain an optional
  `view: ViewResponseData` field. It holds the effective view, its id, the
  full→view commit mappings, and a count of prefixes excluded by permission.
  Code that builds these structs needs `view: None`.

## Dependencies

- `toml` (with `preserve_order`) is now a regular dependency of
  `oakvcs-core`, not part of `local-repo`. With `default-features = false`,
  server consumers can still parse `OAK_PROJECT`.
