//! Test-only commit-graph fixtures shared by the merge-base and review
//! fork-point resolver tests: the confirmed squash-revert repro shape and a
//! seeded generator of Oak-shaped DAGs (squash merges whose merge parent is a
//! branch tip, syncs whose merge parent is a main commit, stale syncs and
//! re-merged branches that produce criss-cross histories).

use std::collections::{HashMap, HashSet};

use oak_core::{Commit, Hash, Repository, SqliteRepository};

pub(crate) struct Graph {
    /// Creation (topological) order.
    pub(crate) commits: Vec<Commit>,
}

impl Graph {
    pub(crate) fn new() -> Self {
        Self {
            commits: Vec::new(),
        }
    }

    pub(crate) fn add(
        &mut self,
        branch: &str,
        parent: Option<&Hash>,
        merge_parent: Option<&Hash>,
    ) -> Hash {
        let seq = self.commits.len();
        let commit = Commit::with_timestamp(
            branch.to_string(),
            parent.cloned(),
            merge_parent.cloned(),
            oak_core::Tree::empty_hash(),
            "tester".to_string(),
            Some(format!("c{seq}")),
            Vec::new(),
            chrono::DateTime::from_timestamp(1_700_000_000 + seq as i64, 0).unwrap(),
        )
        .unwrap();
        let hash = commit.hash.clone();
        self.commits.push(commit);
        hash
    }

    /// Store the commits accepted by `keep` in a fresh on-disk repository.
    pub(crate) fn materialize(
        &self,
        keep: impl Fn(&Commit) -> bool,
    ) -> (tempfile::TempDir, SqliteRepository) {
        // In-memory keeps the seeded property affordable in CI; the directory
        // is still returned so callers own a scratch location.
        let dir = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(std::path::Path::new(":memory:")).unwrap();
        for commit in self.commits.iter().filter(|c| keep(c)) {
            repo.store_commit(commit).unwrap();
        }
        (dir, repo)
    }

    pub(crate) fn get(&self, hash: &Hash) -> Option<&Commit> {
        self.commits.iter().find(|c| &c.hash == hash)
    }

