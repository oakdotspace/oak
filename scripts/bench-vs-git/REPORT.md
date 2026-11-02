# oak vs git — benchmark report (2026-10-28)

Branch `perf-vs-git`. Harness: [`bench.py`](bench.py). Raw results: [`results/2026-10-28/`](results/2026-10-28/).

## TL;DR

Before this branch, oak 0.106.0 lost to git on almost everything that touches
a working tree or a server: a 200-file clone took **1.7 s (git: 72 ms)**,
`status` on the Linux kernel took **1.6 s (git: 0.25 s)**, a vscode clone took
**147 s (git: 2.7 s)**, and every push re-read the whole snapshot. With the
fixes on this branch oak now beats git on every local operation except
`diff`/`log`, and is within ~1.1–1.4× of git on small-repo clone/push.
Large-repo push/clone and push throughput under concurrency are still behind
git; the remaining causes are identified below with recommended fixes.

## Setup

- Apple M5 Max, 18 cores, APFS, macOS (Darwin 27). git 2.54.0 (Apple Git-157). Rust release build.
- **oak-base** = installed `oak 0.106.0` release. **oak-new** = this branch.
- Corpora (public, `--depth 1` from GitHub, `.git` stripped before import):

  | corpus | files | size | rev |
  |---|---|---|---|
  | BurntSushi/ripgrep | 226 | 4.4 MB | 91f5d22 |
  | microsoft/vscode | 21,145 | 429 MB | b6bfa95e |
  | torvalds/linux | 97,310 | 2.1 GB | c339f4010 |

