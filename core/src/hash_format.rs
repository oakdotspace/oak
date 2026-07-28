//! Content-addressing format versions: the v1 (current) and v2 (prepared,
//! dormant) hash preimages for every Oak object kind.
//!
//! # Why this module exists
//!
//! **v1 — what ships today — has two structural weaknesses.**
//!
//! 1. *No domain separation.* Every object kind (blob, chunk, tree, commit)
//!    hashes its raw bytes with bare BLAKE3, so identical bytes in different
//!    roles collide: the empty blob, the empty-tree sentinel, and the empty
//!    chunk share one hash; a small file's single chunk hash equals its blob
//!    hash; a blob whose content happens to be a valid canonical tree
//!    serialization hashes identically to that tree. Git prefixes every
//!    object with `"<type> <len>\0"` for exactly this reason.
//!
//! 2. *Ambiguous commit preimage.* The commit hash covers
//!    `branch\nparent\nmerge\nmanifest\nauthor\nmessage\ntimestamp` joined by
//!    `\n`. Fields containing `\n` can shift bytes between positions (two
//!    distinct commits, one preimage), and the `files: Vec<FileChange>` list
//!    is excluded entirely, so the per-commit file-change rows can diverge
//!    between client and server without changing the commit hash.
//!
//! The compatible half of the fix landed alongside this module: tree entry
//! names are validated (no `\t`/`\n`/`\0`/`/`, no `.`/`..`, non-empty),
//! `Hash` deserialization requires real lowercase hex, and commit creation
//! rejects `\n`/`\t`/`\0` in branch names and authors (both are Oak-generated
//! or config values, so nothing legal is rejected). With branch, author, and
//! the RFC 3339 timestamp all `\n`-free, the message — the one field that may
//! legitimately contain newlines — can no longer bleed into a neighbor, so
//! the v1 preimage is unambiguous *for data Oak will now accept*. What
//! validation cannot fix is domain separation and the excluded files list:
//! those change every hash, hence v2.
//!
//! **v2 preimages** (all BLAKE3, all length-safe):
//!
//! - blob:   `"oak-blob\0"   ++ content`
//! - chunk:  `"oak-chunk\0"  ++ content`
//! - tree:   `"oak-tree\0"   ++ canonical entry bytes` (same line format as v1)
//! - commit: `"oak-commit\0" ++ field ++ field ++ ...` where every field is
//!   length-prefixed (`{decimal byte length}:{bytes}`, netstring-style) and
//!   optional fields encode as a single `-` byte when absent — so `None` and
//!   `Some("")` are distinct and no field can impersonate its neighbor
//!   regardless of content. Field order: branch name, parent hash, merge
//!   parent hash, manifest hash, author, message, RFC 3339 timestamp, then
//!   the file-change list (a length-prefixed count followed by each change's
//!   path, type letter, old/new blob hashes, old path, and old/new modes,
//!   every one length-prefixed or `-`). Including the files list makes the
//!   commit hash commit to its `commit_files` rows.
//!
//! # Versioning and migration story
//!
//! The format is a repo-level flag: [`HashFormat::from_metadata`] parses the
//! `hash_format` metadata value ([`crate::MetadataKey::HashFormat`]), and an
//! absent value means v1, so every existing repository — including the Oak
//! repo itself and everything on the server — keeps hashing and verifying
//! byte-identically. **Nothing writes that key yet**: no CLI command, no
//! migration. v2 cannot be enabled today, only exercised through this module
//! in tests.
//!
//! Flipping a repo to v2 is a full re-hash of every object (blob, chunk,
//! tree, commit) plus rewrite of every row keyed by hash, so it must be an
//! explicit offline migration (`oak upgrade --hash-format=v2` or similar),
//! never a default. Mixed-format push/pull is not supported: the server and
//! client must agree on the repo's format, which the protocol will need to
//! advertise before any migration ships. Until then, the golden tests in
//! this module pin the v1 preimages so no refactor can drift them silently.

use crate::error::{OakError, Result};
use crate::hash::{hash_bytes, Hash};
use crate::{ChangeType, FileChange, FileMode};
use chrono::{DateTime, Utc};

