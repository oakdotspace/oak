//! Backend capability advertisement.
//!
//! Each `Repository` impl returns a `Capabilities` describing which Oak features
//! it supports. CLI commands check up-front and fail fast with `OakError::Unsupported`
//! rather than letting a missing feature blow up deep in the call stack.
//!
//! The git backend returns a deliberately reduced set: features like branch
//! parents have no clean git mapping (see design notes for `GitRepository`).

/// Feature flags advertised by a `Repository` implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Branch parent relationships (used by `oak merge` to pick a default target).
    pub branch_parents: bool,
    /// Tags: `oak tag ...`.
    pub tags: bool,
    /// Releases: `oak release ...`.
    pub releases: bool,
    /// Content-defined chunking exposed at the storage layer.
    /// Used by `oak push` / `oak pull` against an Oak server.
    pub chunks: bool,
    /// Branch metadata beyond the head ref: description, status, created_at.
    /// SQLite stores these natively; the git backend uses a sidecar.
    pub branch_metadata: bool,
}

impl Capabilities {
    /// Every feature on. Used by SQLite/Postgres backends.
    pub const fn full() -> Self {
        Self {
            branch_parents: true,
            tags: true,
            releases: true,
            chunks: true,
            branch_metadata: true,
        }
    }

    /// Every feature off. Starting point for backends that opt in selectively.
    pub const fn none() -> Self {
        Self {
            branch_parents: false,
            tags: false,
            releases: false,
            chunks: false,
            branch_metadata: false,
        }
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::full()
    }
}
