# Binary hashes: design

`oak-core`'s `Hash` is a newtype over a hex `String`
(`pub struct Hash(pub String)`, `core/src/hash.rs:24`). Every hash costs a
24-byte `String` plus a 64-byte heap allocation (40 bytes for a git SHA-1), and
every clone of a hash allocates again. This document proposes storing hashes
as 33 inline bytes instead, while hex remains the only external form. It
covers the representation, how hex stays byte-identical at every boundary,
what replaces `as_str()`, `.0` and the tuple constructor, a step-by-step
migration, and the compatibility cost for the published `oakvcs-core` crate
and for oakspace.

**Status:** proposal, not scheduled. This is fix 4 ("binary hashes") from
[manifest-scaling-measurements.md](manifest-scaling-measurements.md). Fixes
1–3 shipped on 2026-10-20. Fix 4 was split out because it is a breaking change
to a published crate, and it buys only a constant factor.

**Baseline:** oak `main` at `15bcc581` (`oakvcs-core` 0.105.0). The oakspace
working copy pins `oakvcs-core` 0.94.0 from crates.io.

**Labels:** every number comes from a run unless it says *(est.)*.

## Summary

- **Representation:** `struct Hash { width: Width, bytes: [u8; 32] }`, where
  `Width` is a `#[repr(u8)]` enum (20 or 32). It is 33 bytes, `Copy` and has
  no heap allocation. A git SHA-1 uses the first 20 bytes, and the unused tail
  is always zero. The tag records only the width, never the algorithm, so
  equality means exactly what it means today.
- **Gain (measured):** −56 bytes per manifest entry (−27 to −33%) and
  1.6–1.9× faster manifest clones. Hex parsing is 1.8× faster, and building a
  `HashSet` of hashes is 1.6× faster. Nothing that is O(files) becomes
  cheaper.
- **Hex stays the only external form:** serde, wire JSON, SQLite and Postgres
  TEXT columns, the canonical tree bytes and the commit preimages are all
  byte-identical. No hash changes, nothing needs a database migration, and
  mixed client and server versions keep working.
- **Call sites (counted by the compiler, not grep):** 1,173 `as_str()`, 192
  `.0`, 449 constructor uses and 125 `short()`, across oak and oakspace
  (1,939 in all). 47% are test code.
- **`as_str()` cannot survive**, because there is no hex string left to
  borrow. It becomes `to_hex()`, which returns a 65-byte stack value that
  derefs to `str`. In a pilot on oak, 93% of `as_str()` sites compiled after a
  one-token rewrite. Five regex patterns fixed another 5%, and 12 sites (2%)
  needed hand edits.
- **Migration:** add the new API, migrate the call sites, deprecate the old
  API, ship one breaking release with storage unchanged, then switch the
  storage. The switch does not change the public API.
