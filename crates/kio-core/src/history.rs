//! Bounded, CAS-backed snapshot-history traversal.
//!
//! This module deliberately reads commit and tree objects from the CAS. Mutable
//! manifests, SQLite projections, and other acceleration data are never accepted
//! as history truth.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::json;

use crate::ExitCode;
use crate::cas::{ObjectKind, ObjectStore, StoredObject};
use crate::dag::{CommitObject, MAX_TREE_ENTRIES, NormalizeRef, TreeEntry, TreeObject};
use crate::error::{KioError, Result};

pub const DEFAULT_MAX_HISTORY_COMMITS: u64 = 100_000;
pub const DEFAULT_MAX_HISTORY_TREE_ENTRIES: u64 = 10_000_000;
pub const DEFAULT_MAX_HISTORY_VERIFIED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Aggregate limits for one linear history walk. A reader applies a fresh set
/// of counters to every invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryLimits {
    pub max_commits: u64,
    pub max_tree_entries: u64,
    pub max_verified_bytes: u64,
}

impl HistoryLimits {
    #[must_use]
    pub const fn new(max_commits: u64, max_tree_entries: u64, max_verified_bytes: u64) -> Self {
        Self {
            max_commits,
            max_tree_entries,
            max_verified_bytes,
        }
    }
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_HISTORY_COMMITS,
            DEFAULT_MAX_HISTORY_TREE_ENTRIES,
            DEFAULT_MAX_HISTORY_VERIFIED_BYTES,
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryStats {
    pub commits: u64,
    pub tree_entries: u64,
    pub verified_bytes: u64,
}

/// The immutable tree identity used to join a persisted tree entry to indexed
/// chunks. `normalize = None` is an exact value and is never supplemented from a
/// later commit or mutable cache.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TreeBinding {
    pub path: String,
    pub raw_hash: String,
    pub normalize: Option<NormalizeRef>,
}

impl From<&TreeEntry> for TreeBinding {
    fn from(entry: &TreeEntry) -> Self {
        Self {
            path: entry.path.clone(),
            raw_hash: entry.raw_hash.clone(),
            normalize: entry.normalize.clone(),
        }
    }
}

/// One exact binding appearance, including the commit and tree that attest it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryBinding {
    pub commit_hash: String,
    pub tree_hash: String,
    pub binding: TreeBinding,
}

#[derive(Debug, Clone)]
pub struct HistoryNode {
    pub commit_hash: String,
    pub commit: CommitObject,
    pub tree: TreeObject,
    pub commit_bytes: u64,
    pub tree_bytes: u64,
}

impl HistoryNode {
    fn binding(&self, entry: &TreeEntry) -> HistoryBinding {
        HistoryBinding {
            commit_hash: self.commit_hash.clone(),
            tree_hash: self.commit.tree.clone(),
            binding: TreeBinding::from(entry),
        }
    }

    fn entry(&self, path: &str) -> Option<&TreeEntry> {
        self.tree
            .entries
            .binary_search_by(|entry| entry.path.as_bytes().cmp(path.as_bytes()))
            .ok()
            .map(|index| &self.tree.entries[index])
    }

    fn contains_binding(&self, binding: &TreeBinding) -> bool {
        self.entry(&binding.path).map(TreeBinding::from).as_ref() == Some(binding)
    }
}

/// A complete newest-first linear history reachable from one snapshot commit.
#[derive(Debug, Clone)]
pub struct LinearHistory {
    start_commit: String,
    nodes: BTreeMap<String, HistoryNode>,
    visit_order: Vec<String>,
    stats: HistoryStats,
}

impl LinearHistory {
    #[must_use]
    pub fn start_commit(&self) -> &str {
        &self.start_commit
    }

    #[must_use]
    pub const fn stats(&self) -> HistoryStats {
        self.stats
    }

    #[must_use]
    pub fn node(&self, commit_hash: &str) -> Option<&HistoryNode> {
        self.nodes.get(commit_hash)
    }

    /// Deterministic newest-first traversal order. Every reachable commit
    /// appears exactly once.
    pub fn nodes_in_visit_order(&self) -> impl Iterator<Item = &HistoryNode> {
        self.visit_order
            .iter()
            .filter_map(|hash| self.nodes.get(hash))
    }

    /// Every binding appearance, sorted by exact identity and then commit hash.
    #[must_use]
    pub fn bindings(&self) -> Vec<HistoryBinding> {
        let mut bindings = self
            .nodes
            .values()
            .flat_map(|node| {
                node.tree
                    .entries
                    .iter()
                    .map(move |entry| node.binding(entry))
            })
            .collect::<Vec<_>>();
        sort_bindings(&mut bindings);
        bindings
    }

    /// Test reachability within the complete graph. A commit is its own ancestor.
    #[must_use]
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        if !self.nodes.contains_key(ancestor) || !self.nodes.contains_key(descendant) {
            return false;
        }
        let mut pending = vec![descendant];
        let mut visited = BTreeSet::new();
        while let Some(hash) = pending.pop() {
            if hash == ancestor {
                return true;
            }
            if !visited.insert(hash) {
                continue;
            }
            if let Some(node) = self.nodes.get(hash)
                && let Some(parent) = node.commit.parent.as_deref()
            {
                pending.push(parent);
            }
        }
        false
    }

    /// The earliest linear commit where `binding` appears.
    #[must_use]
    pub fn introduction_candidates(&self, binding: &TreeBinding) -> Vec<HistoryBinding> {
        let mut candidates = self
            .nodes
            .values()
            .filter(|node| {
                node.contains_binding(binding)
                    && node.commit.parent.as_deref().is_none_or(|parent| {
                        self.nodes
                            .get(parent)
                            .is_none_or(|parent_node| !parent_node.contains_binding(binding))
                    })
            })
            .filter_map(|node| node.entry(&binding.path).map(|entry| node.binding(entry)))
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| a.commit_hash.as_bytes().cmp(b.commit_hash.as_bytes()));
        candidates
    }

    /// The unique earliest introduction in a linear history.
    #[must_use]
    pub fn ancestor_most_introductions(&self, binding: &TreeBinding) -> Vec<HistoryBinding> {
        // `visit_order` is newest-first. The oldest matching introduction is
        // the first one encountered from its reverse, irrespective of how its
        // content-addressed commit hash happens to sort.
        self.visit_order
            .iter()
            .rev()
            .filter_map(|hash| self.nodes.get(hash))
            .find_map(|node| {
                (node.contains_binding(binding)
                    && node.commit.parent.as_deref().is_none_or(|parent| {
                        self.nodes
                            .get(parent)
                            .is_none_or(|parent_node| !parent_node.contains_binding(binding))
                    }))
                .then(|| node.entry(&binding.path).map(|entry| node.binding(entry)))
                .flatten()
            })
            .into_iter()
            .collect()
    }

    /// Canonical introduction in the unique linear ancestry.
    #[must_use]
    pub fn canonical_introduction(&self, binding: &TreeBinding) -> Option<HistoryBinding> {
        self.ancestor_most_introductions(binding).into_iter().next()
    }

    /// Distinct snapshot paths carrying `raw_hash`, in UTF-8 byte order.
    #[must_use]
    pub fn snapshot_paths_for_raw(&self, raw_hash: &str) -> Vec<String> {
        self.nodes
            .get(&self.start_commit)
            .into_iter()
            .flat_map(|node| node.tree.entries.iter())
            .filter(|entry| entry.raw_hash == raw_hash)
            .map(|entry| entry.path.clone())
            .collect()
    }
    /// The newest exact persisted binding for `path`.
    #[must_use]
    pub fn newest_binding_for_path(&self, path: &str) -> Option<HistoryBinding> {
        self.nodes_in_visit_order()
            .find_map(|node| node.entry(path).map(|entry| node.binding(entry)))
    }

    /// For every path absent from the snapshot tree, return its newest exact
    /// linear-ancestry binding. Results are sorted by path bytes. A binding whose
    /// normalize reference is absent remains present here with `normalize=None`;
    /// downstream chunk projection must treat it as ineligible.
    #[must_use]
    pub fn final_deleted_bindings(&self) -> Vec<HistoryBinding> {
        let live_paths = self
            .nodes
            .get(&self.start_commit)
            .into_iter()
            .flat_map(|node| node.tree.entries.iter().map(|entry| entry.path.as_str()))
            .collect::<BTreeSet<_>>();
        let mut newest_by_path = BTreeMap::new();
        for node in self.nodes_in_visit_order() {
            for entry in &node.tree.entries {
                if !live_paths.contains(entry.path.as_str()) {
                    newest_by_path
                        .entry(entry.path.clone())
                        .or_insert_with(|| node.binding(entry));
                }
            }
        }
        newest_by_path.into_values().collect()
    }
}

