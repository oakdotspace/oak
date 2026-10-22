# Manifest scaling measurements

Oak stores manifests as Merkle trees, so storage is shared across commits and
a commit writes O(changes × depth) tree objects. But the hot paths that *read*
manifests flatten the whole tree into one `Vec<ManifestEntry>`, so a 1-file
change costs O(files). This document puts numbers on that at 10k, 100k and 1M
files, says which path breaks first, and ranks the fixes. Fixes 1–3 and the
cheap wins have since been implemented; [After the fixes](#after-the-fixes)
has the same measurements re-run on that code.

Measured 2026-10-16 against `oakvcs-core` 0.105.0 (`oak/core`). The server
pins `oakvcs-core` 0.94.0 from crates.io. Its `flatten_tree`,
`three_way_merge_manifests`, `ManifestEntry` and `Hash` are identical to
0.105.0, so the core numbers apply to the server too.

**Labels.** Every number in a table comes from a run unless it says *(est.)*.
"Emulated" rows re-implement a server code path inside the bench, without
Postgres. They are measured CPU for that re-implementation, not measurements
of the live server.

## Summary

- **Every flattened path grows roughly linearly with repo size, at about
  0.2–5 µs per file.** At 1M files, a 1-file `diff_trees` takes **3.6 s** and peaks at
  **725 MiB**. `three_way_merge_manifests` takes **1.9 s** (423 MiB). One
  emulated server squash-merge attempt takes **5.3 s** of CPU (1.2 GiB), and
  the server can retry up to 4 times while it holds the merge lock for that
  target branch.
- **A tree-level walk that skips equal subtree hashes gives the same answers
  in 0.01–0.12 ms** at every size. It visits 2–3 × depth trees (at most 27 at
  1M). In all six cases (two scenarios × three sizes), the prototype merge's
  root hash matched the root hash of the flat merge.
- **Server memory breaks first.** `manifest_cache` holds up to 512 flattened
  manifests, with no byte limit. One cached 1M-file manifest is **244 MiB**,
  so a full cache would be **122 GiB**; at 100k files it would be 11.4 GiB.
  On a server with 8 GiB of RAM *(est.)*, about 33 distinct 1M-file manifests
  (or about 360 at 100k) exhaust memory. Ordinary merges and commit-page
  views fill the cache, and each cache hit then deep-copies the whole manifest
  (57 ms and 187 MiB at 1M).
- **Next to break: the server's per-level tree read budget, a hard error.**
  The server reads trees one BFS level at a time and fails a level over
  128 MiB decoded. The largest level measured 58.3 MiB at 1M files, so with
  this repo shape `get_manifest` on a cache miss starts failing near **2.2M
  files** *(est.)*. Repos with many files at a single depth hit it sooner.
- **Mount cold start is O(files) before the mount serves anything.** At 1M
  files it takes 10.4 s of local SQLite work, then 1,000 *sequential*
  `blobs/info` POSTs; at 50 ms RTT that totals about **60 s** *(est.)*.

## Setup

- **Machine and build:** Apple M1 Pro (10 cores), 16 GiB RAM, macOS (Darwin
  27.0.0), rustc 1.94.1 `--release`, single-threaded.
- **Bench:** a scratch binary, not committed. It links `oak/core` by path and
  calls the real `build_tree`, `flatten_tree`, `diff_trees`, `Manifest::new`,
  `three_way_merge_manifests` and `SqliteRepository`. Source and raw output
  are in the shared session notes folder (see
  [Reproducing](#reproducing)).
- **Timing:** median of 9, 5 and 3 runs at 10k, 100k and 1M files. Inputs are
  built outside the timed region.
- **Memory:** "peak" is the high-water mark of Rust heap bytes above the
  pre-op baseline, from a counting global allocator. C allocations (zstd
  contexts, SQLite page cache) are not counted. Process max RSS: about 2.5 GiB
  for the 1M core run, 0.94 GiB for the 1M mount run. Bytes per entry come from
  `malloc_size()`, i.e. the actual allocator block sizes.
- **Synthetic repo:** BFS-grown directories with 3–15 files and 2–8 subdirs
  each, using realistic names (`components/`, `handler_factory.cc`). Every
  file has a distinct BLAKE3 blob hash, and 5% are executable.
- **Change set:** the branch modifies one file. The parent (main) modifies a
  different file in another top-level directory, or, for the "same dir"
  case, a sibling of the branch's file.

| Shape | 10k | 100k | 1M |
|---|---|---|---|
| Avg path length (bytes) | 43.0 | 61.6 | 75.7 |
| Avg / max depth | 4.8 / 6 | 6.2 / 7 | 8.0 / 9 |
| Tree objects per version | 1,099 | 11,083 | 111,102 |
| Files per directory | 9.1 | 9.0 | 9.0 |

Calibration: `~/.cargo/registry/src` has 60,813 files, average path 45.1
bytes, average depth 3.9, and 4.8 files per directory. The synthetic 10k shape
matches it on path length. At 100k and 1M the synthetic repos are deeper than
that sample, so per-entry byte counts lean pessimistic. Real repos have
smaller directories (more trees per file), so per-tree costs (zstd, SQLite
queries) lean optimistic.

## Results: 1-file change

Each cell is median time · peak heap.

| Operation | 10k | 100k | 1M | Cost per file at 1M |
|---|---|---|---|---|
| `flatten_tree` (server-style cloning fetch) | 2.1 ms · 2.1 MiB | 23 ms · 22 MiB | 244 ms · 233 MiB | 0.24 µs |
| `Manifest::new` (sort + hash every tree) | 8.5 ms · 2.6 MiB | 106 ms · 27 MiB | 1,295 ms · 258 MiB | 1.3 µs |
| `build_tree`, validated (put_tree / prepare_manifest) | 14 ms · 2.6 MiB | 164 ms · 27 MiB | 2,016 ms · 258 MiB | 2.0 µs |
| **`diff_trees`** | **22 ms · 6.7 MiB** | **279 ms · 71 MiB** | **3,601 ms · 725 MiB** | 3.6 µs |
| **`three_way_merge_manifests`** (inputs prebuilt) | **3.7 ms · 4.3 MiB** | **115 ms · 38 MiB** | **1,896 ms · 423 MiB** | 1.9 µs |
| `get_manifest` cache hit (`(*entries).clone()`) | 0.43 ms · 1.6 MiB | 6.2 ms · 17 MiB | 57 ms · 187 MiB | 57 ns |
| Emulated `get_manifest` cache miss, CPU only¹ | 22 ms · 3.9 MiB | 246 ms · 40 MiB | 2,676 ms · 407 MiB | 2.7 µs |
| Emulated `server_merge_resolve` attempt, CPU² | 22 ms · 11 MiB | 344 ms · 118 MiB | 5,276 ms · 1,244 MiB | 5.3 µs |
| `Manifest::get`, miss (linear scan) | 0.01 ms | 0.06 ms | 0.94 ms | — |
| *Prototype:* tree-level diff | 0.01 ms (10 trees) | 0.01 ms (12) | 0.02 ms (18) | — |
| *Prototype:* tree-level merge, changes in different dirs | 0.01 ms (3 trees) | 0.01 ms (3) | 0.02 ms (3) | — |
| *Prototype:* tree-level merge, changes in same dir | 0.07 ms (15 trees, 5 new) | 0.10 ms (18, 6 new) | 0.12 ms (27, 9 new) | — |

¹ Emulates `collect_reachable_trees` + `flatten_tree`. Per BFS level it
creates a new zstd stream decoder for each tree (as
`tree_zstd_decompress_bounded` does), parses each tree, clones it into `out`
as the server does, then flattens with a cloning fetch. It also includes a
blake3 verify (141 ms at 1M) that the server's batched read does not do, so
the server's own CPU is about 2.5 s at 1M *(est.)*. Not included: the 6, 7 or
9 Postgres round trips (one per level), and transferring 0.57, 5.7 or
56.8 MiB of compressed tree rows.

² Three cache-hit clones (branch, parent, LCA), then
`three_way_merge_manifests`, the `base_by_path` map, `clean_entries.clone()`
and `build_tree`. The bench uses 0.105's *validating* `build_tree`, while the
server's 0.94 version skips validation. Comparing 0.105's `build_tree`
(2.0 s) with `Manifest::new` (1.3 s) at 1M, the server attempt is about 0.7 s
cheaper *(est.)*. Not included: blob text merges, Postgres, and
`finalize_squash_merge`. Retries cap at `SQUASH_MERGE_MAX_ATTEMPTS = 4`, so
the worst case is about 21 s of CPU at 1M and 1.4 s at 100k *(est.)*.

Where `diff_trees` spends its time at 1M, from the component rows *(est.)*:

| Part of `diff_trees` (1M files) | Time | Share |
|---|---|---|
| Two `Manifest::new` calls (re-hash trees whose root hashes it already has) | ≈2.6 s | ≈72% |
| Two `flatten_tree` calls | ≈0.5 s | ≈14% |
| The diff itself | ≈0.5 s | ≈14% |

`three_way_merge_manifests` grows faster than linearly from 10k to 100k
files (3.7 ms to 115 ms, 31×). Its three `HashMap<&str, _>` maps and the
all-paths set stop fitting in cache.

## Bytes per entry and `manifest_cache` footprint

`ManifestEntry` is 56 bytes inline (`path: String`, `blob_hash: Hash(String)`,
`mode`), plus a path allocation and a 64-byte hex hash allocation.

| | 10k | 100k | 1M |
|---|---|---|---|
| Walk output (what `manifest_cache` stores), bytes/entry | 220.9 | 238.9 | 255.8 |
| … of which path heap: requested / allocated | 58.3 / 65.1 | 93.7 / 101.5 | 121.9 / 133.0 |
| … of which hex hash heap | 64 | 64 | 64 |
| Exact clone (what a cache hit returns), bytes/entry | 171.9 | 189.1 | 203.2 |
| Same clone with a binary `[u8; 32]` hash, bytes/entry | 116.1 | 133.2 | 147.2 |
| **One cached manifest** | 2.1 MiB | 22.8 MiB | **244 MiB** |
| **Worst-case 512-entry cache** (512 × the line above) | **1.05 GiB** | **11.4 GiB** | **122 GiB** |
| Cached manifests that fill 8 GiB *(est.)* | 3,880 (more than the 512 cap) | ≈360 | ≈33 |
| Parsed tree objects, one full version (`HashMap<hash, Tree>`) | 1.9 MiB | 18.6 MiB | 184 MiB |
| zstd tree rows, one full version | 0.57 MiB | 5.7 MiB | 56.8 MiB |
| New tree objects per 1-file commit | 5 (5.8 KiB) | 6 (7.5 KiB) | 9 (8.6 KiB) |

The code comment at `postgres.rs:381` sizes the cache for manifests of "a few
hundred small entries". With 10k-file manifests a full cache already exceeds
1 GiB.

Walk output is 26–29% larger than an exact clone, for two reasons:
- `flatten_recursive` builds each path with `format!("{}/{}")`, which
  reserves about twice the prefix length (122 bytes requested for paths that
  average 75.7 bytes at 1M).
- The `Vec` keeps its doubling slack: capacity 16,384 for 10,000 entries.

The cache stores the walk output as-is, so it keeps both kinds of waste.

A byte-bounded **tree-object** cache instead pays for one full version once,
then about 9 small trees for each further 1-file commit. For 512 such
versions of a 1M-file repo that is about 184 MiB + 511 × 9 × 1.7 KiB ≈
**191 MiB** parsed, or less than 60 MiB as zstd rows *(est.)*. The flattened
cache would need 122 GiB for the same versions.

## Mount cold start (`rebuild_base_sizes`)

Run against a real `SqliteRepository::open_relaxed` holding the head tree. The
"fresh" case has no blob rows or chunk refs, so every unique blob misses
twice: `get_blob_chunks`, then `get_blob`. The "warm/resume" case has chunk
refs for every blob, so every blob hits `get_blob_chunks`.

| Step | 10k | 100k | 1M |
|---|---|---|---|
| `cache.get_manifest` (one SQLite `get_tree` per dir, each doing zstd + blake3 + parse) | 27 ms · 2.2 MiB | 288 ms · 22 MiB | 3,116 ms · 233 MiB |
| Size loop, fresh (all miss) | 69 ms | 715 ms | 7,236 ms |
| Size loop, warm (all chunk hits) | 43 ms | 575 ms | 7,237 ms |
| `blobs/info` POSTs needed, sequential, 1,000 hashes each | 10 | 100 | 1,000 |
| Request JSON for those POSTs *(est.)* | 0.64 MiB | 6.4 MiB | 63.9 MiB |
| **Fresh mount, time before serving, at 50 ms RTT** *(est.)* | ≈0.6 s | ≈6 s | **≈60 s** |
| **Resume, time before serving** (local only, measured parts) | 0.07 s | 0.86 s | **10.4 s** |

The size loop costs about 7 µs per blob either way. Its two lookups re-prepare
their SQL on every call.

One code observation, not measured: when a blob is present locally but has no
chunk refs, the loop calls `get_blob`, which reads and decodes the full
content just to get its size. `get_blob_size` already exists for this.

## What breaks first

1. **Server memory, through `manifest_cache`, from about 100k files.** The
   cache is bounded by entry count, and each entry costs 22.8 MiB at 100k
   files and 244 MiB at 1M. Normal traffic fills it: each merge loads three
   manifests (branch, parent, LCA), and each view of a distinct commit adds
   one. It ends in an OOM, not a slowdown.
2. **Merge and diff latency, from about 1M files.** Per call this is 3.6 s for
   `diff_trees` and 5.3 s per squash-merge attempt, plus Postgres reads on
   cache misses. At 100k files the same calls take 0.28 s and 0.34 s, which is
   tolerable. Merges into the same target branch run one at a time, because
   each squash merge holds that target's merge lock (`squash_merge_gate`).
3. **The hard read-budget error at about 2.2M files** *(est., shape
   dependent)*. `get_trees_for_integrity_with_policy` fails any BFS level over
   128 MiB decoded or 2M entries. The largest level measured 0.68, 5.1 and
   58.3 MiB (7.7k, 57k and 661k entries) at the three sizes.
4. **Mount startup: about 6 s at 100k and about 60 s at 1M** *(est.)*. This
   is a usability cost, not a failure.

## Ranked fixes

1. **A byte-bounded tree-object cache instead of flattened manifests.**
   - **Fixes:** the first failure. The worst case drops from 122 GiB to about
     0.2 GiB at 1M *(est.)*.
   - **Effort:** small, and server-only (`manifest_cache` plus
     `get_manifest`).
   - **Also do:** return a shared `Arc<[ManifestEntry]>` or a borrowed view
     instead of `(*entries).clone()`. That saves 57 ms and 187 MiB per hit at
     1M.
   - **Interim step:** keep the flattened cache but bound it by entry bytes.
     That alone removes the OOM.
   - **Limit:** flattening still costs about 0.24 µs per file, so CPU stays
     O(files) until fix 2 lands.
2. **Tree-level diff and merge that skip equal subtree hashes.**
   - **Fixes:** the CPU problem. Measured: `diff_trees` 3,601 ms → 0.02 ms and
     merge 1,896 ms → 0.02–0.12 ms at 1M, with peak memory going from hundreds
     of MiB to effectively zero.
   - **Cost becomes:** O(changes × depth × fanout) instead of O(files).
   - **Effort:** `diff_trees` keeps its signature, so it is a drop-in. Merge
     is more work: the server and CLI consume the flat `clean_entries`, so
     they need a result that is a new root hash plus the changed or
     conflicting paths.
   - **Interim step:** stop `diff_trees` from calling `Manifest::new` on both
     flattened sides (≈72% of its time). Check one subtlety first: rename
     pairing follows entry order, and flatten order differs slightly from
     path-sorted order.
3. **Lazy, per-directory blob sizes in mounts.**
   - **Fixes:** today, startup does O(files) SQLite lookups and N/1000
     sequential round trips. Lazy sizes would cost O(directory size) on first
     access instead, about 9 entries here, or one batched lookup per
     directory.
   - **Gain:** removes 10.4 s of local work at 1M before the mount serves,
     plus most of the about 50 s spent in `blobs/info` *(est.)*.
   - **Why third:** it only affects mounts and is a usability cost, not a
     failure.
4. **Binary hashes.**
   - **Gain:** −56 bytes per entry (−27 to −33%) and about 1.9× faster manifest
     clones (56.7 → 30.5 ms at 1M), both measured. It also shrinks tree maps
     keyed by hex `String`.
   - **Effort:** the highest. `Hash(pub String)` is public API, used in wire
     JSON and SQLite TEXT columns.
   - **Why last:** it is a constant-factor gain only and does not change any
     O(files) path.

### Cheap wins found along the way

These were not in the original list. Each is small and independent.

- **Reuse the zstd decoder context.** Creating a stream decoder per tree takes
  1,632 ms at 1M; one reused `bulk::Decompressor` does the same work in
  218 ms (7.5× faster). That is about 61% of the emulated cache-miss CPU. The
  SQLite `get_tree` has the same pattern (`zstd::decode_all` per call), and
  it dominates the 3.1 s mount `get_manifest`.
- **Size path strings exactly in `flatten_recursive` and `shrink_to_fit`
  before caching.** That is 21–22% fewer bytes per cached manifest (255.8 →
  about 203 bytes per entry at 1M).
- **Stop calling `Manifest::get` in per-path loops.** Each miss is a full
  scan (0.94 ms at 1M), so loops over touched paths cost O(touched × files).
  Examples: revert in `oak-web/src/server/api/repos.rs:10486` (two scans per
  path), and mount status in `cli/src/commands/mount/mod.rs:1708`. Index by
  path when looping.
- **Use `get_blob_size` in `rebuild_base_sizes`** instead of `get_blob`, so
  locally present blobs don't have their full content decoded.

## After the fixes

Measured 2026-10-18 on the same machine and the same synthetic repos, with a
second scratch bench linked against the patched core. The code is on two
branches, pushed but not merged: `manifest-tree-walks` in oak/oak (core and
CLI) and `manifest-cache-bytes` in oak/oakspace (server).

| 1-file change | 10k | 100k | 1M |
|---|---|---|---|
| `diff_trees` (now a tree walk) | 22 ms → **0.01 ms** | 279 ms → **0.01 ms** | 3,601 ms · 725 MiB → **0.02 ms** · ~0 |
| Merge, changes in different dirs: `three_way_merge_manifests` → `three_way_merge_trees` | 3.7 ms → **0.01 ms** | 115 ms → **0.01 ms** | 1,896 ms · 423 MiB → **0.02 ms** |
| Same merge, both changes in one dir | → 0.07 ms | → 0.11 ms | → 0.14 ms |
| Trees read by the diff / by the same-dir merge | 10 / 15 | 12 / 18 | 18 / 27 |
| `flatten_tree` | 2.1 → 1.3 ms | 23 → 17 ms | 244 → 190 ms, 233 → 189 MiB |
| Path heap per flattened entry (bytes) | 58.3 → 43.0 | 93.7 → 61.6 | 121.9 → 75.7 |
| SQLite `get_manifest` (CLI and mount walk) | 27 → 12 ms | 288 → 130 ms | 3,116 → 1,407 ms |
| `oak status` branch change set¹ | 26.6 → **0.11 ms** | 300 → **0.23 ms** | 3,550 → **0.18 ms** |
| Mount (macOS/Linux): local work before serving² | 0.1 s → none | 1.0 s → none | 10.4 s → none |
| Server cache miss: zstd decode of all trees³ | 14 → 2 ms | 150 → 20 ms | 1,632 → 218 ms |
| Server `manifest_cache`, worst case | 1.05 GiB → ≤ 512 MiB | 11.4 GiB → ≤ 512 MiB | 122 GiB → ≤ 512 MiB |

¹ "Before" is the old code path (two `get_manifest` calls, then a flat diff)
run on the patched core, so it already includes the decoder speedup. On the
unpatched core it would be about 2 × 3.1 s plus the diff at 1M *(est.)*.

² By construction: startup no longer runs the size scan, and resume no longer
flattens the head manifest. Instead, the first listing of each directory pays
one `get_tree`, about 7 µs per file without a local size (the measured
per-blob cost of the old scan), and one `blobs/info` request when any size is
unknown *(est.)*.

³ Measured in the original bench's breakdown (a new streaming decoder per
tree vs. one reused context). The server's new fast path is the reused
context. A unit test checks it returns exactly what the streaming decoder
returns for valid, corrupt, empty, truncated, concatenated and over-limit
rows.

What changed, by fix:

1. **Server caches.** `manifest_cache` is now also bounded at 512 MiB, and
   walk results are shrunk before caching. A new `tree_content_cache` (256 MiB,
   second-chance eviction) holds stored tree rows for manifest walks, so a
   miss on a new commit fetches only the rows it doesn't share with cached
   commits. The batch read's metadata query, budgets, decode and validation
   still run over the same bytes in the same order. Integrity reads never use
   it.
2. **Tree-level diff and merge.** `diff_trees` returns exactly what
   flatten-and-diff returned, rename pairing included; 2,000 randomized cases
   check this. `three_way_merge_trees` is new. It takes subtrees changed on
   one side, or identically on both, whole by hash, and falls back to the
   flat merge for entries whose kind differs across sides. Its root and
   conflicts match `three_way_merge_manifests` plus `build_tree` in 3,000
   randomized cases. `oak status` now diffs trees.
3. **Lazy mount sizes.** Each directory's unknown sizes come from one batched
   `blobs/info` call, made without the inode lock, the first time the
   directory is listed. One remote failure stops further attempts for the
   session.
4. **Binary hashes:** not done (see below).

Cheap wins: the reused zstd context (SQLite client and server), exact-length
paths in `flatten_recursive`, an indexed revert loop on the server, and
`get_blob_size` instead of `get_blob` for mount size probes.

Still O(files), as follow-ups:

- **CLI merge flows, the parts still flat.** `oak merge` into a local
  parent, `oak pull`'s parent sync, re-parenting and `oak split` now merge
  roots with `three_way_merge_trees` and move the working tree by tree diff
  (`cli/src/commands/merge_tree.rs`). Pull's branch fast-forward and the
  dirty guards followed (see [Pull fast-forward and dirty
  guards](#pull-fast-forward-and-dirty-guards)), then the working-tree moves
  of `oak switch`, `oak switch -c`, in-place `oak split` and `oak branch
  --from` (see [Branch moves](#branch-moves-switch-split-branch---from)).
  What still costs O(files): the `--continue` paths (they rescan the working
  tree and flatten the recorded result), `oak pull --force` and `oak switch
  --clean` (they rewrite every file on purpose), the working-tree walk every
  dirty check makes, `oak status` and `oak commit` themselves, and merge
  previews, whose four-tree safety classifier still takes whole manifests
  (only the merge inside them is tree-level).
- **The server squash merge** stays flat until an oak-core release includes
  `three_way_merge_trees` and the server bumps to it.
- **`oak diff <rev> <rev>`** still flattens: its renderer looks up base
  entries by path, and the external difftool materializes both trees.
- **The server's 128 MiB per-level read budget is unchanged**, so its hard
  error near 2.2M files *(est.)* remains. Splitting a large frontier into
  budget-sized batches is a separate change.
- **Cache hits still deep-copy the manifest**, because `get_manifest` returns
  an owned `Manifest`. Returning a shared one changes the `AsyncRepository`
  trait.
- **Binary hashes**, and **Windows (ProjFS) mounts**, which still size every
  blob up front.

Validation:

- **oak:** 2,150 tests pass. The one failure,
  `native_ci_build_budget_preserves_coverage_without_restoring_target`,
  already fails on main: it looks for a `cache:` block that main's
  `.oak/workflows/ci.yml` doesn't have.
- **oakspace:** all 1,326 nextest tests pass against a local Postgres.
- **Both repos:** `cargo fmt` and `clippy -D warnings` are clean.

## Pull fast-forward and dirty guards

Measured 2026-10-21, same machine, `--release`, with a scratch bench that
calls `pull_async` against a mocked server (phase 2 of `oak pull`: clean
guard, working-tree update, ref publication) and the clean check directly.
The working tree has 100k or 1M real files, about 10 per directory, about
50 bytes each, with a warm stat cache. Upstream edits one file. "Before" is
branch `tree-merge-cli`; "after" is the change described here. Medians of
3 runs at 100k and the mean of 2 at 1M; the breakdown rows, timed
separately, give the range over 2 runs.

| | 100k | 1M |
|---|---|---|
| `oak pull` phase 2, one-file fast-forward | 10.9 s → **0.36 s** | 126 s → **6.9 s** |
| Dirty guard (`oak merge`, pull, fetch, sparse) | 1.01 s → **0.31 s** | 15.1 s → **6.4 s** |
| … the working-tree walk and stat-cache load, both before and after | 0.28–0.54 s | 5.3 s |
| … hashing the scan into trees in memory (after only) | 0.07 s | 0.71 s |
| … flattening HEAD (before only) | 0.09–0.12 s | 1.0–1.5 s |
| … reading every HEAD blob to prove it local (before only) | 0.45–0.60 s | 5.4–6.2 s |
| `oak status` (unchanged) | 1.0 s | 15–17 s |

What changed:

1. **The fast-forward moves the tree by diff.** `pull::update_working_dir`
   used to flatten both heads, check every in-cone blob, and hand all 1M
   entries to the materializer, which read each file back to prove it
   unchanged. It now diffs the two roots (`WorktreeDiff::between`), runs the
   same restricted, known-loss and `OAK_ALLOW_PARTIAL_CLONE` preflight, with
   the same messages, over the changed files only, and writes those. The
   clean guard before it is what makes that safe: it proved the tree holds
   the old head. `--force` skips the guard, so it still rewrites every file.
   A file the old head withheld (restricted, declared lost) whose blob this
   pull brings in is no exception: the guard runs after the download, sees
   the absent file as a deletion and refuses, as before; `--force` lands it.
2. **The dirty guards prove a clean tree by tree hash.**
   `worktree_is_clean_without_storing_blobs` hashes the scanned files into
   trees in memory and compares the root with HEAD's. In a sparse checkout
   it reads only HEAD's directories on the way to the cone's prefixes, so
   out-of-cone trees and blobs can be missing. When the proof fails (a real
   change, a withheld file, a foreign hash format like the git backend's),
   the full status comparison runs over the same scan, so a dirty answer and
   its errors are `oak status`'s. `oak merge` now uses this check instead of
   the whole status pipeline. The phase 2 pull above is now dominated by it.
   `oak switch`, `oak switch -c` (whose yes/no decides between carrying a
   dirty tree and materializing `main`), `oak split` and `oak branch --from`
   have since moved to it too; none of them used the change list. Their
   guard costs what the row above shows, and their working-tree moves have
   since followed the fast-forward onto tree diffs (see [Branch
   moves](#branch-moves-switch-split-branch---from)).

The guard still walks every file: finding an edit or an untracked file
without a filesystem watcher means a stat per file and the whole stat
cache. That is the 5.3 s left at 1M.

Because the check no longer fails on a missing HEAD blob, it stopped
standing in for a blob preflight. That matters where refs move before the
working tree: in-place `oak split` publishes the rewritten branch head
before writing the new tip, and `oak branch --from` stores the branch row
before writing the seed. Both now check, before anything moves, the blobs
that working-tree write needs (`WorkingDirMove::require_blobs` today, which
skips withheld files as the write does). The old guard only covered blobs
HEAD shared with the target, so a split that dropped a commit could already
move refs and then stop on a blob only the new tip had. `oak switch` and
`oak switch -c` already wrote the working tree before any ref.

### Why `oak status` and `oak commit` still flatten HEAD

Status and commit fail closed (`IncompleteBlobData`) when any in-cone HEAD
blob is missing locally, even one whose file sits unchanged on disk. The
check is there so a partly materialized tree (files never written because
their blobs never arrived) can't be committed as a mass deletion. A tree
comparison skips equal subtrees by hash, so it never sees those blobs; keeping
the check means visiting every entry, which is the O(files) work the
comparison exists to avoid.

Checking only the blobs of changed paths would keep the partial-tree case:
a file that was never written reads as deleted, and its blob gets checked.
It would drop one thing: today every `oak status` reports a blob missing
under an unchanged file (a damaged store); with the tree comparison only a
command that reads that blob would. The dirty guards accept that, because
their callers read and check the blobs they use. For status and commit it is
a weaker guarantee, so it stays a decision for later, not part of this
change.

A cheaper step that keeps the guarantee: the status check proves presence
with `get_blob`, which reads and decompresses every blob (5.4–6.2 s of the
15 s above, with 50-byte files; real file sizes cost more). An existence
query would answer the same question without the bytes. It would stop
catching a stored blob that fails to decode, which `get_blob` notices in
passing today.

## Branch moves: switch, split, branch --from

Measured 2026-10-21, same machine and repo shape as the pull bench above
(`--release`, warm stat cache), with a scratch bench that runs each command
whole, dirty guard included, against a target that differs from HEAD by one
file. "Before" is branch `tree-clean-guards`; "after" is the change described
here. 100k: the median of 3 runs before and the range of 5 after; 1M: the
mean of 2 runs before and the range of 3 after.

| One-file move, whole command | 100k | 1M |
|---|---|---|
| `oak switch NAME` | 5.9 s → **0.34–0.46 s** | 135 s → **6.4–7.0 s** |
| `oak switch -c` (materializing `main`) | 5.4 s → **0.35–0.43 s** | 128 s → **6.4–9.2 s** |
| `oak branch --from` (`branch::new_branch` with a seed)¹ | 5.9 s → **0.34–0.44 s** | 139 s → **6.4–7.4 s** |
| In-place `oak split` (drop a commit) | 5.9 s → **0.34–0.44 s** | 132 s → **6.5–7.6 s** |
| … of which the dirty guard (from the table above) | 0.31 s | 6.4 s |

¹ Not a CLI flag: `oak switch NAME`'s prompt to create a missing branch off
the server's `main` reaches it.

What changed: those four commands moved the working tree the way `oak pull`
used to, through `switch::update_working_dir`: flatten the target, read every
blob to prove it local, hand every file to the materializer (which reads each
one back to prove it unchanged), then walk the tree twice to delete untracked
files and empty directories. Their dirty guard already proves the tree holds
the snapshot it compared against, so `switch::WorkingDirMove` now diffs that
root (the current branch's effective head, or HEAD when detached) to the
target's with `WorktreeDiff::between`, writes the files that differ, deletes
the tracked files the target drops, and prunes only the directories that
empties. What remains is the guard's walk.

- **Where the whole-tree rewrite stays.** `oak switch --clean` and `oak
  switch -c --clean` skip the guard, so they rewrite everything, as `oak pull
  --force` does. So does a move whose guard snapshot isn't stored locally,
  and one whose diff reaches a tree missing locally: under a directory the
  target drops, the diff needs HEAD's tree to list the files to remove, while
  the rewrite needs only the target's data, as it always has.
- **Withheld content is unchanged.** Restricted and declared-lost files in
  the diff are skipped with the same warnings, and a lost one is still
  removed from disk. Unlike the rewrite, the diff move checks every blob it
  writes before the first write even when some are withheld, so it can't stop
  halfway.
- **Hydration and ordering.** `oak switch` fetches from the remote only the
  blobs the move writes. In-place `oak split` and `oak branch --from` still
  plan the move and check its blobs before any ref or branch row moves; the
  check now covers only the files the move writes. Because the diff trusts
  the dirty check, `oak switch` and `oak branch --from` now hold the workdir
  lock from the check on. `oak split` can't (its plan editor runs in
  between), so it re-reads the snapshot under the lock and rewrites the
  whole tree if the branch moved meanwhile.
- **Untracked content the guard doesn't see now survives.** That is empty
  directories and non-regular files such as sockets and FIFOs: the scan
  skips them, so the guard passes, and the old rewrite's sweep deleted them.
  A regular untracked file never reaches the move, because the guard refuses
  it (`oak switch -c` carries the tree instead of materializing). Git's
  checkout leaves untracked empty directories alone too, and nothing relied
  on the sweep. `--clean` still sweeps.

Not changed:

- **Case-only renames on a case-insensitive filesystem** (the macOS
  default) lose the file under the diff and the old rewrite alike, and in
  `oak pull` and `oak merge`, which share the materializer: the file on disk
  keeps its old-case name, which then reads as untracked (rewrite) or matches
  the dropped path (diff), and is deleted. A pre-existing materializer bug.
- **`oak switch NAME` for a branch that isn't local yet** first pulls it with
  no old head to diff from, which writes every file of the fetched head into
  the working tree. The move after it is exact, but that path is still
  O(files).

## Caveats

- These are synthetic repos, and the shape matters. Per-file costs scale with
  path length and depth, and per-tree costs scale with directory count.
- Peak memory counts only the Rust heap. Process RSS was higher: about 2.5 GiB
  for the 1M core run.
- Server numbers cover CPU only. Postgres latency and transfer come on top of
  them (cache misses, BFS round trips).
- The prototypes are scratch code that only show the achievable cost. The
  merge prototype reports file-vs-directory kind clashes as conflicts and does
  not run rename detection.
- The 8 GiB server size is illustrative, not the real deployment size.

## Reproducing

The bench source (`main.rs`, `Cargo.toml`) and the raw TSV output are in the
session notes folder:
`~/Library/Application Support/Claude/side-session-notes/local_643637fc-6a54-4bf2-ada6-958c2f93df8d/manifest-bench/`.
Build them with `CARGO_TARGET_DIR` outside any mount, then run
`manifest-bench core <N>` and `manifest-bench mount <N> <scratch-dir>`. The
after-the-fixes bench is `manifest-bench-after/` beside it (`core <N>` and
`sqlite <N> <scratch-dir>`), with its output in `results-after/`.

The [pull fast-forward and dirty guard](#pull-fast-forward-and-dirty-guards)
bench is two `cargo` examples, `ffbench.rs` and `scanbench.rs`, in
`~/Library/Application Support/Claude/side-session-notes/local_13bcb90f-0d26-4bb7-aca3-0a6d1a4cc10e/worktree-bench/`,
with a README and the raw output. Copy them into `cli/examples/` of each
checkout being compared and build with
`cargo build --release -p oakvcs-cli --example ffbench`.

The [branch move](#branch-moves-switch-split-branch---from) bench is one
example, `movebench.rs`, in
`~/Library/Application Support/Claude/side-session-notes/local_c6cd07e7-bc71-4a05-a962-36ddc2f08874/move-bench/`,
with its raw output. Build it the same way and run `movebench <N>
<scratch-dir> [runs]`; both builds can share one scratch repo.
