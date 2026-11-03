# Oak 0.109.1

Oak 0.109.1 is a patch release of the `oak` CLI and the `oakvcs-core`
library. It adds `oak queue` and `oak release` (alias `oak tag`), and fixes
several macOS mount problems. `oakvcs-core` only changes version. Hashes and
on-disk formats are unchanged.

Released source: oak/oak main at the version-bump commit (notes cover main
through `d269d554`). The previous release, 0.109.0, was built from oak/oak
main `16a220e8`.

## `oak queue`

The CLI for the server's merge queue (oakspace migration 0141).

- `oak queue add` queues a branch (default: the current one) at its pushed
  head, instead of merging it.
- `oak queue status` shows what's queued, which trains are testing, and
  recent results.
- `oak queue remove` takes a branch out of the queue.

The server tests queued branches together with everything ahead of them, as
the exact tree main will have, and lands them in order. While a repo requires
the queue, `oak merge` onto main is refused with `merge_queue_required`. Run
these from a checkout, or name the target with `--repo` and `--branch`.

## `oak release` / `oak tag`

Repository tags and releases as one concept: immutable tags on main with
optional notes and downloadable assets. It works against servers that
advertise `release_protocol` v1 (oak.space with the `releases` flag, and
`oak serve`).

- `list`, `view`, `create`, `edit` and `delete` manage tags. A tag's commit
  never changes. Deleting a tag frees its name for reuse.
- `upload`, `remove-asset` and `download` manage assets. Each downloaded file
  is verified against its content hash before it is written.
- `push` publishes local-only tags, such as those kept by
  `oak clone <git-url>`, at their commits. Those commits must already be on
  main.
- `list --offline` reads the cache that `oak pull` keeps.

## macOS mounts (FSKit)

- Extended attributes are native in the FSKit extension.
- AppleDouble `._*` sidecar files no longer show up in directory listings.
- Dev builds and reloads of the extension no longer break live mounts that
  share the broker.

## Build

- Debug builds carry line tables only, which keeps `target/` smaller.
  Backtraces still show file and line. `CARGO_PROFILE_DEV_DEBUG=2` restores
  full debug info.