#[derive(Debug, Clone)]
pub struct HistoryReader {
    store: ObjectStore,
    // Keep the capability root separately from `ObjectStore`: tolerant history
    // walks must authenticate a missing tree against the strict shallow-receipt
    // namespace, rather than treating every missing CAS tree as intentional.
    kio_dir: PathBuf,
    limits: HistoryLimits,
}

impl HistoryReader {
    #[must_use]
    pub fn new(kio_dir: impl Into<PathBuf>) -> Self {
        Self::with_limits(kio_dir, HistoryLimits::default())
    }

    #[must_use]
    pub fn with_limits(kio_dir: impl Into<PathBuf>, limits: HistoryLimits) -> Self {
        let supplied = kio_dir.into();
        // HistoryReader has historically accepted ordinary ambient paths
        // (including macOS tempfile paths spelled through `/var` while the
        // canonical mount name is `/private/var`).  Canonicalize once before
        // handing the path to the capability-bound receipt reader; all later
        // reads stay on this fixed store identity.  Keep the original path only
        // when canonicalization fails so strict ObjectStore reads preserve the
        // established typed error at use time.
        let kio_dir = supplied.canonicalize().unwrap_or(supplied);
        Self {
            store: ObjectStore::new(&kio_dir),
            kio_dir,
            limits,
        }
    }

    /// Read one selected snapshot without traversing its ancestry. This is the
    /// direct `--at`/explicit-commit primitive; a shallow ancestor that is not
    /// required by the selected snapshot cannot make this read fail.
    pub fn snapshot(&self, commit_hash: &str) -> Result<HistoryNode> {
        self.read_node(commit_hash, HistoryStats::default())
            .map(|(node, _)| node)
    }

    /// Read the strict newest-first ancestry of one commit.
    pub fn walk(&self, start_commit: &str) -> Result<LinearHistory> {
        let walk = self.walk_many([start_commit])?;
        ensure_acyclic(&walk.nodes)?;
        let edges = parent_edges_from_nodes(&walk.nodes);
        let visit_order = linear_order(&edges, &walk.nodes, start_commit)?;
        Ok(LinearHistory {
            start_commit: start_commit.to_owned(),
            nodes: walk.nodes,
            visit_order,
            stats: walk.stats,
        })
    }

    /// Read a strict union of roots that belong to one linear history.
    ///
    /// Shared ancestry is decoded and verified once. Roots that are not on one
    /// chain are a schema violation rather than implicit branches.
    pub fn walk_for_roots(&self, roots: &BTreeSet<String>) -> Result<LinearHistory> {
        let walk = self.walk_many(roots.iter().map(String::as_str))?;
        ensure_acyclic(&walk.nodes)?;
        let edges = parent_edges_from_nodes(&walk.nodes);
        let start_commit = linear_tip(&edges, roots)?;
        let visit_order = linear_order(&edges, &walk.nodes, &start_commit)?;
        Ok(LinearHistory {
            start_commit,
            nodes: walk.nodes,
            visit_order,
            stats: walk.stats,
        })
    }

    /// PC45/PC46 (05 §1.6 / §2.2): a linear walk tolerant of a shallow
    /// (tree-discarded) *ancestor* — it is
    /// skipped (recorded in the returned `shallow_skipped` list, sorted/deduped)
    /// and the walk continues through that commit's predecessor (still readable
    /// from its commit object, which shallow GC never discards, §2.2). The **start**
    /// commit itself is never tolerated this way: if its own tree is gone the
    /// call hard-fails exactly like [`Self::walk`] (PC47 — a cursor's or `--at`'s
    /// snapshot commit needs its whole tree, so there is no partial degradation
    /// to fall back to). A missing *commit* object (not just its tree) is never
    /// shallow-tolerated either — shallow GC only ever discards trees (§2.2), so
    /// a missing commit is corruption, and this call fails exactly like
    /// `walk` in that case too.
    pub fn walk_allow_shallowed(&self, start_commit: &str) -> Result<(LinearHistory, Vec<String>)> {
        self.walk_for_roots_allow_shallowed(&BTreeSet::from([start_commit.to_owned()]))
    }

    /// The receipt-gated tolerant counterpart of [`Self::walk_for_roots`].
    /// Every supplied root remains strict; only a non-root ancestor with a
    /// markerless, exact shallow receipt may be skipped while its predecessor keeps
    /// being traversed. This is intended for derived-state rebuilds, not for
    /// explicit snapshot selection.
    pub fn walk_for_roots_allow_shallowed(
        &self,
        roots: &BTreeSet<String>,
    ) -> Result<(LinearHistory, Vec<String>)> {
        let allowed_shallow = self.markerless_shallow_receipts()?;
        let walk = self.walk_many_tolerant(roots.iter().map(String::as_str), &allowed_shallow)?;
        ensure_acyclic(&walk.nodes)?;
        ensure_acyclic_edges(&walk.parents)?;
        let start_commit = linear_tip(&walk.parents, roots)?;
        let visit_order = linear_order(&walk.parents, &walk.nodes, &start_commit)?;
        Ok((
            LinearHistory {
                start_commit,
                nodes: walk.nodes,
                visit_order,
                stats: walk.stats,
            },
            walk.shallow_skipped,
        ))
    }

