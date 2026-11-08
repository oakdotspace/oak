# Oak 0.112.0

Oak 0.112.0 is a minor release of the `oak` CLI and the `oakvcs-core` library.
Projects are gone: access in a repo now comes from teams and ownership
boundaries set on oak.space, and you clone the directories you want with
`--path`.

Object hashes are unchanged, and there's no SQLite migration.

Released source: oak/oak main at the version-bump commit. The previous release,
0.111.0, was built from oak/oak main `099d2cce`.

## Teams and ownership boundaries

A repo's directories can have **boundaries**: an owner team, an approval
setting (required, advisory or off), and who may read and write them. They're
set on the repo's Access page on oak.space, not in files. New commands:

- `oak access` lists the boundaries you can see.
- `oak owners PATH` shows who owns a path; `oak owners --branch` shows what the
  current branch touches and which approvals it still needs.
- `oak team list|show|create|add|remove` manages an organization's teams and
  sub-teams.
- `oak approve [--path DIR]` approves the boundaries you own (it replaces
  `--project`, and still needs an API key with the Review scope).

`finish`, `merge` and `mount finish` report `impact.touched_boundaries`.
`oak status --json` no longer has an `impact` block.

## Cloning directories

`oak clone ORG/REPO --path DIR` (repeatable) gives you just those directories.
Where the server serves views of the repo, it's a pruned view: directories you
can't read aren't in the tree at all. Elsewhere it's a sparse clone as before.
`oak mount --path` does the same for mounts, and `oak sparse show|add|remove`
changes the directories of an existing checkout.

## Moving repos

`oak repo move ORG/SOURCE --into ORG/TARGET:PATH --owner-team TEAM` moves a repo
into a directory of another, with its history (`--dry-run` shows the plan; the
default policy starts it from the web). `oak repo adopt`, run in a checkout of
a moved repo, carries its work over to a new checkout of the target.

## Removed

- `oak project` (list, show, which, deps, impact, search, import, adopt) and
  `oak view`. Their replacements are above.
- Cloning by project name (`oak clone acme/web` for a project).
- `<dir>/.oak/workflows/` below the repo root is refused again: workflows live
  at the root. A repo that added project workflows under 0.111.0 has to move
  them before newer clones can check it out.

## Compatibility

This release goes with the oak.space server that has teams. Approvals use
response schema 2, which 0.111.0 and earlier can't read, and this CLI can't
read the old one.