/// Repository-level content-addressing format. See the module docs for the
/// preimage definitions and the migration story.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HashFormat {
    /// Bare BLAKE3 over raw bytes; `\n`-joined commit fields, files excluded.
    /// The format of every existing repository.
    #[default]
    V1,
    /// Domain-prefixed BLAKE3, length-prefixed commit fields, files included.
    /// Prepared but not enabled anywhere — no CLI command or migration can
    /// switch a repo to it yet.
    V2,
}

impl HashFormat {
    /// Parse the repo-level `hash_format` metadata value. Absent means v1 —
    /// the format of every repository written before the flag existed. An
    /// unrecognized value is an error (a newer repo this build can't read),
    /// not a silent fallback.
    pub fn from_metadata(value: Option<&str>) -> Result<Self> {
        match value {
            None | Some("v1") => Ok(HashFormat::V1),
            Some("v2") => Ok(HashFormat::V2),
            Some(other) => Err(OakError::Config(format!(
                "unknown hash format {other:?} (this build understands v1 and v2)"
            ))),
        }
    }

    /// Canonical metadata value for this format.
    pub fn as_str(&self) -> &'static str {
        match self {
            HashFormat::V1 => "v1",
            HashFormat::V2 => "v2",
        }
    }
}

/// Hash a blob's content under the given format.
pub fn hash_blob(format: HashFormat, content: &[u8]) -> Hash {
    domain_hash(format, b"oak-blob\0", content)
}

/// Hash a chunk's content under the given format.
pub fn hash_chunk(format: HashFormat, content: &[u8]) -> Hash {
    domain_hash(format, b"oak-chunk\0", content)
}

/// Hash a tree's canonical entry bytes (see [`crate::Tree::canonical_bytes`])
/// under the given format.
pub fn hash_tree(format: HashFormat, canonical_bytes: &[u8]) -> Hash {
    domain_hash(format, b"oak-tree\0", canonical_bytes)
}

fn domain_hash(format: HashFormat, domain: &[u8], content: &[u8]) -> Hash {
    match format {
        HashFormat::V1 => hash_bytes(content),
        HashFormat::V2 => {
            let mut preimage = Vec::with_capacity(domain.len() + content.len());
            preimage.extend_from_slice(domain);
            preimage.extend_from_slice(content);
            hash_bytes(&preimage)
        }
    }
}

/// Everything a commit hash is derived from. Borrowed view over
/// [`crate::Commit`]'s fields so `Commit::with_timestamp` can hash before
/// constructing the struct.
#[derive(Clone, Copy)]
pub struct CommitFields<'a> {
    pub branch_name: &'a str,
    pub parent_hash: Option<&'a Hash>,
    pub merge_parent_hash: Option<&'a Hash>,
    pub manifest_hash: &'a Hash,
    pub author: &'a str,
    pub message: Option<&'a str>,
    pub timestamp: &'a DateTime<Utc>,
    /// Hashed under v2 only; v1 ignores it (its historical weakness).
    pub files: &'a [FileChange],
}

/// Hash a commit under the given format.
pub fn hash_commit(format: HashFormat, f: &CommitFields) -> Hash {
    match format {
        HashFormat::V1 => {
            // The shipped preimage, byte for byte. `None` hashes like the
            // empty string for parents and message (`None` vs `Some("")` is
            // documented as indistinguishable in v1), and `files` is absent.
            let content = format!(
                "{}\n{}\n{}\n{}\n{}\n{}\n{}",
                f.branch_name,
                f.parent_hash.map(|h| h.as_str()).unwrap_or_default(),
                f.merge_parent_hash.map(|h| h.as_str()).unwrap_or_default(),
                f.manifest_hash,
                f.author,
                f.message.unwrap_or_default(),
                f.timestamp.to_rfc3339()
            );
            hash_bytes(content.as_bytes())
        }
        HashFormat::V2 => {
            let mut p = Vec::new();
            p.extend_from_slice(b"oak-commit\0");
            field(&mut p, f.branch_name.as_bytes());
            opt_field(&mut p, f.parent_hash.map(|h| h.as_str()));
            opt_field(&mut p, f.merge_parent_hash.map(|h| h.as_str()));
            field(&mut p, f.manifest_hash.as_str().as_bytes());
            field(&mut p, f.author.as_bytes());
            opt_field(&mut p, f.message);
            field(&mut p, f.timestamp.to_rfc3339().as_bytes());
            field(&mut p, f.files.len().to_string().as_bytes());
            for c in f.files {
                field(&mut p, c.path.as_bytes());
                field(&mut p, change_type_str(c.change_type).as_bytes());
                opt_field(&mut p, c.old_blob_hash.as_ref().map(|h| h.as_str()));
                opt_field(&mut p, c.new_blob_hash.as_ref().map(|h| h.as_str()));
                opt_field(&mut p, c.old_path.as_deref());
                opt_field(&mut p, c.old_mode.map(mode_str));
                opt_field(&mut p, c.new_mode.map(mode_str));
            }
            hash_bytes(&p)
        }
    }
}