    fn walk_many<'a>(&self, starts: impl IntoIterator<Item = &'a str>) -> Result<WalkState> {
        let mut state = WalkState::default();
        let scheduled = starts
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let mut pending = scheduled.iter().rev().cloned().collect::<Vec<_>>();
        let mut scheduled = scheduled;

        while let Some(commit_hash) = pending.pop() {
            let (node, next_stats) = self.read_node(&commit_hash, state.stats)?;

            if let Some(parent) = node.commit.parent.as_ref()
                && scheduled.insert(parent.clone())
            {
                pending.push(parent.clone());
            }

            state.stats = next_stats;
            state.nodes.insert(commit_hash, node);
        }

        Ok(state)
    }

    /// Same traversal as [`Self::walk`], except a shallow (tree-missing) commit
    /// other than `start_commit` is skipped instead of failing the whole walk
    /// (PC45). The commit object of a skipped node is still required (it is
    /// where the parent list to keep walking comes from) — only the tree read is
    /// tolerated. Skipped hashes are still counted against `HistoryStats` for
    /// their commit bytes (their tree contributes zero entries/bytes, same as a
    /// legitimately empty tree would).
    fn walk_many_tolerant<'a>(
        &self,
        starts: impl IntoIterator<Item = &'a str>,
        _allowed_shallow: &BTreeMap<String, String>,
    ) -> Result<TolerantWalkState> {
        let mut state = TolerantWalkState::default();
        let strict_starts = starts
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let mut pending = strict_starts.iter().rev().cloned().collect::<Vec<_>>();
        let mut scheduled = strict_starts.clone();

        while let Some(commit_hash) = pending.pop() {
            let is_start = strict_starts.contains(&commit_hash);
            let outcome =
                self.read_node_tolerant(&commit_hash, state.stats, is_start, _allowed_shallow)?;
            let (parent, next_stats) = match outcome {
                TolerantNodeOutcome::Full(node, next_stats) => {
                    let parent = node.commit.parent.clone();
                    state.nodes.insert(commit_hash.clone(), *node);
                    (parent, next_stats)
                }
                TolerantNodeOutcome::ShallowSkipped { parent, stats } => {
                    state.shallow_skipped.push(commit_hash.clone());
                    // A shallow ancestor still occupies a position in the
                    // newest-first / visit order for `--include-deleted`'s
                    // `nodes_newest_first()` — but that method already
                    // filter_maps through `self.nodes`, so a hash with no node
                    // is silently and correctly skipped there. Do not push it
                    // into `order` — nothing downstream needs it and every
                    // consumer indexes through `self.nodes` first.
                    (parent, stats)
                }
            };
            state.parents.insert(commit_hash.clone(), parent.clone());
            if let Some(parent) = parent
                && scheduled.insert(parent.clone())
            {
                pending.push(parent);
            }
            state.stats = next_stats;
        }

        state.shallow_skipped.sort();
        state.shallow_skipped.dedup();
        Ok(state)
    }

    fn read_node(
        &self,
        commit_hash: &str,
        stats: HistoryStats,
    ) -> Result<(HistoryNode, HistoryStats)> {
        let next_commit_count = checked_total(stats.commits, 1);
        if next_commit_count > self.limits.max_commits {
            return Err(history_limit_error(
                "commits",
                stats,
                self.limits,
                next_commit_count,
            ));
        }

        let commit_object = self.read_required(ObjectKind::Commit, commit_hash, commit_hash)?;
        let commit_bytes = commit_object.bytes.len() as u64;
        let after_commit_bytes = checked_total(stats.verified_bytes, commit_bytes);
        if after_commit_bytes > self.limits.max_verified_bytes {
            return Err(history_limit_error(
                "verified_bytes",
                stats,
                self.limits,
                after_commit_bytes,
            ));
        }
        let commit = decode_commit(commit_object)?;

        let tree_object = self.read_required(ObjectKind::Tree, &commit.tree, commit_hash)?;
        let tree_bytes = tree_object.bytes.len() as u64;
        let next_verified_bytes = checked_total(after_commit_bytes, tree_bytes);
        if next_verified_bytes > self.limits.max_verified_bytes {
            return Err(history_limit_error(
                "verified_bytes",
                stats,
                self.limits,
                next_verified_bytes,
            ));
        }
        let tree = decode_tree(tree_object)?;
        let next_tree_entries = checked_total(stats.tree_entries, tree.entries.len() as u64);
        if next_tree_entries > self.limits.max_tree_entries {
            return Err(history_limit_error(
                "tree_entries",
                stats,
                self.limits,
                next_tree_entries,
            ));
        }

        Ok((
            HistoryNode {
                commit_hash: commit_hash.to_owned(),
                commit,
                tree,
                commit_bytes,
                tree_bytes,
            },
            HistoryStats {
                commits: next_commit_count,
                tree_entries: next_tree_entries,
                verified_bytes: next_verified_bytes,
            },
        ))
    }

    fn read_required(
        &self,
        kind: ObjectKind,
        object_hash: &str,
        commit_hash: &str,
    ) -> Result<StoredObject> {
        match self.store.read_object(kind, object_hash) {
            Ok(object) => Ok(object),
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                if kind == ObjectKind::Tree {
                    crate::gc::validate_final_shallow_tree(
                        &self.kio_dir,
                        commit_hash,
                        object_hash,
                    )?;
                }
                Err(history_shallow_error(commit_hash, kind, object_hash))
            }
            Err(error) => Err(error),
        }
    }

    /// [`Self::read_node`]'s shallow-tolerant counterpart (PC45): the commit
    /// object is always required (shallow GC never discards commits, only trees
    /// — §2.2 — so a missing commit is corruption regardless of `is_start`), but
    /// a missing *tree* is tolerated for any node other than `is_start` (the
    /// walk's own starting commit, whose full tree PC47 always requires).
    fn read_node_tolerant(
        &self,
        commit_hash: &str,
        stats: HistoryStats,
        is_start: bool,
        _allowed_shallow: &BTreeMap<String, String>,
    ) -> Result<TolerantNodeOutcome> {
        let next_commit_count = checked_total(stats.commits, 1);
        if next_commit_count > self.limits.max_commits {
            return Err(history_limit_error(
                "commits",
                stats,
                self.limits,
                next_commit_count,
            ));
        }

        let commit_object = self.read_required(ObjectKind::Commit, commit_hash, commit_hash)?;
        let commit_bytes = commit_object.bytes.len() as u64;
        let after_commit_bytes = checked_total(stats.verified_bytes, commit_bytes);
        if after_commit_bytes > self.limits.max_verified_bytes {
            return Err(history_limit_error(
                "verified_bytes",
                stats,
                self.limits,
                after_commit_bytes,
            ));
        }
        let commit = decode_commit(commit_object)?;

        let tree_object = match self.store.read_object(ObjectKind::Tree, &commit.tree) {
            Ok(object) => object,
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                if _allowed_shallow.get(commit_hash) != Some(&commit.tree) {
                    return Err(KioError::new(
                        "KIO-E-STORE-CORRUPT-001",
                        "missing commit tree has no canonical shallow receipt",
                        json!({"commit_hash": commit_hash, "tree_hash": commit.tree}),
                        ExitCode::PermanentFailure,
                    ));
                }
                if is_start {
                    return Err(history_shallow_error(
                        commit_hash,
                        ObjectKind::Tree,
                        &commit.tree,
                    ));
                }
                return Ok(TolerantNodeOutcome::ShallowSkipped {
                    parent: commit.parent,
                    stats: HistoryStats {
                        commits: next_commit_count,
                        tree_entries: stats.tree_entries,
                        verified_bytes: after_commit_bytes,
                    },
                });
            }
            Err(error) => return Err(error),
        };
        let tree_bytes = tree_object.bytes.len() as u64;
        let next_verified_bytes = checked_total(after_commit_bytes, tree_bytes);
        if next_verified_bytes > self.limits.max_verified_bytes {
            return Err(history_limit_error(
                "verified_bytes",
                stats,
                self.limits,
                next_verified_bytes,
            ));
        }
        let tree = decode_tree(tree_object)?;
        let next_tree_entries = checked_total(stats.tree_entries, tree.entries.len() as u64);
        if next_tree_entries > self.limits.max_tree_entries {
            return Err(history_limit_error(
                "tree_entries",
                stats,
                self.limits,
                next_tree_entries,
            ));
        }

        Ok(TolerantNodeOutcome::Full(
            Box::new(HistoryNode {
                commit_hash: commit_hash.to_owned(),
                commit,
                tree,
                commit_bytes,
                tree_bytes,
            }),
            HistoryStats {
                commits: next_commit_count,
                tree_entries: next_tree_entries,
                verified_bytes: next_verified_bytes,
            },
        ))
    }

    /// Build the only allow-list a tolerant walk may use.  Active sweeps have
    /// not reached a stable markerless shallow state, so no absent tree is
    /// tolerated until recovery completes. `read_shallow_receipts` is itself a
    /// strict no-follow, canonical receipt inventory.
    fn markerless_shallow_receipts(&self) -> Result<BTreeMap<String, String>> {
        crate::gc::validated_final_shallow_receipts(&self.kio_dir)
    }
}

