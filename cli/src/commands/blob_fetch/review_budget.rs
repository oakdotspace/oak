//! Review-only limits. Count the expansion without materializing its paths.
use oak_core::{Hash, OakError, Result, Tree, TreeEntryKind};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const MAX_NODES: u64 = 1_000_000;
const MAX_PATH_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DEPTH: u32 = 256;

#[derive(Clone, Copy, Default)]
struct Cost {
    nodes: u64,
    files: u64,
    path_bytes: u64,
    depth: u32,
}

pub(crate) struct ReviewPreparationBudget {
    pub(crate) bytes_left: usize,
    deadline: Instant,
    memo: HashMap<Hash, Cost>,
    charged: HashSet<Hash>,
    nodes: u64,
    path_bytes: u64,
}

fn exhausted(detail: &str) -> OakError {
    OakError::Server(format!(
        "review preparation incomplete: work budget exceeded ({detail}); narrow the review"
    ))
}

impl ReviewPreparationBudget {
    pub(crate) fn new() -> Self {
        Self::with_limits(128 * 1024 * 1024, Duration::from_secs(30))
    }
    pub(super) fn with_limits(bytes: usize, time: Duration) -> Self {
        Self {
            bytes_left: bytes,
            deadline: Instant::now() + time,
            memo: HashMap::new(),
            charged: HashSet::new(),
            nodes: 0,
            path_bytes: 0,
        }
    }
    pub(crate) fn check_deadline(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            return Err(OakError::Server(
                "review preparation incomplete: time budget exceeded".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
    fn check_cost(cost: Cost) -> Result<()> {
        if cost.nodes > MAX_NODES {
            return Err(exhausted("expanded tree entries"));
        }
        if cost.path_bytes > MAX_PATH_BYTES {
            return Err(exhausted("expanded path bytes"));
        }
        if cost.depth > MAX_DEPTH {
            return Err(exhausted("tree depth"));
        }
        Ok(())
    }
    pub(crate) fn inspect<F>(&mut self, root: &Hash, mut fetch: F) -> Result<()>
    where
        F: FnMut(&Hash) -> Result<Option<Tree>>,
    {
        self.check_deadline()?;
        let cost = self.cost(root, 0, &mut HashSet::new(), &mut fetch)?;
        if !self.charged.contains(root) {
            let nodes = self.nodes.saturating_add(cost.nodes);
            let paths = self.path_bytes.saturating_add(cost.path_bytes);
            Self::check_cost(Cost {
                nodes,
                path_bytes: paths,
                ..cost
            })?;
            self.nodes = nodes;
            self.path_bytes = paths;
            self.charged.insert(root.clone());
        }
        Ok(())
    }
    fn cost<F>(
        &mut self,
        hash: &Hash,
        depth: u32,
        visiting: &mut HashSet<Hash>,
        fetch: &mut F,
    ) -> Result<Cost>
    where
        F: FnMut(&Hash) -> Result<Option<Tree>>,
    {
        self.check_deadline()?;
        if depth > MAX_DEPTH {
            return Err(exhausted("tree depth"));
        }
        if hash == &Tree::empty_hash() {
            return Ok(Cost::default());
        }
        if let Some(cost) = self.memo.get(hash) {
            return Ok(*cost);
        }
        if self.memo.len() >= 100_000 {
            return Err(exhausted("unique tree objects"));
        }
        if !visiting.insert(hash.clone()) {
            return Err(exhausted("cyclic tree graph"));
        }
        let tree = fetch(hash)?.ok_or_else(|| OakError::ManifestNotFound(hash.to_string()))?;
        let mut cost = Cost::default();
        for entry in &tree.entries {
            self.check_deadline()?;
            cost.nodes = cost.nodes.saturating_add(1);
            if entry.kind == TreeEntryKind::Tree {
                let child = self.cost(&entry.hash, depth + 1, visiting, fetch)?;
                cost.nodes = cost.nodes.saturating_add(child.nodes);
                cost.files = cost.files.saturating_add(child.files);
                cost.path_bytes = cost
                    .path_bytes
                    .saturating_add(child.path_bytes)
                    .saturating_add(child.files.saturating_mul(entry.name.len() as u64 + 1));
                cost.depth = cost.depth.max(child.depth.saturating_add(1));
            } else {
                cost.files = cost.files.saturating_add(1);
                cost.path_bytes = cost.path_bytes.saturating_add(entry.name.len() as u64);
            }
            Self::check_cost(cost)?;
        }
        visiting.remove(hash);
        self.memo.insert(hash.clone(), cost);
        Ok(cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::{FileMode, TreeEntry};

    #[test]
    fn long_paths_and_deep_trees_have_independent_work_limits() {
        for (depth, width, name_len) in [(16, 2, 1000), (257, 1, 1)] {
            let leaf = Tree::new(vec![TreeEntry {
                name: "file".into(),
                kind: TreeEntryKind::Blob,
                hash: oak_core::hash_bytes(b"x"),
                mode: FileMode::Regular,
            }])
            .unwrap();
            let mut root = leaf.hash.clone();
            let mut trees = HashMap::from([(root.clone(), leaf)]);
            for _ in 0..depth {
                let tree = Tree::new(
                    (0..width)
                        .map(|i| TreeEntry {
                            name: format!("{i}{}", "a".repeat(name_len)),
                            kind: TreeEntryKind::Tree,
                            hash: root.clone(),
                            mode: FileMode::Regular,
                        })
                        .collect(),
                )
                .unwrap();
                root = tree.hash.clone();
                trees.insert(root.clone(), tree);
            }
            let mut budget = ReviewPreparationBudget::new();
            let error = budget
                .inspect(&root, |hash| Ok(trees.get(hash).cloned()))
                .unwrap_err();
            assert!(
                error.to_string().contains(if depth > 256 {
                    "tree depth"
                } else {
                    "path bytes"
                }),
                "{error}"
            );
        }
    }
}