- **Recommendation:** do it, but not now. First, oakspace has to catch up from
  0.94 to the current core. Then ship the break as part of the next
  intentional breaking core release, ideally the one that makes `get_manifest`
  return a shared manifest. The additive steps can start any time. See
  [Recommendation](#recommendation).

## What it buys

Numbers are from the earlier report (2026-10-16), plus a re-run on the same
machine (Apple M1 Pro) on 2026-10-21 against `main`. The re-run used a
scratch bench with the same synthetic repo generator (see
[Reproducing](#reproducing)). Clone times are medians of 7 runs at 1M entries.

| | `Hash(String)` today | proposed `{width, [u8; 32]}` | inline hex `{len, [u8; 64]}` (alternative) |
|---|---|---|---|
| `size_of::<ManifestEntry>()` | 56 | 64 | 96 |
| Hash heap per entry | 64 | 0 | 0 |
| Exact clone, bytes/entry at 1M | 203.2 | **147.2** (−56, −27.6%) | 179.2 (−24, −11.8%) |
| Exact clone, bytes/entry at 100k | 189.1 | 133.2 (−29.6%) | 165.1 |
| Clone 1M entries, earlier report | 56.7 ms | 30.5 ms (1.86×) | — |
| Clone 1M entries, re-run | 41–43 ms | 26–27 ms (1.6×) | 29–30 ms (1.4×) |
| Parse 1M hex hashes, with validation | 62.8 ms, 83.9 MiB | 34.2 ms, 31.5 MiB | — |
| Build a `HashSet` of 1M hashes | 139.7 ms, 111 MiB | 86.2 ms, 68 MiB | — |
| Hash → hex | free (borrow) | 10.7 ns | free (borrow) |
| Canonical tree bytes for 1M entries | 25.5 ms | 35.9 ms | 25.5 ms *(est.)* |

The 33-byte proposed form measured the same as a bare `[u8; 32]`. The width
tag costs nothing because `ManifestEntry` and `TreeEntry` round up to 64 bytes
either way. With a `#[repr(u8)]` enum as the tag, `Option<Hash>` is also
33 bytes.

What this means in practice:

- **Server caches hold more.** `manifest_cache` is now bounded by bytes
  (512 MiB). The same budget holds 1.38× more entries at 1M files and 1.48×
  more at 10k files. Maps keyed by hash shrink too, once their keys stop being
  hex `String`s.
- **Every hash computation stops allocating.** `hash_bytes` and
  `hash_string` (about 575 call sites by grep) build a hex `String` today.
  Afterwards they copy 32 bytes.
- **Encoding hex costs a little more.** Writing canonical tree bytes costs
  about 10 ns more per entry. At 1M entries that is +10 ms, against 1,295 ms
  for `Manifest::new`, so under 1%.
- **The asymptotics do not change.** Since fixes 1–3, the O(files) paths that
  remain are the ones that still flatten: `get_manifest` cache hits deep-copy
  the manifest, `oak diff <rev> <rev>` flattens it, and Windows mounts size
  every blob up front. This change makes each of them about 1.6× cheaper; it
  does not remove them.

## What `Hash` is today

- **Kinds.** `HashKind` is inferred from the hex length. 64 hex characters is
  BLAKE3, the hash the SQLite and Postgres backends produce. 40 hex
  characters is a git SHA-1, produced by the git backend. `GitSha256` is
  declared, but nothing produces it, because `gix` is built with only its
  `sha1` feature. Length alone cannot tell BLAKE3 from SHA-256, and `kind()`
  says so in its docs.
- **Traits.** `Debug`, `Clone`, `PartialEq`, `Eq`, `std::hash::Hash`,
  `Display`, `Serialize` and `Deserialize`. It has no `Copy`, `Ord`,
  `Default`, `AsRef<str>`, `Borrow<str>` or `Deref`, and no `ToSql` or
  `FromSql`. So no code relies on looking a hash up as a `&str` key, and no
  derived `Ord` depends on it.
- **Validation.** `from_hex` and serde accept only lowercase hex of length 40
  or 64. The 0.94.0 release that oakspace uses is looser: its `from_hex`
  accepts uppercase, and its `Deserialize` accepts any string
  (`Ok(Hash(s))`). The tuple constructor `Hash(s)` has never validated.
- **Where hex is observable.** None of these may change:
  1. serde: wire JSON (`core/src/protocol.rs`) and stored JSON.
  2. SQLite TEXT columns (`core/src/migrations/sqlite`): `blobs`, `chunks`,
     `blob_chunks`, `trees`, `tree_entries`, the four hash columns of
     `commits`, `commit_files`, `branch_heads` and `tags`.
  3. Postgres (oakspace): every hash column in the 128 migrations is
     `text` (one is `varchar(64)`). None is `bytea`.
  4. Canonical tree bytes, `kind\tname\thash\tmode`
     (`serialize_entries`, `core/src/tree.rs:240`). These bytes are both the
     tree-hash preimage and the stored `trees.content`, so the stored trees
     embed hex too.
  5. Commit preimages, v1 and v2 (`hash_commit`,
     `core/src/hash_format.rs:156`). Golden tests pin both.
  6. CLI text and `--json` output, URLs, and 12-character short hashes.
- **Out of scope.** Many protocol DTOs already carry hashes as plain
  `String`s, for example `ChunkRefData.hash` (`core/src/protocol.rs:190`).
  They do not use `Hash`, so they neither change nor benefit.

## Call sites

### Method

`grep` cannot tell `Hash::as_str()` from `String::as_str()`, or `Hash.0` from
any other tuple's `.0`. So the counts come from the compiler. In scratch
copies of `oakvcs-core` (0.105.0 for oak, 0.94.0 for oakspace via
`[patch.crates-io]`), I marked the struct, `as_str`, `short`, `from_hex` and
`kind` `#[deprecated]`. Then I ran `cargo check --all-targets
--message-format=json` and kept one warning per source span. The warning text
tells the use kinds apart. "Field" means `.0` access or a destructuring
pattern. "Tuple struct" means a constructor call or `Hash` used as a function
(`.map(Hash)`). `use` imports are dropped. A second run with only
`--lib --bins` decides which sites are production code. Everything else is
test-only code: `#[cfg(test)]` modules, `tests/` and benches.

The checks ran on macOS. Linux FUSE and Windows ProjFS mount code is not
compiled there; grep finds about 7 more sites in it. oakspace was counted in
the working copy as it stood on 2026-10-21.

### Counts (production + test)

| Crate | `as_str()` | `.0` | constructor | `short()` | `from_hex` (unaffected) |
|---|---|---|---|---|---|
| oak `core` | 110 + 36 | 2 + 3 | 29 + 9 | 1 + 0 | 16 + 12 |
| oak `cli` | 130 + 336 | 27 + 6 | 58 + 116 | 84 + 20 | 66 + 31 |
| oakspace `oak-storage` | 163 + 10 | 20 + 0 | 90 + 9 | 0 | 0 |
| oakspace `oak-web` | 128 + 260 | 94 + 40 | 69 + 69 | 20 + 0 | 2 + 1 |
| **Total** | **531 + 642 = 1,173** | **143 + 49 = 192** | **246 + 203 = 449** | **105 + 20 = 125** | 128 + 44 |

For comparison, grep finds 2,567 `.as_str()` calls and 1,882 `.0` accesses in
the same trees. Most are on other types.

### What the sites look like

- **`as_str()`.** Most are expression-scoped. That covers SQL parameters,
  `format!` and `json!` arguments, `push_str`, comparisons and set lookups.
  205 of oakspace's 561 are sqlx `.bind(hash.as_str())`. The problem sites
  keep the `&str` beyond the statement: `opt.map(|h| h.as_str())`, and
  `HashSet<&str>` or `HashMap<&str, _>` built from hashes.
- **`.0`.** 68 are `.0.clone()` into a `String`. The other 124 borrow
  `&h.0` to look up a `String`-keyed map or cache, compare it with an
  `Option<String>` DTO field, slice it (`&h.0[..12.min(h.0.len())]`) or
  destructure it.
- **Constructors.**

  | Source | Production | Test |
  |---|---|---|
  | An existing `String` (`Hash(s)`, `Hash(x.clone())`) | 128 | 49 |
  | `.map(Hash)` over `Option<String>` | 63 | 0 |
  | A database row (`Hash(row.get(..))`) | 32 | 0 |
  | A gix object id (`Hash(oid.to_string())`) | 13 | 0 |
  | BLAKE3 `to_hex().to_string()` | 5 | 0 |
  | A string literal (`"nonexistent"`, `"abc123".repeat(..)`) | 1 | 141 |
  | `format!` (`"{index:064x}"`, `"manifest-{nonce}"`) | 0 | 13 |
  | Empty placeholder `Hash(String::new())` | 4 | 0 |

## Design

### Representation

```rust
/// Width of the digest. A hash's algorithm is not recorded (see `kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
enum Width {
    Sha1 = 20,
    Full = 32,
}

/// A content hash: a 32-byte BLAKE3 or git SHA-256 digest, or a 20-byte git
/// SHA-1 digest. Bytes past the width are always zero, so the derived `Eq`
/// and `Hash` compare only meaningful bytes.
#[derive(Clone, Copy, PartialEq, Eq, std::hash::Hash)]
pub struct Hash {
    width: Width,
    bytes: [u8; 32],
}
```

- **Why the tag records width and not algorithm.** Today two hashes are equal
  when their hex is equal. A hash read from JSON or SQLite has no algorithm
  context, so a parser would tag every 64-character value as BLAKE3. If the
  tag were an algorithm, those values would compare unequal to the same bytes
  from a SHA-256 git backend. A width tag keeps today's semantics, and
  `kind()` keeps its current best-effort answer.
- **Why not a bare `[u8; 32]`.** The git backend's 20-byte SHA-1 ids go
  through the same `Hash` in the `Repository` trait. A zero-padded SHA-1
  without a tag cannot be told apart from a BLAKE3 hash that ends in 12 zero
  bytes. The tag is free (see the size row above).
- **Why not separate types, such as `Hash` and `GitOid`.** That is stronger
  typing, but the `Repository` trait would need to become generic over the id
  type. That is a much larger change than the one that wins the memory, and
  nothing currently needs it.
- **Ordering.** Implement `Ord` by hand over `as_bytes()`, not by deriving it.
  For lowercase hex, byte order is the same as string order. That holds
  between widths too, because a shorter prefix sorts first in both. So any
  code that sorts hashes by hex, or migrates to sorting `Hash`es, gets the
  same order.
- **`Debug`.** Print `Hash("<hex>")`, the same shape the derived `Debug`
  prints today, so logs and assertion messages read the same.

### API

```rust
impl Hash {
    pub const fn from_blake3_bytes(bytes: [u8; 32]) -> Hash;
    pub fn from_bytes(bytes: &[u8]) -> Result<Hash>;      // 20 or 32 bytes
    pub fn from_hex(hex: &str) -> Result<Hash>;           // unchanged contract
    pub fn as_bytes(&self) -> &[u8];                      // 20 or 32 bytes
    pub fn to_hex(&self) -> HashHex;                      // replaces as_str()
    pub fn short(&self) -> HashHex;                       // 12 chars; was &str
    pub fn kind(&self) -> HashKind;                       // unchanged
    pub fn hex_starts_with(&self, prefix: &str) -> bool;  // prefix match, no allocation
}
impl From<blake3::Hash> for Hash;
#[cfg(feature = "local-repo")] impl From<&gix::oid> for Hash;
#[cfg(feature = "local-repo")] impl rusqlite::types::FromSql for Hash;
impl FromStr for Hash;                     // = from_hex
impl Ord, PartialOrd, Display, Debug, Serialize, Deserialize for Hash;

/// Lowercase hex of a `Hash` on the stack: 40 or 64 bytes, or 12 for `short()`.
#[derive(Clone, Copy)]
pub struct HashHex { buf: [u8; 64], len: u8 }
impl Deref<Target = str>, AsRef<str>, Display, Debug, PartialEq<str>,
     PartialEq<&str>, PartialEq<String> for HashHex;
#[cfg(feature = "local-repo")] impl rusqlite::ToSql for HashHex;  // borrowed Text
```

`hash_bytes` and `hash_string` keep their signatures. The empty-tree and
empty-blob hashes can become `const`s.

### Keeping hex at every boundary

| Boundary | Today | After |
|---|---|---|
| serde `Serialize` | `serialize_str(&self.0)` | `serialize_str(&self.to_hex())`, no allocation |
| serde `Deserialize` | `String` allocation, then `from_hex` | a visitor that decodes `&str` in place, with the same errors |
| SQLite write | `params![h.as_str()]` | `params![h.to_hex()]`; `ToSql for HashHex` borrows the stack buffer |
| SQLite read | `Hash(row.get::<_, String>(i)?)`, unchecked | `row.get::<_, Hash>(i)?` through `FromSql`, which decodes the TEXT bytes in place |
| Postgres (oakspace) | `.bind(h.as_str())`, `Hash(row.get(..))` | `.bind(h.to_string())`, and a helper that calls `Hash::from_hex` |
| Canonical tree bytes | `push_str(e.hash.as_str())` | write 40 or 64 hex bytes from a lookup table into the buffer |
| Tree parse | `Hash::from_hex(field)`, allocates | decode into the entry, no allocation |
| Commit preimages | `as_str()` and `{}` | `to_hex()` and `{}`; the golden tests in `hash_format.rs` must pass unchanged |
| CLI output | `Display`, `short()` | the same text |

Notes on these boundaries:

- **sqlx.** `oakvcs-core` has no sqlx dependency, so it cannot implement
  `sqlx::Encode` or `sqlx::Decode`. The orphan rule stops oakspace from
  implementing them for `Hash` as well. So oakspace binds an owned `String`.
  sqlx's `'q` bind lifetime makes a borrowed `HashHex` awkward wherever a
  query is built across statements. One allocation per bind does not matter
  next to a Postgres round trip.
- **serde stays hex in every format.** It must not branch on
  `is_human_readable()`. Only JSON is in use today. Keeping a single
  representation means no future cache file or binary format silently gets a
  different encoding.
- **Reads become strict.** A non-canonical hash in a database row now fails
  when it is read, instead of flowing through. That is the main behavior
  change; see [Compatibility](#compatibility).

### The hex codec

Use about 40 lines of code with no new dependency. Decode through a 256-entry
table that maps any non-lowercase-hex byte to `0xff`, OR the results together,
and check the OR once at the end. Encode through a 256-entry table of
two-byte pairs. The implementation matters. A first decoder used a `match`
and `?` per nibble, and it took 205 ms per million hashes, 3× slower than
today's validate-and-allocate `from_hex` (62.8 ms). The table version takes
34.2 ms. Ship it with a test that round-trips all 256 byte values and checks
that uppercase and non-hex input are rejected.

### Replacing `as_str()`

`as_str(&self) -> &str` borrows from the hex `String`. Once the storage is
binary, there is nothing to borrow. These were the options:

| Option | Verdict |
|---|---|
| `to_hex() -> HashHex`, a 65-byte `Copy` value that derefs to `str` | **Chosen.** No allocation. Most sites change by one token. |
| Return `String` | An allocation on every call, including the hot SQLite binds and preimage building. Worse than today. |
| Cache the hex lazily inside `Hash` (`OnceLock<Box<str>>`) | Adds at least 16 bytes and the allocation comes back. That defeats the purpose. |
| A global intern table, bytes → `&'static str` | Leaks memory and adds contention. |
| Store hex inline (`[u8; 64]`) and keep `as_str()` | Viable, and covered under [Alternatives](#alternatives-considered). It saves less than half as much memory. |

**Pilot.** I applied the rewrite to oak's 612 sites in a scratch copy, with
`to_hex()` returning `HashHex` by value over the existing `String` storage.
That gives exactly the lifetimes the binary form will have. Then I ran
`cargo check --all-targets`:

- **572 sites (93%)** compiled after `h.as_str()` became
  `h.to_hex().as_str()`. The 372 test sites all fall in this group or the
  next one.
- **28 sites (5%)** needed one of five regex patterns. One of them, inside a
  test's `assert_eq!`, also needs `PartialEq` and `Debug` on `HashHex`, both
  of which are in the API above.
  - `opt.map(|h| h.as_str())` passed as a SQL parameter became
    `opt.map(|h| h.to_hex())`, which `ToSql for HashHex` covers (17 sites).
  - The same closure followed by `.unwrap_or_default()`, or passed to
    `opt_field`, needed `.as_deref()` added (7 sites).
  - `.map(Hash::as_str)` became `.map(Hash::to_hex)` (2 sites).
  - `let s = h.as_str();` became `let s = h.to_hex();` (2 sites).
- **12 sites (2%)**, all in the CLI, needed hand edits. Ten keep hashes
  in a `HashSet<&str>` or `HashMap<&str, _>`: merge ancestry walks (7 sites
  in `cli/src/commands/merge.rs:1045–1116`), `mount/core.rs:580`,
  `mount/repo_cache.rs:95` and `sync.rs:1874`. The fix is to key them by
  `Hash`. That is better anyway, because the new `Hash` is `Copy` and its
  hashing is cheaper than a string's. The other two are `Option` fallbacks
  such as `.map(..).unwrap_or("none")` (`status.rs:452`, `change.rs:848`).

In oakspace, the 205 `.bind(h.as_str())` sites become `.bind(h.to_string())`
mechanically. The rest should break down the way oak's did.

Nobody has to find these sites by hand. Running the deprecation harness
against the release that marks `as_str()` deprecated produces the exact list
of spans, and a script can rewrite them. Only the sites that then fail to
borrow-check need a person, and the compiler names those too.

### Replacing `.0`, the constructor and `short()`

- **`.0.clone()`** becomes `h.to_string()` (68 sites; same allocation as
  today).
- **`&h.0` used as a string** becomes `&*h.to_hex()`, or better, the map or
  cache gets re-keyed by `Hash`. That second option is where server memory
  savings compound: `String`-keyed tree and blob caches in oakspace. Sliced
  short forms become `h.short()`.
- **`Hash(s)` from a `String`** becomes `Hash::from_hex(&s)?`, which can fail.
  Each of the 246 production constructor sites needs a decision: propagate
  the error, or use `expect` where the input is trusted, such as the
  database's own rows. `.map(Hash)` becomes
  `.map(|s| Hash::from_hex(&s)).transpose()?`. This is the slowest part of
  the migration, because it is not mechanical.
- **Database rows** use `FromSql` in core. oakspace gets a small
  `fn hash_col(row, name) -> Result<Hash>` helper.
- **gix ids** become `Hash::from(oid)`, which copies bytes without a hex round
  trip (`core/src/git.rs`, 13 sites). **BLAKE3 output** becomes
  `Hash::from(hasher.finalize())` (5 sites).
- **Test literals.** Literals that are not valid hex (141 test sites, and one
  in production) cannot be represented. Most are lookups of an absent object
  (`Hash("nonexistent".to_string())`), so they become a valid but unstored
  hash such as `hash_string("nonexistent")` or
  `Hash::from_blake3_bytes([0xee; 32])`. Literals already shaped like hashes,
  such as `format!("{index:064x}")`, work through `from_hex`. The hash.rs
  tests that build non-hex `Hash`es on purpose (`"é".repeat(20)`, `"abc"`)
  are deleted, because those states can no longer exist.
- **Empty placeholders.** oakspace builds an empty manifest with
  `Hash(String::new())` in 4 production sites (`branch_detail.rs:334` and
  `:2059`, `commit_pages.rs:210` and `:722`). These become `Manifest::empty()`,
  which already exists (`core/src/lib.rs:229`) and uses the real empty-tree
  hash.
- **`short()`** returns `HashHex` instead of `&str` (125 sites). Most are
  `format!` arguments and compile unchanged, because `HashHex` implements
  `Display`.

### `Copy`

The binary `Hash` should be `Copy`. Adding `Copy` makes
`clippy::clone_on_copy` fire on every `hash.clone()`, and both repos run
clippy with `-D warnings`. A name-based grep finds about 800 such calls in oak
and about 700 in oakspace *(est.; this overcounts)*. `cargo clippy --fix`
rewrites them mechanically. Keep that diff in its own step so the storage
change stays small enough to review.

## Migration plan

The goal is one breaking release, taken while storage is still `String`, so
that switching storage afterwards is a change inside core that downstream
code never sees. Every step leaves both repos building and their tests
passing.

0. **Prerequisites.**
   - oakspace moves from `oakvcs-core` 0.94.0 to the current core. It is 11
     breaking (0.x minor) releases behind, and it needs this upgrade anyway
     for the server-side squash merge on `three_way_merge_trees`.
   - Audit stored data. Check that every hash column, in Postgres and in a
     sample of client SQLite databases, matches
     `^([0-9a-f]{40}|[0-9a-f]{64})$`. Under 0.94.0, deserialization accepted
     any string and `from_hex` accepted uppercase, so the server may hold rows
     that the strict reader will refuse. Fix or quarantine any it finds before
     step 2.
1. **Additive API in core** (non-breaking; oak's CLI moves with it).
   - a. Add `HashHex`, `to_hex`, `as_bytes`, `from_bytes`,
     `from_blake3_bytes`, `hex_starts_with`, `FromStr`, `Ord` (on the string
     for now), `From<blake3::Hash>`, `From<&gix::oid>`, `ToSql for HashHex`
     and `FromSql for Hash`.
   - b. Migrate core: 146 `as_str`, 5 `.0`, 38 constructors, 1 `short`.
   - c. Migrate the CLI in batches, one command module at a time: 466
     `as_str`, 33 `.0`, 174 constructors, 104 `short`.
   - d. Mark `as_str()` and the `.0` field `#[deprecated]`. A tuple field can
     carry `#[deprecated]`: field access and patterns then warn, but
     constructor calls do not, so step 3 still needs its own break. Once oak
     has no uses left, cut a release. The deprecations stop new uses in oak
     and give oakspace an exact list of what to change.
2. **oakspace migrates** on that release: 561 `as_str`, 154 `.0`, 237
   constructors, 20 `short`. Its CI turns deprecation warnings into errors, so
   the version bump and the migration land together, split by module.
3. **Breaking release.** Make the field private. Remove `as_str()` and the
   tuple constructor. Change `short()` to return `HashHex`. Storage is still
   `String`, so behavior is identical. Code that finished step 2 compiles
   unchanged, except any `short()` site that holds the `&str` past its
   statement; there are few, because most are `format!` arguments.
4. **Switch storage** inside `hash.rs`. Add the hex table codec and in-place
   decoding for serde and `FromSql`. Remove the hex round trip from
   `hash_bytes`. The `hash_format` golden tests and the tree and commit
   integrity tests must pass unchanged. Re-run the manifest bench, expecting
   about 147 bytes per entry and 1.6–1.9× faster clones. The public API does
   not change, so this can ship in an ordinary release, and rolling it back
   means pinning the previous version.
5. **Cleanups.** Derive `Copy` and run `clippy --fix`. Re-key `String`-keyed
   hash maps and caches by `Hash`, in core, the CLI and the oakspace server
   caches. Make the empty-tree and empty-blob hashes `const`.

**Effort** *(est.)*: the pilot suggests about 2 weeks for one engineer.
Steps 1a–1d take about 3–4 days, mostly the CLI's constructor decisions.
Step 2 takes about 3–4 days. Steps 3–5 take about 2 days, mostly review of
mechanical diffs.

## Compatibility

- **Wire protocol:** unchanged. JSON still carries 40- or 64-character
  lowercase hex strings. Old and new clients and servers interoperate in any
  combination, so no capability flag is needed.
- **Storage:** no migration. The SQLite and Postgres TEXT columns and the
  stored tree bytes stay byte-identical, so downgrading is safe.
- **Hashes:** no object hash changes. The golden tests for the v1 and v2
  preimages guard this.
- **Behavior changes:**
  1. A non-canonical hash in a database row, or anywhere a `Hash` used to be
     built unchecked, now fails when it is read, instead of propagating.
     Step 0's audit exists for this reason.
  2. `Ord` is new.
  3. `short()` returns `HashHex`.
  4. `Debug` output is unchanged.
- **Published crate (`oakvcs-core`):**
  - Steps 1, 4 and 5 are additive.
  - Step 3 is breaking, so it ships as a 0.x minor bump.
  - Its consumers are the `oak` CLI, which moves with it in the same
    workspace; oakspace; the Unity integration mentioned in the workspace
    `Cargo.toml`, which uses `default-features = false`; and any other
    crates.io users. The release notes need an old → new table:
    `as_str()` → `to_hex()`, `.0.clone()` → `to_string()`,
    `Hash(s)` → `Hash::from_hex(&s)?`, and `Hash(String::new())` →
    `Manifest::empty()` or `Option<Hash>`.
- **oakspace:**
  - It is the largest downstream user: 972 sites, half of the 1,939 total.
  - It is already 11 breaking releases behind, which is why step 0 comes
    first.
  - Its Postgres schema does not change.
  - `manifest_cache` stores `Arc<Vec<ManifestEntry>>`, so its values shrink
    as soon as step 4 ships. Its keys, and those of `tree_content_cache`,
    are `(repo_id, String)`, and those shrink only once they are re-keyed by
    `Hash` (step 5).
- **Git:** only SHA-1 is produced today. The width tag already fits a future
  SHA-256 backend. Whether a 32-byte hash is BLAKE3 or SHA-256 stays as
  ambiguous as it is now.

## Alternatives considered

- **Inline hex (`{len: u8, buf: [u8; 64]}`, keep `as_str()`).** This keeps
  the 1,173 `as_str()` sites as they are. Only `.0` and the constructors
  change, about 640 sites. It removes the per-hash allocation, so clones get
  1.4× faster (29–30 ms at 1M). But it saves only 24 bytes per entry
  (179.2 bytes/entry), because the entry grows to 96 bytes. It also needs the
  same breaking release, since the field must become private. So it pays the
  same downstream cost for under half the memory gain. Worse, moving to
  binary later would mean a second break, because `as_str()` would still be
  public. It is still a fallback if oakspace's `as_str()` migration stalls.
- **`Arc<str>` or `Box<str>`:** still on the heap, and it gains nothing over
  `String` except a cheaper clone.
- **Binary columns (`BLOB`/`bytea`) and a binary wire format:** these halve
  index sizes. But they need a database migration and a protocol change. They
  are independent of this design and could follow it.
- **Separate types for git and BLAKE3 ids:** see
  [Representation](#representation).

## Recommendation

**Yes, but not now.** Make it one breaking `oakvcs-core` release, scheduled
after two things happen:

1. **oakspace runs a current core.** Adding a break on top of an
   11-release gap makes the server upgrade harder, and the server needs that
   upgrade first for tree-level squash merges.
2. **There is a reason to break anyway.** The best companion is making
   `get_manifest` return a shared manifest (`Arc`) instead of a deep copy,
   which is also a trait change in core. That change removes cache-hit clones
   entirely, so it beats binary hashes' 1.6–1.9× clone speedup. Bundle the two
   so downstream pays for one break. What binary hashes then add on top is
   −27 to −33% memory per manifest entry and per cached hash, plus faster
   parsing and hashing.

Pull it forward if a profile or the server's byte-bounded caches show hash
storage as the next bottleneck. Two concrete triggers: steady eviction at the
512 MiB `manifest_cache` budget for active repos, or `String` hash allocation
showing up in a merge or cache-miss profile. Until then, the fixes that
change O(files) paths should come first.

Steps 1a–1c can start at any time. They are additive, they touch only oak,
and they reduce the eventual break to a version bump for the CLI.

## Reproducing

The source and raw output are in the shared session notes folder
(`side-session-notes/local_ff9506bf-…/binary-hashes/`), outside the repo:

- `hashbench/`: a scratch crate that links `oak/core` by path, reuses the
  manifest bench's generator, and measures clone, parse, encode, preimage and
  `HashSet` costs for each representation. Run it with
  `hashbench <N>`. `results/` holds the 100k and 1M runs; the
  `*-naive-hex.tsv` files are from the first decoder.
- `counts/`: per-site JSON (`oak-sites.json`, `space-sites.json`) and the
  scripts that turn `cargo check --message-format=json` output into the tables
  above. To regenerate, add `#[deprecated(note = "HB_…")]` to `Hash`,
  `as_str`, `short`, `from_hex` and `kind` in a scratch copy of core. Point the
  workspace at it (oak directly; oakspace via `[patch.crates-io]`). Run
  `cargo check --workspace --all-targets` and then `--lib --bins` with
  `--message-format=json`, and run `parse2.py` and `final.py`.