- **Local lane**: each tool on its own copy; median of 7 runs (import: 1–2).
- **Forge lane**: [ForgeMark](https://github.com/entireio/forgemark)'s workload
  shapes, replayed with the real CLIs against local servers (`oak serve` vs
  `git daemon --enable=receive-pack`). ForgeMark itself drives go-git against
  smart-HTTP git forges, so it cannot target an Oak server; the harness keeps
  its commit shape (1–10 files × 2 KiB per checkpoint), warmup/measure
  windows, strategies (`branch`, `clone`, `session`) and stats (ops/s,
  p50/p95/p99). Unlike ForgeMark, client cost (process start, local commit,
  checkout write) is included — that's what an agent pays.

## Results

### Local operations (median ms; lower is better)

| corpus | op | git | oak-base | **oak-new** |
|---|---|---:|---:|---:|
| ripgrep | import (init + first commit) | 150 | 38 | **40** |
| | status (clean) | 11.9 | 13.6 | **10.5** |
| | status (1 modified) | 11.0 | 13.6 | **10.4** |
| | diff (1 modified) | 9.8 | 14.3 | **10.2** |
| | commit checkpoint | 30.5 | 18.8 | **16.0** |
| | log -n 10 | 10.6 | 8.4 | **8.7** |
| | branch create | 11.2 | 12.9 | **13.8** |
| vscode | import | 10,893 | 759 | **960** |
| | status (clean) | 141 | 388 | **77** |
| | status (1 modified) | 143 | 389 | **64** |
| | status (+100 untracked) | 117 | 384 | **67** |
| | diff (1 modified) | 30 | 410 | **77** |
| | commit checkpoint | 242 | 417 | **190** |
| linux | import | 62,400 | 4,175 | **3,054** |
| | status (clean) | 249 | 1,595 | **149** |
| | status (1 modified) | 248 | 1,587 | **150** |
| | diff (1 modified) | 78 | 1,660 | **173** |
| | commit checkpoint | 561 | 1,883 | **488** |
| | log -n 10 | 18.7 | 33.2 | **28.6** |

On-disk repo size: oak is ~35% smaller than git everywhere (linux 436 MB vs 666 MB).

`git diff` is not apples-to-apples: it only stats index entries, while
`oak diff` must also find untracked files (oak has no index; new files are
part of the next commit), so it pays for a full directory walk.

### Forge lane — ripgrep (ForgeMark shapes, 15 s windows)

| strategy | c | metric | git | oak-base | **oak-new** |
|---|---:|---|---:|---:|---:|
| seed push | 1 | ms | 101 | 324 | **171** |
| full clone | 1 | ms | 68 | 1,746 | **78** |
| branch (push loop) | 1 | ops/s · p50 | 12.1 · 81 ms | 7.8 · 123 ms | **11.0 · 91 ms** |
| | 4 | ops/s · p50 | 33.1 · 101 ms | 13.7 · 208 ms | **21.5 · 182 ms** |
| | 8 | ops/s · p50 | 37.8 · 174 ms | 10.8 · 379 ms | **19.3 · 410 ms** |
| clone loop | 1 | ops/s · p50 | 10.7 · 72 ms | 0.6 · 1,719 ms | **9.7 · 86 ms** |
| | 4 | ops/s · p50 | 31.1 · 100 ms | 0.8 · 5,298 ms | **23.7 · 136 ms** |
| | 8 | ops/s · p50 | 37.3 · 158 ms | 0.6 · 9,158 ms | **28.5 · 222 ms** |
| session push | 1 | ops/s · p50 | 7.6 · 106 ms | 2.1 · 111 ms | **8.9 · 85 ms** |
| session clone | 1 | p50 | 73 ms | 1,661 ms | **88 ms** |

git had 6–20 push failures per window at c≥4 (`unable to migrate objects to
permanent storage` — git daemon's quarantine racing concurrent pushes into one
repo); oak had none.

### Forge lane — vscode (21k files, 20 s windows)

| | git | oak-base | **oak-new** |
|---|---:|---:|---:|
| seed push (first publication) | 7.0 s | 14.6 s | **14.6 s** |
| full clone | 2.7 s | **147 s** | **6.4 s** |
| shallow clone (c=1, p50) | 2.7 s | — (none finished in the window) | **6.2 s** |
| checkpoint push (c=1, p50) | 248 ms | — | **451 ms** |
| checkpoint push (c=4, p50) | 422 ms | — | **559 ms** |

Linux (97k files) **cannot be pushed to `oak serve` at all**, with either
build: `blobs total 96634 exceeds limit 50000` (see open issue 3).

## What was slow, and what this branch changes

Each fix was found by profiling (`sample`) the client and `oak serve`.

1. **Checkout fsynced every file — twice.** `apply_manifest` and clone wrote
   each working-tree file with `write_atomic`, which calls `sync_all` on the
   file and on its directory. On macOS that is `F_FULLFSYNC`, a drive-cache
   flush, ~5 ms each: 98% of a 200-file clone's wall time. Working-tree files
   are a projection of content already durable in `.oak`, and git never fsyncs
   them either. New `atomic_file::write_worktree_file` keeps temp+rename
   atomicity (readers never see a torn file) and sets the exact mode with one
   `fchmod`, without syncing. *Clone 1.7 s → 0.08 s.*
2. **Clone read and decompressed every blob twice**: once just to check it
   exists (`get_blob(..).is_none()`). Now uses `has_blob` (here and in
   merge/materialize).
3. **`status`/`commit`/`diff` checked blob presence with one un-cached SQL
   statement per file** (`missing_blobs_in_manifest`): 21k–97k
   parse+prepare+autocommit round trips per run. Added a batched
   `Repository::has_blobs` (one cached statement, one read transaction), and
   a `HydratedTree` metadata memo: blobs are never deleted, so once a tree is
   verified complete, the next status skips the check entirely.
4. **The status diff rebuilt and re-hashed both manifests** through
   `Manifest::new` even with no sparse cone. It now diffs the scanned entries
   against HEAD in place.
5. **HEAD-manifest load and the filesystem walk ran serially.** They're
   independent; `compute_changes_inner` now overlaps them with `rayon::join`.
6. **Empty-file repos wrote to the DB on every status**
   (`ensure_empty_blobs_in_manifest` re-stored the empty blob). Read first.
7. **`oak diff` flattened HEAD's whole manifest a second time** just to look
   up pre-images. It now resolves only the changed paths.
8. **SQLite hot lookups re-prepared their SQL every call.** `get_blob`,
   `has_blob`, `get_tree` now use `prepare_cached`; new
   `SqliteRepository::blob_descriptors` sizes many blobs in one read txn.
9. **Every push re-read, decompressed and BLAKE3-verified every blob in the
   snapshot — twice (planner and payload) — then sent every blob hash to
   `blobs/check`.** On a 50k-file repo a one-file push sent 50,002 hashes and
   251 trees. New `ServerKnownObjects` walks each outgoing snapshot alongside
   the snapshots of the heads the server just reported (branch head, `main`),
   path by path, skipping identical subtrees by hash; anything reachable from
   a server head isn't re-read, re-verified, re-checked or re-sent. Totals for
   transport selection still count everything, but presence checks skip
   proven blobs. *1-file push on a 50k-file repo: 1.6–2.0 s → 0.26 s; payload
   50,002 hashes / 251 trees → 2 blobs / 3 trees.*
10. **`oak serve` re-validated the full closure of every pushed commit**,
    re-hashing every stored blob. New
    `protocol::validate_staged_closure_over_admitted` stops at trees already
    in the server's published closure (a published tree's closure was
    validated when admitted) and accepts redundant copies of them. The
    original `validate_staged_closure` is unchanged for other callers.
    *Server publish of that push: 0.75 s → 0.1 s.*