/// Hash a commit under the **frozen legacy v1 preimage** that predates
/// `merge_parent_hash` (commits created before ~April 2026, when the merge
/// parent entered the preimage). Six newline-joined fields — identical to
/// today's v1 except the `merge_parent` line is absent entirely:
///
/// ```text
/// branch\nparent\nmanifest\nauthor\nmessage\ntimestamp.to_rfc3339()
/// ```
///
/// This format is used **only** to verify peer-claimed hashes of commits
/// from that era (all of which have no merge parent); nothing may ever hash
/// a new commit with it. `f.merge_parent_hash` and `f.files` are ignored —
/// callers must gate on `merge_parent_hash` being absent.
pub fn hash_commit_v1_pre_merge_parent(f: &CommitFields) -> Hash {
    let content = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        f.branch_name,
        f.parent_hash.map(|h| h.as_str()).unwrap_or_default(),
        f.manifest_hash,
        f.author,
        f.message.unwrap_or_default(),
        f.timestamp.to_rfc3339()
    );
    hash_bytes(content.as_bytes())
}

/// Append one length-prefixed field: `{decimal byte length}:{bytes}`.
fn field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(bytes.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(bytes);
}

/// Append an optional field: `-` when absent, length-prefixed when present
/// (so `None` ≠ `Some("")` — `-` vs `0:`).
fn opt_field(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        None => out.push(b'-'),
        Some(v) => field(out, v.as_bytes()),
    }
}

fn change_type_str(t: ChangeType) -> &'static str {
    match t {
        ChangeType::Added => "A",
        ChangeType::Modified => "M",
        ChangeType::Deleted => "D",
        ChangeType::Renamed => "R",
    }
}

