//! Project path-prefix filtering — shared by the CLI and the server.
//!
//! Every repo carries one or more *projects* (see migration
//! `0035_teams_and_projects.sql`). A project is a path-rooted subtree of a
//! repo identified by its `path_prefix`. `'/'` is the migration default and
//! matches the whole repo; deeper prefixes carve out a project from a
//! monorepo.
//!
//! `oak clone` / `oak mount` accept `--team` and `--project` flags. The
//! server resolves those to one or more `path_prefix` values, ships back
//! only the paths under them, and the client uses the same primitives here
//! to know which manifest entries belong in the working tree.

use crate::{Manifest, ManifestEntry};

/// True iff `path` falls under `prefix` at directory boundaries.
///
/// Both inputs are stripped of leading/trailing slashes and compared
/// component-wise. `prefix = "/"` (the migration default) matches every
/// path. The directory-boundary rule avoids the "`pay` matches
/// `payments/foo`" trap a naive `starts_with` would have.
pub fn path_in_prefix(prefix: &str, path: &str) -> bool {
    let prefix_components: Vec<&str> = prefix
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let path_components: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if prefix_components.len() > path_components.len() {
        return false;
    }
    prefix_components
        .iter()
        .zip(path_components.iter())
        .all(|(p, c)| p == c)
}

/// True iff `path` is covered by ANY of `prefixes`. Empty `prefixes` means
/// "no filter is active" — callers should fast-path that case before
/// asking, but to keep callsites safe we still return `true` (no filter
/// → everything is in scope).
pub fn path_in_any_prefix(prefixes: &[String], path: &str) -> bool {
    if prefixes.is_empty() {
        return true;
    }
    prefixes.iter().any(|p| path_in_prefix(p, path))
}

/// True iff a directory `dir` is worth descending into when filtering by
/// `prefixes`. Returns `true` when the directory is inside a prefix OR a
/// prefix is inside the directory (we might find in-scope files deeper).
/// Empty `prefixes` always returns `true`.
pub fn dir_in_any_prefix(prefixes: &[String], dir: &str) -> bool {
    if prefixes.is_empty() {
        return true;
    }
    let dir_components: Vec<&str> = dir
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    for prefix in prefixes {
        let prefix_components: Vec<&str> = prefix
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        // Walk pairs: either the dir is inside the prefix (dir_components
        // is at least as deep) or the prefix is inside the dir (the other
        // way around). Either way, descending is potentially useful.
        let common = dir_components.len().min(prefix_components.len());
        let mut matches_so_far = true;
        for i in 0..common {
            if dir_components[i] != prefix_components[i] {
                matches_so_far = false;
                break;
            }
        }
        if matches_so_far {
            return true;
        }
    }
    false
}

/// Partition manifest entries by the prefix filter. `(inside, outside)`.
/// Empty `prefixes` puts everything in `inside` and `outside` is empty.
pub fn split_manifest_by_prefixes<'a>(
    manifest: &'a Manifest,
    prefixes: &[String],
) -> (Vec<&'a ManifestEntry>, Vec<&'a ManifestEntry>) {
    if prefixes.is_empty() {
        return (manifest.entries.iter().collect(), Vec::new());
    }
    let mut inside = Vec::new();
    let mut outside = Vec::new();
    for entry in &manifest.entries {
        if path_in_any_prefix(prefixes, &entry.path) {
            inside.push(entry);
        } else {
            outside.push(entry);
        }
    }
    (inside, outside)
}

/// Build a new `Manifest` containing only entries inside the prefix filter.
pub fn filter_manifest_by_prefixes(manifest: &Manifest, prefixes: &[String]) -> Manifest {
    if prefixes.is_empty() {
        return manifest.clone();
    }
    let entries: Vec<ManifestEntry> = manifest
        .entries
        .iter()
        .filter(|e| path_in_any_prefix(prefixes, &e.path))
        .cloned()
        .collect();
    Manifest::new(entries)
}

