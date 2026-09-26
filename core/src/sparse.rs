//! Sparse-checkout cones — Perforce-style partial clones.
//!
//! A **sparse cone** scopes a working tree to a set of directory (or file)
//! path prefixes. Only files covered by the cone are materialized on disk and
//! downloaded from the server; everything else in the repository is left out
//! of the working tree but **carried forward verbatim** in commits, so a
//! sparse clone never deletes the paths it doesn't check out.
//!
//! This mirrors how Oak's path-permissions work server-side: the manifest
//! (tree structure) is always complete — Oak's content-addressed model can't
//! omit entries without breaking hash verification — so a sparse checkout
//! withholds blob *content* for out-of-cone paths, not the file names.
//!
//! The cone is persisted per-repo in [`crate::MetadataKey::SparsePaths`] as a
//! newline-separated list of normalized prefixes. Absent or empty means a
//! full (non-sparse) checkout.
//!
//! Matching semantics are intentionally identical to the server's path-rule
//! matcher: a prefix covers a path when the path *is* the prefix or sits under
//! it on a `/` boundary (`src` covers `src` and `src/a.rs`, but not `src2`).

/// Normalize a cone prefix the same way manifest paths are normalized for
/// matching: drop leading/trailing slashes and collapse `\\` to `/` so a
/// Windows-typed `src\\app` and a stray `/src/` both compare equal to `src`.
pub fn normalize_prefix(raw: &str) -> String {
    raw.trim().replace('\\', "/").trim_matches('/').to_string()
}

/// Does `prefix` cover `path`? True when `path` equals the prefix or sits
/// underneath it on a path boundary. Both arguments must already be
/// normalized (see [`normalize_prefix`]). An empty prefix covers nothing.
fn prefix_covers(prefix: &str, path: &str) -> bool {
    if prefix.is_empty() {
        return false;
    }
    path == prefix
        || (path.len() > prefix.len()
            && path.as_bytes()[prefix.len()] == b'/'
            && path.starts_with(prefix))
}

/// A set of path prefixes a working tree is scoped to. Construct with
/// [`SparseCone::new`] (or [`SparseCone::from_metadata`] when loading the
/// persisted form); query with [`SparseCone::covers`].
///
/// A `SparseCone` is always non-empty: [`SparseCone::new`] returns `None` when
/// every input prefix is blank, because a cone covering nothing would hide the
/// entire working tree — that state is represented as "no cone" (a full
/// checkout), never an empty `SparseCone`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparseCone {
    /// Normalized, de-duplicated, sorted prefixes. Never empty.
    prefixes: Vec<String>,
}

impl SparseCone {
    /// Build a cone from raw prefixes. Prefixes are normalized, blank ones are
    /// dropped, and duplicates collapse. Returns `None` when nothing usable
    /// remains — i.e. the repo should be treated as a full checkout.
    pub fn new<I, S>(prefixes: I) -> Option<SparseCone>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut prefixes: Vec<String> = prefixes
            .into_iter()
            .map(|p| normalize_prefix(p.as_ref()))
            .filter(|p| !p.is_empty())
            .collect();
        prefixes.sort();
        prefixes.dedup();
        if prefixes.is_empty() {
            None
        } else {
            Some(SparseCone { prefixes })
        }
    }

    /// Parse the persisted [`crate::MetadataKey::SparsePaths`] value (one
    /// prefix per line). `None` (key absent) or an all-blank value yields a
    /// full checkout (`None`).
    pub fn from_metadata(raw: Option<&str>) -> Option<SparseCone> {
        SparseCone::new(raw?.lines())
    }

    /// Serialize to the persisted metadata form: one normalized prefix per
    /// line. Round-trips with [`SparseCone::from_metadata`].
    pub fn to_metadata(&self) -> String {
        self.prefixes.join("\n")
    }

    /// The normalized prefixes, sorted. Always at least one.
    pub fn prefixes(&self) -> &[String] {
        &self.prefixes
    }

    /// Is `path` inside the cone? `path` is a repo-relative manifest path
    /// (`/`-separated, no leading slash); it is normalized defensively before
    /// matching.
    pub fn covers(&self, path: &str) -> bool {
        let path = normalize_prefix(path);
        self.prefixes.iter().any(|p| prefix_covers(p, &path))
    }

    /// A new cone with `additions` unioned in. Returns the merged cone (always
    /// `Some` — adding to a non-empty cone can't empty it).
    pub fn with_added<I, S>(&self, additions: I) -> SparseCone
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let merged = self
            .prefixes
            .iter()
            .cloned()
            .chain(additions.into_iter().map(|p| normalize_prefix(p.as_ref())))
            .filter(|p| !p.is_empty());
        // Safe to unwrap: `self.prefixes` is non-empty by construction.
        SparseCone::new(merged).expect("union with a non-empty cone is non-empty")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_and_unifies_separators() {
        assert_eq!(normalize_prefix("/src/"), "src");
        assert_eq!(normalize_prefix("src"), "src");
        assert_eq!(normalize_prefix("  src/app  "), "src/app");
        assert_eq!(normalize_prefix("src\\app"), "src/app");
        assert_eq!(normalize_prefix("///"), "");
        assert_eq!(normalize_prefix(""), "");
    }

    #[test]
    fn new_drops_blanks_and_dedups() {
        let cone = SparseCone::new(["src", "", "  ", "/src/", "libs"]).unwrap();
        assert_eq!(cone.prefixes(), &["libs".to_string(), "src".to_string()]);
    }

    #[test]
    fn new_returns_none_when_everything_blank() {
        assert!(SparseCone::new(["", "  ", "///"]).is_none());
        assert!(SparseCone::new(Vec::<String>::new()).is_none());
    }

    #[test]
    fn covers_respects_path_boundaries() {
        let cone = SparseCone::new(["src", "docs/api"]).unwrap();
        assert!(cone.covers("src"));
        assert!(cone.covers("src/main.rs"));
        assert!(cone.covers("src/deep/nested/x.rs"));
        assert!(cone.covers("docs/api/index.md"));
        // Siblings sharing a textual prefix are NOT covered.
        assert!(!cone.covers("src2/x.rs"));
        assert!(!cone.covers("srcfoo"));
        assert!(!cone.covers("docs/guide.md"));
        // Top-level files outside every prefix are excluded.
        assert!(!cone.covers("README.md"));
        assert!(!cone.covers("Cargo.toml"));
    }

    #[test]
    fn covers_normalizes_the_queried_path() {
        let cone = SparseCone::new(["src"]).unwrap();
        assert!(cone.covers("/src/main.rs"));
        assert!(cone.covers("src\\main.rs"));
    }

    #[test]
    fn metadata_round_trips() {
        let cone = SparseCone::new(["libs/shared", "src"]).unwrap();
        let raw = cone.to_metadata();
        assert_eq!(raw, "libs/shared\nsrc");
        let parsed = SparseCone::from_metadata(Some(&raw)).unwrap();
        assert_eq!(parsed, cone);
    }

    #[test]
    fn from_metadata_absent_or_blank_is_full_checkout() {
        assert!(SparseCone::from_metadata(None).is_none());
        assert!(SparseCone::from_metadata(Some("")).is_none());
        assert!(SparseCone::from_metadata(Some("\n  \n")).is_none());
    }

    #[test]
    fn with_added_unions_and_dedups() {
        let cone = SparseCone::new(["src"]).unwrap();
        let widened = cone.with_added(["libs", "src"]);
        assert_eq!(widened.prefixes(), &["libs".to_string(), "src".to_string()]);
    }
}