fn mode_str(m: FileMode) -> &'static str {
    match m {
        FileMode::Regular => "regular",
        FileMode::Executable => "executable",
        FileMode::Symlink => "symlink",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_string;
    use crate::{Commit, FileMode, ManifestEntry, TreeEntry, TreeEntryKind};
    use chrono::TimeZone;

    fn fixed_timestamp() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap()
    }

    fn sample_files() -> Vec<FileChange> {
        vec![FileChange {
            path: "src/lib.rs".to_string(),
            change_type: ChangeType::Modified,
            old_blob_hash: Some(hash_string("old")),
            new_blob_hash: Some(hash_string("new")),
            old_path: None,
            old_mode: Some(FileMode::Regular),
            new_mode: Some(FileMode::Regular),
        }]
    }

    // ---- v1 stability (golden values + equivalence with the live paths) ----
    //
    // STOP: the hex values pinned below are the v1 content-addressing format,
    // which is FROZEN. Every object on every existing server (oak.space and
    // every user's repos) was hashed this way. If a change to the hashing code
    // makes one of these assertions fail, the fix is to REVERT the change, not
    // to update the expected value — updating it re-breaks compatibility with
    // all stored data (clones/mounts fail with "commit hash mismatch"). A new
    // format must be a new `HashFormat` variant behind an explicit migration,
    // never an edit to these constants. See the module docs for why.

    /// BLAKE3 of the empty input — the v1 hash shared by the empty blob, the
    /// empty-tree sentinel, and the empty chunk (the v1 weakness v2 removes).
    const V1_EMPTY: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";

    #[test]
    fn v1_blob_chunk_tree_are_bare_blake3() {
        for content in [&b""[..], b"hello world", b"\x00\x01\xff"] {
            let expected = hash_bytes(content);
            assert_eq!(hash_blob(HashFormat::V1, content), expected);
            assert_eq!(hash_chunk(HashFormat::V1, content), expected);
            assert_eq!(hash_tree(HashFormat::V1, content), expected);
        }
    }

    #[test]
    fn v1_empty_golden() {
        assert_eq!(hash_blob(HashFormat::V1, b"").as_str(), V1_EMPTY);
        assert_eq!(crate::Tree::empty_hash().as_str(), V1_EMPTY);
    }

    #[test]
    fn v1_blob_golden() {
        // Pinned current value: BLAKE3("hello world").
        assert_eq!(
            hash_blob(HashFormat::V1, b"hello world").as_str(),
            "d74981efa70a0c880b8d8c1985d075dbcbf679b99a5f9914e5aaf96b831a9e24"
        );
    }

    #[test]
    fn v1_tree_golden() {
        let tree = crate::Tree::new(vec![
            TreeEntry {
                name: "a.txt".to_string(),
                kind: TreeEntryKind::Blob,
                hash: hash_string("a"),
                mode: FileMode::Regular,
            },
            TreeEntry {
                name: "sub".to_string(),
                kind: TreeEntryKind::Tree,
                hash: hash_string("sub"),
                mode: FileMode::Regular,
            },
        ])
        .unwrap();
        // Pinned current value for this fixed two-entry tree.
        assert_eq!(
            tree.hash.as_str(),
            "26fa3b9ad707a970f25bb9821cde463e244307bfe8234a1237eed50a421e85ea"
        );
        assert_eq!(
            hash_tree(HashFormat::V1, &tree.canonical_bytes()),
            tree.hash
        );
    }

    #[test]
    fn v1_commit_matches_live_commit_path_and_golden() {
        let manifest = hash_string("manifest");
        let files = sample_files();
        let commit = Commit::with_timestamp(
            "main".to_string(),
            None,
            None,
            manifest.clone(),
            "tester".to_string(),
            Some("a message".to_string()),
            files.clone(),
            fixed_timestamp(),
        )
        .unwrap();
        let ts = fixed_timestamp();
        let via_module = hash_commit(
            HashFormat::V1,
            &CommitFields {
                branch_name: "main",
                parent_hash: None,
                merge_parent_hash: None,
                manifest_hash: &manifest,
                author: "tester",
                message: Some("a message"),
                timestamp: &ts,
                files: &files,
            },
        );
        assert_eq!(commit.hash, via_module);
        // Pinned current value for these fixed inputs.
        assert_eq!(
            commit.hash.as_str(),
            "6b575aadf3ed7e96f242a521087baf9498a67e3b107ee6a2880ab321151912fb"
        );
    }

    #[test]
    fn v1_manifest_root_golden() {
        let manifest = crate::Manifest::new(vec![ManifestEntry {
            path: "dir/file.txt".to_string(),
            blob_hash: hash_string("content"),
            mode: FileMode::Regular,
        }]);
        // Pinned current value: root tree hash of this one-file manifest.
        assert_eq!(
            manifest.hash.as_str(),
            "863eb9a88032134f08722aa08b4aaa346d7408c17dbea4597957175bf907ab2f"
        );
    }

    // ---- v2 properties ----

    #[test]
    fn v2_domains_separate_identical_bytes() {
        for content in [&b""[..], b"same bytes"] {
            let blob = hash_blob(HashFormat::V2, content);
            let chunk = hash_chunk(HashFormat::V2, content);
            let tree = hash_tree(HashFormat::V2, content);
            assert_ne!(blob, chunk);
            assert_ne!(blob, tree);
            assert_ne!(chunk, tree);
            // And none of them collide with v1's bare hash of the same bytes.
            let v1 = hash_bytes(content);
            assert_ne!(blob, v1);
            assert_ne!(chunk, v1);
            assert_ne!(tree, v1);
        }
    }

    #[test]
    fn v2_empty_blob_differs_from_empty_tree() {
        assert_ne!(
            hash_blob(HashFormat::V2, b""),
            hash_tree(HashFormat::V2, b"")
        );
    }

    #[test]
    fn v2_commit_includes_files() {
        let manifest = hash_string("manifest");
        let files = sample_files();
        let ts = fixed_timestamp();
        let base = CommitFields {
            branch_name: "main",
            parent_hash: None,
            merge_parent_hash: None,
            manifest_hash: &manifest,
            author: "tester",
            message: None,
            timestamp: &ts,
            files: &[],
        };
        let without = hash_commit(HashFormat::V2, &base);
        let with = hash_commit(
            HashFormat::V2,
            &CommitFields {
                files: &files,
                ..base
            },
        );
        assert_ne!(without, with);
        // v1 ignores files: the same two inputs collide there (the known
        // weakness this format version exists to fix).
        let v1_without = hash_commit(HashFormat::V1, &base);
        let v1_with = hash_commit(
            HashFormat::V1,
            &CommitFields {
                files: &files,
                ..base
            },
        );
        assert_eq!(v1_without, v1_with);
    }

    #[test]
    fn v2_none_and_empty_message_differ() {
        let manifest = hash_string("manifest");
        let ts = fixed_timestamp();
        let base = CommitFields {
            branch_name: "main",
            parent_hash: None,
            merge_parent_hash: None,
            manifest_hash: &manifest,
            author: "tester",
            message: None,
            timestamp: &ts,
            files: &[],
        };
        let none = hash_commit(HashFormat::V2, &base);
        let empty = hash_commit(
            HashFormat::V2,
            &CommitFields {
                message: Some(""),
                ..base
            },
        );
        assert_ne!(none, empty);
    }

    #[test]
    fn v2_fields_cannot_shift_between_positions() {
        // In v1 these two commits collide: ("x\ny", "z") vs ("x", "y\nz") in
        // adjacent fields produce one preimage. Length prefixes fix that.
        let manifest = hash_string("manifest");
        let ts = fixed_timestamp();
        let a = hash_commit(
            HashFormat::V2,
            &CommitFields {
                branch_name: "main",
                parent_hash: None,
                merge_parent_hash: None,
                manifest_hash: &manifest,
                author: "x\ny",
                message: Some("z"),
                timestamp: &ts,
                files: &[],
            },
        );
        let b = hash_commit(
            HashFormat::V2,
            &CommitFields {
                branch_name: "main",
                parent_hash: None,
                merge_parent_hash: None,
                manifest_hash: &manifest,
                author: "x",
                message: Some("y\nz"),
                timestamp: &ts,
                files: &[],
            },
        );
        assert_ne!(a, b);
    }

    // ---- format flag plumbing ----

    // ---- v1 commit timestamp serialization (regression guard) ----
    //
    // The v1 commit preimage embeds `timestamp.to_rfc3339()` (see
    // `hash_commit`). Real commits carry subsecond precision and arbitrary UTC
    // offsets — e.g. the oak.space `main` HEAD at the time of writing was
    // `2026-06-27T04:33:39.965405+00:00` (microseconds). The other v1 golden
    // tests all use a *clean* whole-second UTC timestamp, so a change to how
    // fractional seconds or the offset render (e.g. switching to
    // `SecondsFormat::Nanos`, a `Z` suffix, or truncating sub-seconds) would
    // silently change the hash of every real commit while leaving those goldens
    // green — and break verification against every already-stored commit on the
    // server. These cases pin that exact behavior.

    /// `DateTime<Utc>::to_rfc3339()` is the only timestamp serialization the v1
    /// commit hash depends on. Pin its observable contract directly so a chrono
    /// bump or a deliberate format swap is caught in isolation, with a readable
    /// diff, before it cascades into a commit-hash change.
    #[test]
    fn rfc3339_timestamp_rendering_is_stable() {
        use chrono::DateTime;
        let render = |s: &str| {
            DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&Utc)
                .to_rfc3339()
        };
        // Subsecond digits auto-scale to 0/3/6/9 places; UTC always renders as
        // a literal `+00:00` offset (never `Z`); a non-UTC offset normalizes to
        // UTC. Each of these is a property a careless change would flip.
        assert_eq!(
            render("2024-01-02T03:04:05+00:00"),
            "2024-01-02T03:04:05+00:00"
        );
        assert_eq!(
            render("2026-06-27T04:33:39.250+00:00"),
            "2026-06-27T04:33:39.250+00:00"
        );
        assert_eq!(
            render("2026-06-27T04:33:39.965405+00:00"),
            "2026-06-27T04:33:39.965405+00:00"
        );
        assert_eq!(
            render("2026-06-27T04:33:39.123456789+00:00"),
            "2026-06-27T04:33:39.123456789+00:00"
        );
        // Wire timestamps arrive as strings, get parsed, then re-rendered when
        // the commit hash is recomputed for verification (see
        // `protocol::commit_data_to_core`). A `+05:30` offset must collapse to
        // the canonical UTC form, or the recomputed hash would never match.
        assert_eq!(
            render("2026-06-27T04:33:39.965405+05:30"),
            "2026-06-26T23:03:39.965405+00:00"
        );
    }

    /// Golden v1 commit hashes for timestamps with real-world precision and
    /// offsets. Same commit fields, only the timestamp varies, so a drift in
    /// timestamp serialization is the only thing that can move these.
    #[test]
    fn v1_commit_timestamp_precision_golden() {
        use chrono::DateTime;
        let manifest = hash_string("manifest");
        let parent = hash_string("parent");
        let commit_hash = |rfc: &str| {
            let ts = DateTime::parse_from_rfc3339(rfc)
                .unwrap()
                .with_timezone(&Utc);
            hash_commit(
                HashFormat::V1,
                &CommitFields {
                    branch_name: "main",
                    parent_hash: Some(&parent),
                    merge_parent_hash: None,
                    manifest_hash: &manifest,
                    author: "Zach <zach@oak.space>",
                    message: Some("subject\n\nbody"),
                    timestamp: &ts,
                    files: &[],
                },
            )
            .0
        };
        // (input timestamp, pinned commit hash) — these must never change.
        for (rfc, expected) in [
            (
                "2024-01-02T03:04:05+00:00",
                "e4fe01caceba2c8fb905de9477af391e9c9ac50195f48025e922f70608868ae0",
            ),
            (
                "2026-06-27T04:33:39.250+00:00",
                "7a667e00cf11830eb56b2014827566dd92fd379be836f49cbcb24939dc367081",
            ),
            (
                "2026-06-27T04:33:39.965405+00:00",
                "ed0e25d76ab3af99c91b19a0b96e7e3ae8e972c92a3bf8106b392813f29506a9",
            ),
            (
                "2026-06-27T04:33:39.123456789+00:00",
                "bd691a79fccc1a680c59ab7fdeb06d0506af828d62703605d35e72cf55798070",
            ),
            (
                "2026-06-27T04:33:39.965405+05:30",
                "5154b359afbae5b4a92afcb076867baf531ce6d19c3cc3ebe4ee15f159575065",
            ),
        ] {
            assert_eq!(commit_hash(rfc), expected, "commit hash drift for {rfc}");
        }
    }

    /// A merge commit (both `parent_hash` and `merge_parent_hash` set) exercises
    /// two hash-position fields the single-parent goldens leave empty. Pinned so
    /// a change to how those fields serialize is caught.
    #[test]
    fn v1_merge_commit_golden() {
        use chrono::DateTime;
        let manifest = hash_string("manifest");
        let parent = hash_string("parent");
        let merge = hash_string("merge");
        let ts = DateTime::parse_from_rfc3339("2026-06-27T04:33:39.965405+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let h = hash_commit(
            HashFormat::V1,
            &CommitFields {
                branch_name: "main",
                parent_hash: Some(&parent),
                merge_parent_hash: Some(&merge),
                manifest_hash: &manifest,
                author: "tester",
                message: Some("merge"),
                timestamp: &ts,
                files: &[],
            },
        );
        assert_eq!(
            h.as_str(),
            "1375c4cb565921d18c3a06c5b6366ba4aa802c71d228cc4e32f750b1f5d4c99d"
        );
    }

    #[test]
    fn format_flag_defaults_to_v1_and_rejects_unknown() {
        assert_eq!(HashFormat::from_metadata(None).unwrap(), HashFormat::V1);
        assert_eq!(
            HashFormat::from_metadata(Some("v1")).unwrap(),
            HashFormat::V1
        );
        assert_eq!(
            HashFormat::from_metadata(Some("v2")).unwrap(),
            HashFormat::V2
        );
        assert!(HashFormat::from_metadata(Some("v3")).is_err());
        assert_eq!(HashFormat::default(), HashFormat::V1);
    }
}