/// Filter a list of manifest entry references by the prefix filter.
pub fn filter_entries_by_prefixes<'a, I>(entries: I, prefixes: &[String]) -> Vec<&'a ManifestEntry>
where
    I: IntoIterator<Item = &'a ManifestEntry>,
{
    if prefixes.is_empty() {
        return entries.into_iter().collect();
    }
    entries
        .into_iter()
        .filter(|e| path_in_any_prefix(prefixes, &e.path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hash_string, FileMode};

    fn entry(path: &str) -> ManifestEntry {
        ManifestEntry {
            path: path.to_string(),
            blob_hash: hash_string(path),
            mode: FileMode::Regular,
        }
    }

    #[test]
    fn root_prefix_matches_everything() {
        assert!(path_in_prefix("/", "src/main.rs"));
        assert!(path_in_prefix("/", "deeply/nested/file.txt"));
        assert!(path_in_prefix("/", "topfile"));
        assert!(path_in_prefix("/", ""));
    }

    #[test]
    fn nested_prefix_matches_descendants() {
        assert!(path_in_prefix("/payments/", "payments/auth.rs"));
        assert!(path_in_prefix("/payments/", "payments/sub/dir/file.rs"));
        assert!(path_in_prefix("payments", "payments/auth.rs"));
        assert!(path_in_prefix("payments/", "payments/auth.rs"));
    }

    #[test]
    fn prefix_respects_directory_boundary() {
        // "pay" must not match "payments/..." — component-wise comparison.
        assert!(!path_in_prefix("/pay/", "payments/auth.rs"));
        assert!(!path_in_prefix("pay", "payments"));
    }

    #[test]
    fn empty_prefixes_lets_everything_through() {
        let prefixes: Vec<String> = Vec::new();
        assert!(path_in_any_prefix(&prefixes, "anything/at/all"));
        assert!(dir_in_any_prefix(&prefixes, "any/dir"));
    }

    #[test]
    fn path_in_any_prefix_unions_matches() {
        let prefixes = vec!["/payments/".to_string(), "/lib/".to_string()];
        assert!(path_in_any_prefix(&prefixes, "payments/auth.rs"));
        assert!(path_in_any_prefix(&prefixes, "lib/util.rs"));
        assert!(!path_in_any_prefix(&prefixes, "billing/foo.rs"));
    }

    #[test]
    fn dir_in_any_prefix_descends_when_useful() {
        let prefixes = vec!["/payments/api/".to_string()];
        // `payments/` is shallower than the prefix but worth descending —
        // the prefix lives below it.
        assert!(dir_in_any_prefix(&prefixes, "payments"));
        assert!(dir_in_any_prefix(&prefixes, "payments/api"));
        assert!(dir_in_any_prefix(&prefixes, "payments/api/v1"));
        // Sibling tree: not worth descending.
        assert!(!dir_in_any_prefix(&prefixes, "billing"));
    }

    #[test]
    fn split_manifest_partitions_by_prefixes() {
        let m = Manifest::new(vec![
            entry("payments/auth.rs"),
            entry("lib/util.rs"),
            entry("billing/charge.rs"),
        ]);
        let prefixes = vec!["/payments/".to_string()];
        let (inside, outside) = split_manifest_by_prefixes(&m, &prefixes);
        let inside_paths: Vec<&str> = inside.iter().map(|e| e.path.as_str()).collect();
        let outside_paths: Vec<&str> = outside.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(inside_paths, vec!["payments/auth.rs"]);
        assert!(outside_paths.contains(&"lib/util.rs"));
        assert!(outside_paths.contains(&"billing/charge.rs"));
    }

    #[test]
    fn split_with_no_prefixes_keeps_everything_inside() {
        let m = Manifest::new(vec![entry("a.rs"), entry("b/c.rs")]);
        let (inside, outside) = split_manifest_by_prefixes(&m, &[]);
        assert_eq!(inside.len(), 2);
        assert!(outside.is_empty());
    }

    #[test]
    fn filter_manifest_drops_out_of_prefix_entries() {
        let m = Manifest::new(vec![entry("payments/a.rs"), entry("billing/b.rs")]);
        let prefixes = vec!["/payments/".to_string()];
        let filtered = filter_manifest_by_prefixes(&m, &prefixes);
        let paths: Vec<&str> = filtered.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["payments/a.rs"]);
    }
}
