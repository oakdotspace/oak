# `oak serve` benchmark report (2026-10-30)

Harness: [`bench.py`](bench.py). Raw results: [`results/2026-10-30/`](results/2026-10-30/). It needs only an `oak` binary: it generates
deterministic repos of N ~2 KiB files (100 per directory), starts `oak serve`
on loopback, and drives it with real CLI processes, so client cost is
included as an agent sees it.

```bash
python3 scripts/bench-serve/bench.py --oak target/release/oak \
  --sizes 1000,10000,30000 --concurrency 1,4,8 --duration 10 --runs 5 \
  --large-mb 128 --out results.json
```

Setup: Apple M5 Max, APFS, release builds. **base** = this tree before the
changes below. **new** = after. p50 in ms unless noted; push throughput is
1-file checkpoint pushes per second with N agents, each on its own branch
(no errors in any run).

| files | metric | base | **new** |
|---:|---|---:|---:|
| 1,000 | seed push (first publication) | 231 | **220** |
| | clone | 221 | **156** |
| | 1-file push | 78 | 79 |
| | push throughput c=8 (ops/s) | 24.1 | **27.6** |
| 10,000 | seed push | 2,854 | **1,997** |
| | clone | 2,046 | **1,369** |
| | 1-file push | 149 | **136** |
| | pull of one new main commit | 77 | **63** |
| | push throughput c=8 (ops/s) | 20.7 | **24.8** |
| 30,000 | seed push | 15,272 | **7,813** |
| | clone | 6,916 | **4,332** |
| | shallow clone | 7,854 | **4,269** |
| | 1-file push | 318 | **282** |
| | pull of one new main commit | 146 | **130** |
| | push throughput c=4 / c=8 (ops/s) | 10.1 / 12.5 | **11.2 / 16.1** |
| 128 MiB file | push / clone | 3,617 / 1,078 | **3,126 / 1,022** |

## What changed

Each change came from profiling `oak serve` and the client with `sample`.

1. **Published-object closure → indexed table.** Every push, pull, chunk
   fetch and blob check parsed one JSON metadata value holding every
   published commit/tree/blob/chunk hash, and every publish rewrote it whole,
   inside the writer lock. It is now `serve_published_objects` (migration
   0015, `WITHOUT ROWID` primary key). Membership is a cached point lookup
   and publishing appends only new rows. Existing repos migrate on first
   access, and a JSON cache written by an older binary after a downgrade is
   folded back in.
2. **Staged-session receipts → table.** Each staged-v1 envelope parsed,
   cloned twice, compared and re-serialized every session's tree/blob/chunk
   sets, so first publication was quadratic. The sets now live in
   `serve_stage_objects` (migration 0016). The session header JSON keeps
   only the boundary, totals and commits, and receipts are dropped when a
   session completes, aborts or expires. Listing stage IDs uses a recursive
   skip-scan rather than `DISTINCT`, which had reintroduced a full pass per
   envelope.
3. **Connection pool.** Every request opened SQLite, re-ran the migration
   check (16 statements), and closed it. Closing the last connection
   checkpoints and deletes the WAL, so every request paid a full checkpoint
   plus `unlink`. Idle connections are now pooled per repo file, keyed by
   inode so a deleted or moved repo is never reused. Delete and transfer
   evict their handles.
4. **One hash for single-chunk blobs** in `verify_stored_blob`. The chunk is
   the whole blob, so hashing it again proved nothing new.
5. **Parallel clone checkout** (client). Writing the working tree was ~85% of
   a large clone, spent in single-threaded `open`/`rename`/`fchmod`.
   Directories are still created serially, and blobs still come off the one
   SQLite connection in bounded batches, but each batch is written across
   threads.
6. **gzip** for JSON responses over 32 KiB (tower-http, fastest level; chunk
   bytes excluded). This is neutral on loopback and cuts pull/`blobs/info`
   bytes several-fold over a real network.

## Still slow, and why

- **1-file push grows with repo size (282 ms at 30k files).** The server is
  only ~45 ms of it. On the server, `admit_commit_file_changes` flattens the
  whole manifest to validate the commit's file list. On the client, the
  planner spends ~110 ms walking the snapshot and `commit` spends ~100 ms on
  its status scan (see `scripts/bench-vs-git/REPORT.md` item 4). A
  path-aligned manifest diff in both places would make it O(change).
- **Seed push is still ~240 round trips for 30k files** (128 blobs per
  staged envelope, each followed by a head re-check). Larger envelopes for
  small inline blobs, or not re-fetching the head between envelopes, would
  cut most of the remaining time.
- **First publication over 50,000 files is refused** (`STAGED_MAX_BLOBS`
  applies to the whole operation).
- **Clone of many small files** is now bound by APFS create+rename cost per
  file. Writing straight to the final path for a fresh, empty destination
  would skip the rename.