enum TolerantNodeOutcome {
    // Boxed: `HistoryNode` is much larger than `ShallowSkipped`'s fields, and
    // this enum is returned by value on every walked commit.
    Full(Box<HistoryNode>, HistoryStats),
    ShallowSkipped {
        parent: Option<String>,
        stats: HistoryStats,
    },
}

#[derive(Debug, Default)]
struct TolerantWalkState {
    nodes: BTreeMap<String, HistoryNode>,
    parents: BTreeMap<String, Option<String>>,
    stats: HistoryStats,
    /// Commit hashes skipped because their tree was gone (PC45/PC46), sorted +
    /// deduped once the walk completes.
    shallow_skipped: Vec<String>,
}

#[derive(Debug, Default)]
struct WalkState {
    nodes: BTreeMap<String, HistoryNode>,
    stats: HistoryStats,
}

fn decode_commit(object: StoredObject) -> Result<CommitObject> {
    let commit: CommitObject = serde_json::from_slice(&object.bytes)
        .map_err(|error| KioError::schema(error.to_string()))?;
    commit.validate()?;
    Ok(commit)
}

fn decode_tree(object: StoredObject) -> Result<TreeObject> {
    let tree: TreeObject = serde_json::from_slice(&object.bytes)
        .map_err(|error| KioError::schema(error.to_string()))?;
    if tree.entries.len() > MAX_TREE_ENTRIES {
        return Err(KioError::schema(format!(
            "tree entries exceed the limit of {MAX_TREE_ENTRIES}"
        )));
    }
    tree.validate()?;
    Ok(tree)
}

fn parent_edges_from_nodes(
    nodes: &BTreeMap<String, HistoryNode>,
) -> BTreeMap<String, Option<String>> {
    nodes
        .iter()
        .map(|(hash, node)| (hash.clone(), node.commit.parent.clone()))
        .collect()
}

/// Return the unique newest supplied root. Every other supplied root must be
/// reachable through immutable predecessor edges. This remains sound across a
/// receipt-authorized shallow commit because its commit object supplies its
/// predecessor edge even though its tree is absent.
fn linear_tip(
    edges: &BTreeMap<String, Option<String>>,
    roots: &BTreeSet<String>,
) -> Result<String> {
    if roots.is_empty() {
        return Err(KioError::schema("history root set is empty"));
    }
    // Visit each predecessor at most once across all roots. Pairwise ancestry
    // comparisons multiply full history walks for every tag pair and can make
    // an otherwise bounded 100,000-commit history impractical to read.
    let mut non_tips = BTreeSet::new();
    let mut walked = BTreeSet::new();
    for root in roots {
        let mut cursor = edges
            .get(root)
            .ok_or_else(|| KioError::schema("cannot prove linear ancestry across missing history"))?
            .as_deref();
        while let Some(hash) = cursor {
            if roots.contains(hash) {
                non_tips.insert(hash);
            }
            if !walked.insert(hash) {
                break;
            }
            cursor = edges
                .get(hash)
                .ok_or_else(|| {
                    KioError::schema("cannot prove linear ancestry across missing history")
                })?
                .as_deref();
        }
    }
    let newest = roots
        .iter()
        .filter(|root| !non_tips.contains(root.as_str()))
        .collect::<Vec<_>>();
    if newest.len() != 1 {
        return Err(KioError::schema(
            "history roots are not on one linear ancestry chain",
        ));
    }
    let tip = (*newest[0]).clone();
    let mut cursor = Some(tip.as_str());
    let mut seen = BTreeSet::new();
    while let Some(hash) = cursor {
        if !seen.insert(hash) {
            return Err(KioError::schema("commit history contains a parent cycle"));
        }
        cursor = match edges.get(hash) {
            Some(parent) => parent.as_deref(),
            None => {
                return Err(KioError::schema(
                    "cannot prove linear ancestry across shallow history",
                ));
            }
        };
    }
    if roots.iter().all(|root| seen.contains(root.as_str())) {
        Ok(tip)
    } else {
        Err(KioError::schema(
            "history roots are not on one linear ancestry chain",
        ))
    }
}