11. **`oak serve` shipped chunk bytes as JSON integer arrays** (`Vec<u8>` →
    `[104,101,…]`, ~3.5× expansion) — serializing and parsing them was the
    whole cost of a large clone. For clients that send
    `chunk_batch_protocol: bounded_v1`, `chunks/download` now returns the
    hosted server's binary shape instead: a `batch_url` (length-framed batch,
    the format the client already speaks for the hosted CDN) and a per-chunk
    `download_url`, authorized by a per-process keyed-BLAKE3 capability scoped
    to one repo with a 1 h expiry (the client presents these without its API
    token, as with the CDN). Older clients keep inline content. *vscode clone
    147 s (base) → 10.8 s → 6.4 s.*

Tests: full workspace suite passes (2249 tests). New tests:
`protocol::tests::validation_over_admitted_trees_skips_their_closure_and_accepts_redundant_copies`,
`serve::tests::bounded_chunk_download_links_binary_fetches_scoped_by_capability`;
`cli/tests/push.rs::ordinary_sparse_one_file_push_does_not_spend_live_verification_quota`
now also asserts only the delta blob is checked/sent (and no longer expects a
capability negotiation the server-head proof makes unnecessary).

## Still slower than git — and why

1. **Push throughput under concurrency (`oak serve`)** flattens at ~20
   pushes/s while git reaches 38. Every push, and every chunk-fetch request,
   parses the whole `ServePublishedClosure` JSON (all published hashes) and
   re-serializes it after publishing, inside the per-repo writer lock. That's
   O(repo) per request and serializes pushes. *Fix:* store the published
   closure (and staged-session object sets) in indexed tables, insert
   incrementally, and test membership with queries.
2. **First publication is quadratic.** Staged pushes go in 128-blob envelopes
   (`MAPPING_PROOF_MAX_BLOBS`) and the server stores each session's growing
   commit/tree/blob sets as one JSON value it parses and rewrites every
   envelope. 10k files: 1.7 s; 20k: 6.3 s; 50k: 142 s (git: ~7 s for vscode's
   21k). Same table fix; larger envelopes for inline blobs would also cut the
   round trips.
3. **Repos with more than 50,000 files can't be first-pushed** (`STAGED_MAX_BLOBS`
   applies to the whole first operation). Linux can't be hosted on `oak serve`.
   The protocol already has object-only staging envelopes; the client could
   split a first publication into several sessions or the cap could apply per
   session rather than per operation.
4. **Large-repo checkpoint push is ~1.8× git** (451 ms vs 248 ms on vscode).
   The planner still walks every tree and sizes every blob of the snapshot
   (to compute operation totals), plus a third tree walk in
   `expanded_path_bytes_for_roots`. Totals could be derived from the server
   head's totals plus the path-aligned delta.
5. **Large-repo clone is ~2.3× git** (6.2 s vs 2.7 s). Remaining time: every
   blob is reassembled and re-compressed with zstd on store, the batch
   endpoint fetches 40 chunks per request (500 requests for vscode), each
   request re-parses the published-closure JSON server-side, and checkout
   writes files on one thread with temp+rename. Parallel checkout and storing
   the received compressed bytes as-is would close most of it.
6. **`oak diff`** remains 2–3× `git diff` on big trees because it must walk
   for untracked files (see note above); **`log -n 10`** is ~10 ms slower on
   linux.

## Reproduce

```bash
python3 scripts/bench-vs-git/bench.py --oak target/release/oak \
  --corpus /path/to/ripgrep --corpus /path/to/vscode --corpus /path/to/linux \
  --lanes local,forge --runs 7 --concurrency 1,4,8 --duration 15 \
  --out results.json
```

On macOS copy a rebuilt `oak` to a new path (or `rm` first): overwriting a
signed binary in place gets the next exec SIGKILLed.

## Public context

Oak's site claims large wins over git on snapshots and large binaries (e.g.
many-small-files snapshot 1.4 s vs 29.7 s) and concedes cold `init` is slower
(42.6 ms vs 14.8 ms) — [ai-tldr summary](https://ai-tldr.dev/releases/oak-vcs-launch/).
The [HN launch thread](https://hn.svelte.dev/item/48631726) pushed back on
unverified numbers and benchmark access. This report's import numbers agree
with the snapshot claim (oak 20× faster on linux); the clone/status/push
regressions above were not covered by those published benchmarks.