    fn ancestors_inclusive(&self, head: &Hash) -> HashSet<Hash> {
        let index: HashMap<&Hash, &Commit> = self.commits.iter().map(|c| (&c.hash, c)).collect();
        let mut out = HashSet::new();
        let mut stack = vec![head.clone()];
        while let Some(hash) = stack.pop() {
            let Some(commit) = index.get(&hash) else {
                continue;
            };
            if !out.insert(hash) {
                continue;
            }
            stack.extend(
                [&commit.parent_hash, &commit.merge_parent_hash]
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        out
    }

    /// Independent oracle: the best common ancestors of `a` and `b` on the
    /// complete graph (common ancestors with no common descendant).
    pub(crate) fn best_common_ancestors(&self, a: &Hash, b: &Hash) -> HashSet<Hash> {
        let a_set = self.ancestors_inclusive(a);
        let b_set = self.ancestors_inclusive(b);
        let common: HashSet<Hash> = a_set.intersection(&b_set).cloned().collect();
        common
            .iter()
            .filter(|c| {
                !common
                    .iter()
                    .any(|d| d != *c && self.ancestors_inclusive(d).contains(*c))
            })
            .cloned()
            .collect()
    }
}

/// `R - C - X - M1..M9 - H` with `M5` reverting X, `H.merge_parent = Z1`
/// (forked from C), and branch `a` = `a1` on X. True merge base of (a1, H)
/// is X; C is the stale base that re-applies X's reverted change.
pub(crate) struct SquashRevert {
    pub(crate) graph: Graph,
    pub(crate) r: Hash,
    pub(crate) c: Hash,
    pub(crate) x: Hash,
    pub(crate) a1: Hash,
    pub(crate) z1: Hash,
    pub(crate) m: Vec<Hash>,
    pub(crate) h: Hash,
}

pub(crate) fn squash_revert() -> SquashRevert {
    let mut graph = Graph::new();
    let r = graph.add("main", None, None);
    let c = graph.add("main", Some(&r), None);
    let x = graph.add("main", Some(&c), None);
    let a1 = graph.add("a", Some(&x), None);
    let z1 = graph.add("z", Some(&c), None);
    let mut m = Vec::new();
    let mut prev = x.clone();
    for _ in 1..=9 {
        prev = graph.add("main", Some(&prev), None);
        m.push(prev.clone());
    }
    let h = graph.add("main", Some(&prev), Some(&z1));
    SquashRevert {
        graph,
        r,
        c,
        x,
        a1,
        z1,
        m,
        h,
    }
}

/// xorshift64* — deterministic, dependency-free.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub(crate) fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

pub(crate) struct RandomDag {
    pub(crate) graph: Graph,
    pub(crate) main: Vec<Hash>,
    /// Every branch tip ever produced (open or merged).
    pub(crate) branch_heads: Vec<Hash>,
}

pub(crate) fn random_oak_dag(rng: &mut Rng) -> RandomDag {
    struct Branch {
        name: String,
        head: Hash,
        open: bool,
    }
    let mut graph = Graph::new();
    let root = graph.add("main", None, None);
    let mut main = vec![root];
    let mut branches: Vec<Branch> = Vec::new();
    let steps = 12 + rng.below(28);
    for _ in 0..steps {
        let open: Vec<usize> = (0..branches.len()).filter(|i| branches[*i].open).collect();
        let roll = rng.below(100);
        if roll < 12 || (open.is_empty() && roll >= 30) {
            let head = main.last().unwrap().clone();
            main.push(graph.add("main", Some(&head), None));
        } else if roll < 30 {
            // Fork from a recent (sometimes older) main commit.
            let window = if rng.chance(20) { 12 } else { 3 };
            let back = rng.below(main.len().min(window));
            let fork = main[main.len() - 1 - back].clone();
            let name = format!("b{}", branches.len());
            let head = graph.add(&name, Some(&fork), None);
            branches.push(Branch {
                name,
                head,
                open: true,
            });
        } else if roll < 58 {
            let i = open[rng.below(open.len())];
            let head = graph.add(&branches[i].name, Some(&branches[i].head), None);
            branches[i].head = head;
        } else if roll < 72 {
            // Sync: usually with main's head, sometimes a stale main commit.
            let i = open[rng.below(open.len())];
            let back = if rng.chance(25) {
                rng.below(main.len().min(8))
            } else {
                0
            };
            let with = main[main.len() - 1 - back].clone();
            let head = graph.add(&branches[i].name, Some(&branches[i].head), Some(&with));
            branches[i].head = head;
        } else {
            // Squash-merge a branch tip into main; sometimes it keeps going
            // (re-merged later, which yields criss-cross shapes).
            let i = open[rng.below(open.len())];
            let head = main.last().unwrap().clone();
            main.push(graph.add("main", Some(&head), Some(&branches[i].head)));
            branches[i].open = rng.chance(35);
        }
    }
    let branch_heads = branches.into_iter().map(|b| b.head).collect();
    RandomDag {
        graph,
        main,
        branch_heads,
    }
}

/// Replay `prepare_remote_review_ancestry`'s hydration policy against a
/// complete in-memory graph: fetch the blocking gaps each round, plus the
/// candidate's first-parent link gap once a candidate stays uncertified for a
/// second round. Returns the final identity and the number of rounds.
pub(crate) fn hydrate_like_review(
    graph: &Graph,
    repo: &SqliteRepository,
    a: &Hash,
    b: &Hash,
    max_rounds: usize,
) -> Result<(super::MergeBaseIdentity, usize), String> {
    use super::{MergeBaseIdentity, MergeBaseUnavailable};
    let mut requested: HashSet<Hash> = HashSet::new();
    let mut uncertified_candidate_rounds = 0usize;
    for round in 1..=max_rounds {
        let identity =
            super::resolve_merge_base_identity_from_heads(repo, "a", Some(a), None, "b", Some(b))
                .map_err(|e| e.to_string())?;
        let MergeBaseIdentity::Unavailable(MergeBaseUnavailable::IncompleteAncestry {
            missing,
            link_gaps,
        }) = identity
        else {
            return Ok((identity, round));
        };
        if missing.is_empty() || missing.iter().any(|h| requested.contains(h)) {
            return Err(format!("no progress at round {round}"));
        }
        let mut wanted = missing;
        if link_gaps.is_empty() {
            uncertified_candidate_rounds = 0;
        } else {
            uncertified_candidate_rounds += 1;
            if uncertified_candidate_rounds >= 2 {
                wanted.extend(link_gaps.into_iter().filter(|g| !requested.contains(g)));
            }
        }
        for hash in wanted {
            let commit = graph
                .get(&hash)
                .ok_or_else(|| format!("reported gap {} is not in the graph", hash.short()))?;
            repo.store_commit(commit).unwrap();
            requested.insert(hash);
        }
    }
    Err(format!("did not converge in {max_rounds} rounds"))
}