/// Rebuild a canonical newest-first node order from the selected tip. Shallow
/// commits remain in the predecessor proof but have no `HistoryNode`, so they
/// do not appear in the returned iterator.
fn linear_order(
    edges: &BTreeMap<String, Option<String>>,
    nodes: &BTreeMap<String, HistoryNode>,
    tip: &str,
) -> Result<Vec<String>> {
    let mut order = Vec::new();
    let mut cursor = Some(tip);
    let mut seen = BTreeSet::new();
    while let Some(hash) = cursor {
        if !seen.insert(hash) {
            return Err(KioError::schema("commit history contains a parent cycle"));
        }
        if nodes.contains_key(hash) {
            order.push(hash.to_owned());
        }
        cursor = edges
            .get(hash)
            .ok_or_else(|| KioError::schema("cannot prove linear history order"))?
            .as_deref();
    }
    Ok(order)
}

fn ensure_acyclic(nodes: &BTreeMap<String, HistoryNode>) -> Result<()> {
    ensure_acyclic_edges(&parent_edges_from_nodes(nodes))
}

fn ensure_acyclic_edges(edges: &BTreeMap<String, Option<String>>) -> Result<()> {
    let mut completed = BTreeSet::new();
    for start in edges.keys() {
        let mut cursor = Some(start.as_str());
        let mut active = BTreeSet::new();
        while let Some(hash) = cursor {
            if completed.contains(hash) {
                break;
            }
            if !active.insert(hash) {
                return Err(KioError::schema("commit history contains a parent cycle"));
            }
            cursor = edges.get(hash).and_then(|parent| parent.as_deref());
        }
        completed.extend(active);
    }
    Ok(())
}

fn sort_bindings(bindings: &mut [HistoryBinding]) {
    bindings.sort_by(|a, b| {
        a.binding
            .cmp(&b.binding)
            .then_with(|| a.commit_hash.as_bytes().cmp(b.commit_hash.as_bytes()))
            .then_with(|| a.tree_hash.as_bytes().cmp(b.tree_hash.as_bytes()))
    });
}

fn checked_total(current: u64, increment: u64) -> u64 {
    current.saturating_add(increment)
}

/// R23-15 (06 §8 L403 "単独操作 exit 4" / 05 §1.6 L307-310): a bounded
/// history-walk aggregate cap overrun is a PERMANENT failure for a standalone
/// walk (restore/purge's ancestor checks, or any other direct
/// `HistoryReader` caller) -- re-running the identical command cannot change
/// the outcome, so `ExitCode::PermanentFailure` (4), not the generic
/// `ExitCode::Failure` (1) this constructor returned before the fix. Search's
/// own multi-scope aggregation (`crates/kio-cli/src/main.rs`) computes ITS
/// exit independently across all searched scopes and does not read this
/// field for that computation, so widening it here cannot regress the
/// existing partial-failure behavior there.
fn history_limit_error(
    exceeded: &str,
    stats: HistoryStats,
    limits: HistoryLimits,
    attempted: u64,
) -> KioError {
    KioError::new(
        "KIO-E-COMMIT-HISTORY-LIMIT-001",
        "history walk aggregate limit exceeded",
        json!({
            "exceeded": exceeded,
            "attempted": attempted,
            "commits": stats.commits,
            "tree_entries": stats.tree_entries,
            "verified_bytes": stats.verified_bytes,
            "max_commits": limits.max_commits,
            "max_tree_entries": limits.max_tree_entries,
            "max_verified_bytes": limits.max_verified_bytes,
        }),
        ExitCode::PermanentFailure,
    )
}

fn history_shallow_error(
    commit_hash: &str,
    missing_kind: ObjectKind,
    missing_object_hash: &str,
) -> KioError {
    KioError::new(
        "KIO-E-COMMIT-SHALLOW-001",
        format!(
            "history walk requires a {} object that is missing or shallow",
            missing_kind.object_type()
        ),
        json!({
            "commit_hash": commit_hash,
            "missing_object_kind": missing_kind.object_type(),
            "missing_object_hash": missing_object_hash,
        }),
        ExitCode::Failure,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::PathBuf;

    use serde_json::json;

    use super::{HistoryLimits, HistoryReader, TreeBinding};
    use crate::cas::{ObjectKind, ObjectStore, hash_bytes};
    use crate::dag::{CommitObject, CommitStats, CommitType, NormalizeRef, TreeEntry, build_tree};
    use crate::gc::ShallowReceipt;

    struct Fixture {
        _temp: tempfile::TempDir,
        kio_dir: PathBuf,
        store: ObjectStore,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let kio_dir = temp.path().join(".kio");
            fs::create_dir(&kio_dir).unwrap();
            fs::create_dir_all(kio_dir.join("refs/heads")).unwrap();
            fs::create_dir_all(kio_dir.join("refs/tags-v1")).unwrap();
            fs::create_dir_all(kio_dir.join("objects/trees")).unwrap();
            fs::write(kio_dir.join("HEAD"), b"unborn\n").unwrap();
            let store = ObjectStore::new(&kio_dir);
            Self {
                _temp: temp,
                kio_dir,
                store,
            }
        }

        fn tree(&self, entries: Vec<TreeEntry>) -> String {
            let tree = build_tree(entries).unwrap();
            self.store
                .write_json(ObjectKind::Tree, &serde_json::to_value(tree).unwrap())
                .unwrap()
                .0
        }

        fn commit(&self, label: &str, tree: &str, parents: Vec<String>) -> String {
            assert!(
                parents.len() <= 1,
                "linear history test fixture rejects merge parents"
            );
            let commit = CommitObject::new(
                tree.to_owned(),
                parents.into_iter().next(),
                "2026-07-13T00:00:00Z".to_owned(),
                label.to_owned(),
                hash_bytes(b"tool-lock"),
                CommitStats {
                    files_added: 0,
                    files_modified: 0,
                    files_deleted: 0,
                },
                CommitType::Auto,
            )
            .unwrap();
            self.store
                .write_json(ObjectKind::Commit, &serde_json::to_value(commit).unwrap())
                .unwrap()
                .0
        }

        fn shallow_receipt(&self, commit: String, tree: String) {
            let leaf = commit.strip_prefix("sha256:").unwrap().to_owned();
            let dir = self.kio_dir.join("gc/shallowed");
            fs::create_dir_all(&dir).unwrap();
            let receipt = ShallowReceipt::new(commit, tree, "2026-07-13T00:00:00Z".into()).unwrap();
            fs::write(dir.join(leaf), receipt.canonical_bytes().unwrap()).unwrap();
        }
    }

    fn entry(path: &str, body: &[u8], normalize: Option<NormalizeRef>) -> TreeEntry {
        let mut entry = TreeEntry::raw_file(path, hash_bytes(body)).unwrap();
        entry.normalize = normalize;
        entry
    }

    fn profile() -> NormalizeRef {
        NormalizeRef {
            tool_profile_hash: hash_bytes(b"profile"),
            r#gen: 7,
            manifest_hash: hash_bytes(b"manifest"),
        }
    }

    #[test]
    fn all_tagged_linear_history_uses_shared_ancestry_and_rejects_other_roots() {
        let names = (0..20_000)
            .map(|index| format!("commit-{index:05}"))
            .collect::<Vec<_>>();
        let mut edges = BTreeMap::new();
        for (index, name) in names.iter().enumerate() {
            edges.insert(
                name.clone(),
                index.checked_sub(1).map(|previous| names[previous].clone()),
            );
        }
        let mut roots = names.iter().cloned().collect::<BTreeSet<_>>();
        super::ensure_acyclic_edges(&edges).unwrap();
        assert_eq!(
            super::linear_tip(&edges, &roots).unwrap(),
            *names.last().unwrap()
        );
        edges.insert("detached".into(), None);
        roots.insert("detached".into());
        assert!(super::linear_tip(&edges, &roots).is_err());
        roots.remove("detached");
        edges.insert(names[0].clone(), Some(names[1].clone()));
        assert!(super::ensure_acyclic_edges(&edges).is_err());
        edges.remove(&names[1]);
        assert!(super::linear_tip(&edges, &roots).is_err());
    }

    #[test]
    fn linear_history_has_one_earliest_introduction() {
        let fixture = Fixture::new();
        let empty = fixture.tree(Vec::new());
        let with_x = fixture.tree(vec![entry("x.md", b"x", Some(profile()))]);
        let root = fixture.commit("root", &empty, Vec::new());
        let first = fixture.commit("first", &with_x, vec![root.clone()]);
        let head = fixture.commit("head", &with_x, vec![first.clone()]);

        let graph = HistoryReader::new(&fixture.kio_dir).walk(&head).unwrap();
        let binding = TreeBinding {
            path: "x.md".to_owned(),
            raw_hash: hash_bytes(b"x"),
            normalize: Some(profile()),
        };
        let candidates = graph.introduction_candidates(&binding);
        let expected = vec![first.as_str()];
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.commit_hash.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(graph.ancestor_most_introductions(&binding), candidates);
        assert_eq!(
            graph.canonical_introduction(&binding).unwrap().commit_hash,
            first
        );
    }

    #[test]
    fn linear_walk_finds_historical_binding_after_deletion() {
        let fixture = Fixture::new();
        let empty = fixture.tree(Vec::new());
        let with_x = fixture.tree(vec![entry("side.md", b"side", Some(profile()))]);
        let root = fixture.commit("root", &empty, Vec::new());
        let side = fixture.commit("side", &with_x, vec![root]);
        let head = fixture.commit("delete-side", &empty, vec![side.clone()]);

        let graph = HistoryReader::new(&fixture.kio_dir).walk(&head).unwrap();
        let binding = TreeBinding {
            path: "side.md".to_owned(),
            raw_hash: hash_bytes(b"side"),
            normalize: Some(profile()),
        };
        let historical = graph.canonical_introduction(&binding).unwrap();
        assert_eq!(historical.commit_hash, side);
        assert_eq!(historical.binding.path, "side.md");
    }

    #[test]
    fn root_union_requires_one_linear_chain() {
        let fixture = Fixture::new();
        let empty = fixture.tree(Vec::new());
        let root = fixture.commit("root", &empty, Vec::new());
        let head = fixture.commit("head", &empty, vec![root.clone()]);
        let roots = BTreeSet::from([root.clone(), head.clone()]);

        let graph = HistoryReader::new(&fixture.kio_dir)
            .walk_for_roots(&roots)
            .unwrap();

        assert_eq!(graph.stats().commits, 2);
        assert!(graph.node(&root).is_some());
        assert!(graph.node(&head).is_some());
        assert_eq!(
            graph
                .nodes_in_visit_order()
                .map(|node| node.commit_hash.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            2
        );

        let roots_with_missing = BTreeSet::from([head, hash_bytes(b"missing-root")]);
        assert!(
            HistoryReader::new(&fixture.kio_dir)
                .walk_for_roots(&roots_with_missing)
                .is_err()
        );
    }

    #[test]
    fn ancestor_most_introduction_removes_later_reintroduction() {
        let fixture = Fixture::new();
        let empty = fixture.tree(Vec::new());
        let with_x = fixture.tree(vec![entry("x.md", b"x", Some(profile()))]);
        let root = fixture.commit("root-add", &with_x, Vec::new());
        let deletion = fixture.commit("delete", &empty, vec![root.clone()]);
        let readd = fixture.commit("readd", &with_x, vec![deletion]);

        let graph = HistoryReader::new(&fixture.kio_dir).walk(&readd).unwrap();
        let binding = TreeBinding {
            path: "x.md".to_owned(),
            raw_hash: hash_bytes(b"x"),
            normalize: Some(profile()),
        };
        assert_eq!(graph.introduction_candidates(&binding).len(), 2);
        let ancestor_most = graph.ancestor_most_introductions(&binding);
        assert_eq!(ancestor_most.len(), 1);
        assert_eq!(ancestor_most[0].commit_hash, root);
    }

    #[test]
    fn root_union_reconstructs_newest_first_order_when_hash_order_is_opposite() {
        let fixture = Fixture::new();
        let empty = fixture.tree(Vec::new());
        let with_x = fixture.tree(vec![entry("x.md", b"x", Some(profile()))]);
        let root = fixture.commit("root", &empty, Vec::new());
        let (first, deletion, readd) = (0_u32..1024)
            .find_map(|attempt| {
                let first =
                    fixture.commit(&format!("first-{attempt}"), &with_x, vec![root.clone()]);
                let deletion =
                    fixture.commit(&format!("delete-{attempt}"), &empty, vec![first.clone()]);
                let readd =
                    fixture.commit(&format!("readd-{attempt}"), &with_x, vec![deletion.clone()]);
                (first < readd).then_some((first, deletion, readd))
            })
            .expect("hash prefix search must find a lexically earlier ancestor");
        let roots = BTreeSet::from([first.clone(), readd.clone()]);

        let history = HistoryReader::new(&fixture.kio_dir)
            .walk_for_roots(&roots)
            .unwrap();
        assert_eq!(history.start_commit(), readd);
        assert_eq!(
            history
                .nodes_in_visit_order()
                .map(|node| node.commit_hash.clone())
                .collect::<Vec<_>>(),
            vec![readd.clone(), deletion, first.clone(), root],
        );
        let binding = TreeBinding {
            path: "x.md".to_owned(),
            raw_hash: hash_bytes(b"x"),
            normalize: Some(profile()),
        };
        assert_eq!(
            history
                .canonical_introduction(&binding)
                .unwrap()
                .commit_hash,
            first
        );
    }

    #[test]
    fn linear_walk_derives_final_deleted_binding_and_preserves_none_normalize() {
        let fixture = Fixture::new();
        let old = fixture.tree(vec![entry("old.md", b"old", None)]);
        let current = fixture.tree(vec![entry("live.md", b"live", Some(profile()))]);
        let root = fixture.commit("old", &old, Vec::new());
        let head = fixture.commit("head", &current, vec![root.clone()]);

        let history = HistoryReader::new(&fixture.kio_dir).walk(&head).unwrap();
        let newest = history.newest_binding_for_path("old.md").unwrap();
        assert_eq!(newest.commit_hash, root);
        assert_eq!(newest.binding.normalize, None);
        let deleted = history.final_deleted_bindings();
        assert_eq!(deleted, vec![newest]);
        assert!(
            history
                .final_deleted_bindings()
                .iter()
                .all(|binding| binding.binding.path != "live.md")
        );

        let graph = HistoryReader::new(&fixture.kio_dir).walk(&head).unwrap();
        let binding = TreeBinding {
            path: "old.md".to_owned(),
            raw_hash: hash_bytes(b"old"),
            normalize: None,
        };
        assert_eq!(
            graph
                .canonical_introduction(&binding)
                .unwrap()
                .binding
                .normalize,
            None
        );
    }

    #[test]
    fn exact_aggregate_boundaries_succeed_and_one_beyond_fails_in_each_walk() {
        let fixture = Fixture::new();
        let tree = fixture.tree(vec![entry("x.md", b"x", Some(profile()))]);
        let root = fixture.commit("root", &tree, Vec::new());
        let head = fixture.commit("head", &tree, vec![root]);
        let baseline = HistoryReader::new(&fixture.kio_dir)
            .walk(&head)
            .unwrap()
            .stats();
        assert_eq!(
            HistoryReader::new(&fixture.kio_dir)
                .walk(&head)
                .unwrap()
                .stats(),
            baseline
        );

        let exact = HistoryLimits::new(
            baseline.commits,
            baseline.tree_entries,
            baseline.verified_bytes,
        );
        let exact_reader = HistoryReader::with_limits(&fixture.kio_dir, exact);
        assert_eq!(exact_reader.walk(&head).unwrap().stats(), baseline);
        // A second invocation proves counters are fresh per walk rather than
        // retained on the reader.
        assert_eq!(exact_reader.walk(&head).unwrap().stats(), baseline);

        let cases = [
            (
                "commits",
                HistoryLimits::new(
                    baseline.commits - 1,
                    baseline.tree_entries,
                    baseline.verified_bytes,
                ),
            ),
            (
                "tree_entries",
                HistoryLimits::new(
                    baseline.commits,
                    baseline.tree_entries - 1,
                    baseline.verified_bytes,
                ),
            ),
            (
                "verified_bytes",
                HistoryLimits::new(
                    baseline.commits,
                    baseline.tree_entries,
                    baseline.verified_bytes - 1,
                ),
            ),
        ];
        for (expected_dimension, limits) in cases {
            let reader = HistoryReader::with_limits(&fixture.kio_dir, limits);
            let error = reader.walk(&head).unwrap_err();
            assert_eq!(error.error_code(), "KIO-E-COMMIT-HISTORY-LIMIT-001");
            assert_eq!(error.context()["exceeded"], json!(expected_dimension));
        }
    }

    /// R23-15 (06 §8 L403 "単独操作 exit 4"): a standalone history walk
    /// (this is the same `HistoryReader` call restore/purge use directly for
    /// their own ancestor checks) that overruns the aggregate cap is a
    /// PERMANENT failure -- exit 4, not the generic exit 1
    /// `history_limit_error` returned before the fix.
    #[test]
    fn r23_15_history_limit_error_is_permanent_failure_exit_4() {
        let fixture = Fixture::new();
        let tree = fixture.tree(vec![entry("x.md", b"x", Some(profile()))]);
        let root = fixture.commit("root", &tree, Vec::new());
        let head = fixture.commit("head", &tree, vec![root]);
        let baseline = HistoryReader::new(&fixture.kio_dir)
            .walk(&head)
            .unwrap()
            .stats();
        let limits = HistoryLimits::new(
            baseline.commits - 1,
            baseline.tree_entries,
            baseline.verified_bytes,
        );
        let reader = HistoryReader::with_limits(&fixture.kio_dir, limits);
        let error = reader.walk(&head).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-COMMIT-HISTORY-LIMIT-001");
        assert_eq!(error.exit_code(), crate::ExitCode::PermanentFailure);
    }

    #[test]
    fn missing_commit_or_tree_is_reported_as_shallow_with_object_cause() {
        let fixture = Fixture::new();
        let empty = fixture.tree(Vec::new());
        let missing_parent = hash_bytes(b"missing-parent");
        let head = fixture.commit("head", &empty, vec![missing_parent.clone()]);
        assert_eq!(
            HistoryReader::new(&fixture.kio_dir)
                .snapshot(&head)
                .unwrap()
                .commit_hash,
            head
        );
        let error = HistoryReader::new(&fixture.kio_dir)
            .walk(&head)
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-COMMIT-SHALLOW-001");
        assert_eq!(error.context()["commit_hash"], json!(missing_parent));
        assert_eq!(error.context()["missing_object_kind"], json!("commit"));

        // Keep this missing-tree case separate from the dangling-parent case
        // above: final shallow inventory validates every commit link globally.
        let fixture = Fixture::new();
        let missing_tree = hash_bytes(b"missing-tree");
        let shallow = fixture.commit("shallow", &missing_tree, Vec::new());
        let error = HistoryReader::new(&fixture.kio_dir)
            .walk(&shallow)
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
        assert_eq!(error.context()["commit_hash"], json!(shallow));

        let raw_only_tree = fixture.store.write_raw(b"raw-only-tree").unwrap();
        let shallow = fixture.commit("raw-is-not-tree", &raw_only_tree, Vec::new());
        let error = HistoryReader::new(&fixture.kio_dir)
            .snapshot(&shallow)
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }

    #[test]
    fn reader_revalidates_cas_instead_of_reusing_previous_walk_truth() {
        let fixture = Fixture::new();
        let tree = fixture.tree(Vec::new());
        let head = fixture.commit("head", &tree, Vec::new());
        let reader = HistoryReader::new(&fixture.kio_dir);
        reader.walk(&head).unwrap();

        let tree_path = fixture.store.object_path(ObjectKind::Tree, &tree).unwrap();
        fs::remove_file(tree_path).unwrap();
        let error = reader.walk(&head).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }

    #[test]
    fn root_union_rejects_sibling_branches() {
        let fixture = Fixture::new();
        let empty = fixture.tree(Vec::new());
        let root = fixture.commit("root", &empty, Vec::new());
        let main = fixture.commit("main", &empty, vec![root.clone()]);
        let side = fixture.commit("side", &empty, vec![root]);
        let roots = BTreeSet::from([main, side]);
        assert!(
            HistoryReader::new(&fixture.kio_dir)
                .walk_for_roots(&roots)
                .is_err()
        );
    }

    /// PC45: a linear walk with a shallow (tree-discarded) *ancestor*
    /// skips it and keeps walking through commits reachable beyond it, instead
    /// of failing the whole walk does
    /// (regression guard: the non-tolerant path is unchanged).
    #[test]
    fn pc45_linear_walk_allow_shallowed_skips_an_ancestor_and_keeps_walking() {
        let fixture = Fixture::new();
        let root_tree = fixture.tree(vec![entry("root.md", b"root", Some(profile()))]);
        let root = fixture.commit("root", &root_tree, Vec::new());
        let missing_tree = hash_bytes(b"pc45-missing-tree");
        let shallow_mid = fixture.commit("shallow-mid", &missing_tree, vec![root.clone()]);
        fixture.shallow_receipt(shallow_mid.clone(), missing_tree);
        let head_tree = fixture.tree(vec![entry("head.md", b"head", Some(profile()))]);
        let head = fixture.commit("head", &head_tree, vec![shallow_mid.clone()]);

        // Unchanged baseline: the non-tolerant walk still hard-fails.
        let error = HistoryReader::new(&fixture.kio_dir)
            .walk(&head)
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-COMMIT-SHALLOW-001");

        let (graph, shallow_skipped) = HistoryReader::new(&fixture.kio_dir)
            .walk_allow_shallowed(&head)
            .unwrap();
        assert_eq!(shallow_skipped, vec![shallow_mid.clone()]);
        // The walk continued past the shallow node to its own parent.
        assert!(graph.node(&root).is_some());
        assert!(graph.node(&head).is_some());
        assert!(graph.node(&shallow_mid).is_none());
        // The root's binding is still reachable through the shallow boundary —
        // `canonical_introduction` (which depends on the generalized
        // `ancestor_most_introductions` topology) resolves it correctly rather
        // than silently dropping it.
        let binding = TreeBinding {
            path: "root.md".to_owned(),
            raw_hash: hash_bytes(b"root"),
            normalize: Some(profile()),
        };
        assert_eq!(
            graph.canonical_introduction(&binding).unwrap().commit_hash,
            root
        );
    }

    /// Tolerance is not a generic missing-tree escape hatch: a missing
    /// ancestor tree without the exact durable shallow receipt is corruption.
    #[test]
    fn tolerant_walk_rejects_unreceipted_missing_ancestor_tree() {
        let fixture = Fixture::new();
        let root_tree = fixture.tree(Vec::new());
        let root = fixture.commit("root", &root_tree, Vec::new());
        let missing_tree = hash_bytes(b"unreceipted-missing-tree");
        let missing = fixture.commit("missing", &missing_tree, vec![root]);
        let head_tree = fixture.tree(Vec::new());
        let head = fixture.commit("head", &head_tree, vec![missing.clone()]);

        let error = HistoryReader::new(&fixture.kio_dir)
            .walk_allow_shallowed(&head)
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
        assert_eq!(error.context()["commit_hash"], json!(missing));
    }

    #[test]
    fn multi_root_tolerant_walk_keeps_every_root_strict() {
        let fixture = Fixture::new();
        let healthy_tree = fixture.tree(Vec::new());
        let healthy = fixture.commit("healthy", &healthy_tree, Vec::new());
        let missing_tree = hash_bytes(b"receipt-backed-root-must-still-fail");
        let shallow_root = fixture.commit("shallow-root", &missing_tree, Vec::new());
        fixture.shallow_receipt(shallow_root.clone(), missing_tree);
        let roots = BTreeSet::from([healthy, shallow_root.clone()]);

        let error = HistoryReader::new(&fixture.kio_dir)
            .walk_for_roots_allow_shallowed(&roots)
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-COMMIT-SHALLOW-001");
        assert_eq!(error.context()["commit_hash"], json!(shallow_root));
    }

    /// PC47: the *start* commit of a tolerant walk still hard-fails when its own
    /// tree is shallow — only a deeper ancestor is tolerated (PC45's skip is not
    /// a blanket exemption; a `--cursor` replay or `--at <shallow-commit>` needs
    /// the whole tree of the exact commit it targets).
    #[test]
    fn pc47_linear_walk_allow_shallowed_hard_fails_when_start_is_shallow() {
        let fixture = Fixture::new();
        let missing_tree = hash_bytes(b"pc47-missing-tree");
        let shallow_head = fixture.commit("shallow-head", &missing_tree, Vec::new());
        fixture.shallow_receipt(shallow_head.clone(), missing_tree);
        let error = HistoryReader::new(&fixture.kio_dir)
            .walk_allow_shallowed(&shallow_head)
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-COMMIT-SHALLOW-001");
        assert_eq!(error.context()["commit_hash"], json!(shallow_head));
    }

    /// PC45's include-deleted path uses the same linear traversal: a shallow
    /// ancestor is skipped and the walk keeps
    /// going through commits beyond it.
    #[test]
    fn pc45_linear_walk_allow_shallowed_skips_a_shallow_ancestor() {
        let fixture = Fixture::new();
        let root_tree = fixture.tree(Vec::new());
        let root = fixture.commit("root", &root_tree, Vec::new());
        let missing_tree = hash_bytes(b"pc45-fp-missing-tree");
        let shallow_mid = fixture.commit("shallow-mid", &missing_tree, vec![root.clone()]);
        fixture.shallow_receipt(shallow_mid.clone(), missing_tree);
        let head_tree = fixture.tree(Vec::new());
        let head = fixture.commit("head", &head_tree, vec![shallow_mid.clone()]);

        let (history, shallow_skipped) = HistoryReader::new(&fixture.kio_dir)
            .walk_allow_shallowed(&head)
            .unwrap();
        assert_eq!(shallow_skipped, vec![shallow_mid]);
        assert!(history.node(&root).is_some());
        assert!(history.node(&head).is_some());
    }

    /// A commit object that is itself missing (as opposed to just its tree) is
    /// never shallow-tolerated, at any position in the walk — shallow GC only
    /// ever discards trees (§2.2), so a missing commit is corruption.
    #[test]
    fn pc45_tolerant_walk_does_not_tolerate_a_missing_commit_object() {
        let fixture = Fixture::new();
        let tree = fixture.tree(Vec::new());
        let missing_parent = hash_bytes(b"pc45-missing-commit");
        let head = fixture.commit("head", &tree, vec![missing_parent.clone()]);
        let error = HistoryReader::new(&fixture.kio_dir)
            .walk_allow_shallowed(&head)
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    }

    #[test]
    fn pc45_tolerant_walk_handles_one_shallow_ancestor() {
        let fixture = Fixture::new();
        let empty = fixture.tree(Vec::new());
        let root = fixture.commit("root", &empty, Vec::new());
        let missing_tree = hash_bytes(b"pc45-linear-missing");
        let shallow = fixture.commit("shallow", &missing_tree, vec![root.clone()]);
        fixture.shallow_receipt(shallow.clone(), missing_tree);
        let head = fixture.commit("head", &empty, vec![shallow.clone()]);

        let (graph, shallow_skipped) = HistoryReader::new(&fixture.kio_dir)
            .walk_allow_shallowed(&head)
            .unwrap();
        assert_eq!(shallow_skipped, vec![shallow]);
        assert!(graph.node(&root).is_some());
        assert!(graph.node(&head).is_some());
    }
}
