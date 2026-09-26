//! CAS-backed planning for Step 4 time-travel search bindings.
//!
//! SQLite tree projections and the mutable manifest are deliberately absent from
//! this module.  Every eligible binding and backing commit is derived from
//! verified commit/tree objects through [`HistoryReader`].

use std::collections::{BTreeMap, BTreeSet};

use kio_core::history::{HistoryBinding, HistoryReader, LinearHistory, TreeBinding};
use kio_core::scope::Repository;
use kio_core::{KioError, Result};
use rusqlite::Connection;

use crate::search_time::{TimeSelector, validate_cursor_cutoff};

/// The exact normalized identity used to join a historical tree binding to the
/// append-only chunk table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SearchContentKey {
    pub raw_hash: String,
    pub tool_profile_hash: String,
    pub r#gen: u64,
    /// A generation can be retried with a different pinned manifest.  It is
    /// consequently part of the searchable normalized identity.
    pub manifest_hash: String,
}

/// One deterministic path alias for an eligible normalized identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHistoryBinding {
    pub raw_hash: String,
    pub tool_profile_hash: String,
    pub r#gen: u64,
    pub manifest_hash: String,
    pub path_at_commit: String,
    pub pointer_commit: String,
    /// Populated only for all-history/since aliases.  Paths are distinct and in
    /// UTF-8 byte order.  Raw twins intentionally remain multiple entries.
    pub current_paths: Vec<String>,
    /// Whether this exact path/identity binding occurs at the page-1 snapshot.
    pub is_live: bool,
}

impl SearchHistoryBinding {
    #[must_use]
    pub fn content_key(&self) -> SearchContentKey {
        SearchContentKey {
            raw_hash: self.raw_hash.clone(),
            tool_profile_hash: self.tool_profile_hash.clone(),
            r#gen: self.r#gen,
            manifest_hash: self.manifest_hash.clone(),
        }
    }

    /// Compatibility singular current path.  It is absent for both zero paths
    /// and identical-byte twins.
    #[must_use]
    pub fn current_path(&self) -> Option<&str> {
        match self.current_paths.as_slice() {
            [only] => Some(only.as_str()),
            _ => None,
        }
    }
}

/// The immutable history relation for one searched scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHistoryPlan {
    pub snapshot_commit: String,
    /// The already-frozen page-1 cutoff.  Filtering `chunks.created_at` remains
    /// the caller's responsibility because timestamps are index metadata, not
    /// commit/tree CAS data.
    pub since_cutoff: Option<String>,
    pub bindings: Vec<SearchHistoryBinding>,
    /// PC45/PC46 (05 §1.6/§2.2): shallow ancestors this plan's history walk
    /// skipped rather than hard-failing on (sorted, deduped). Always empty for
    /// `Current`/`At`, which never walk ancestry.
    pub shallow_skipped: Vec<String>,
}

impl SearchHistoryPlan {
    /// Stable semantic groups for attaching aliases after unique-chunk ranking.
    /// `BTreeMap` orders the normalized identities, and each value preserves the
    /// frozen `(path_at_commit, pointer_commit)` byte order.
    #[must_use]
    pub fn grouped_bindings(&self) -> BTreeMap<SearchContentKey, Vec<SearchHistoryBinding>> {
        let mut groups = BTreeMap::<SearchContentKey, Vec<SearchHistoryBinding>>::new();
        for binding in &self.bindings {
            groups
                .entry(binding.content_key())
                .or_default()
                .push(binding.clone());
        }
        groups
    }
}

/// Build current-selector eligibility from the existing source SQLite projection.
/// This is writer/repair-only input to a complete replica projection; direct
/// search never calls it or opens the source index. Explicit historical
/// selectors use their corresponding resolver plans instead.
pub(super) fn current_history_plan_from_cache(
    conn: &Connection,
    snapshot_commit: &str,
) -> Result<SearchHistoryPlan> {
    let mut stmt = conn
        .prepare(
            "SELECT raw_hash, tool_profile_hash, gen, manifest_hash, path
             FROM tree_entries
             WHERE commit_hash = ?1 AND tool_profile_hash IS NOT NULL
             ORDER BY raw_hash, tool_profile_hash, gen, path",
        )
        .map_err(|error| KioError::schema(error.to_string()))?;
    let rows = stmt
        .query_map(rusqlite::params![snapshot_commit], |row| {
            Ok(SearchHistoryBinding {
                raw_hash: row.get(0)?,
                tool_profile_hash: row.get(1)?,
                r#gen: row.get::<_, i64>(2)? as u64,
                manifest_hash: row.get(3)?,
                path_at_commit: row.get(4)?,
                pointer_commit: snapshot_commit.to_owned(),
                current_paths: Vec::new(),
                is_live: true,
            })
        })
        .map_err(|error| KioError::schema(error.to_string()))?;
    let mut by_key = BTreeMap::<SearchContentKey, SearchHistoryBinding>::new();
    for row in rows {
        let binding = row.map_err(|error| KioError::schema(error.to_string()))?;
        by_key.entry(binding.content_key()).or_insert(binding);
    }
    Ok(SearchHistoryPlan {
        snapshot_commit: snapshot_commit.to_owned(),
        since_cutoff: None,
        bindings: by_key.into_values().collect(),
        shallow_skipped: Vec::new(),
    })
}

pub(super) fn install_eligible_identities(
    conn: &Connection,
    plan: &SearchHistoryPlan,
) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| KioError::schema(error.to_string()))?;
    tx.execute_batch(
        "DROP TABLE IF EXISTS temp.kio_eligible_identity;
         CREATE TEMP TABLE kio_eligible_identity (
             raw_hash TEXT NOT NULL,
             tool_profile_hash TEXT NOT NULL,
             gen INTEGER NOT NULL,
             manifest_hash TEXT NOT NULL,
             PRIMARY KEY(raw_hash, tool_profile_hash, gen, manifest_hash)
         ) WITHOUT ROWID;",
    )
    .map_err(|error| KioError::schema(error.to_string()))?;
    for key in plan.grouped_bindings().keys() {
        tx.execute(
            "INSERT INTO kio_eligible_identity(raw_hash, tool_profile_hash, gen, manifest_hash)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                key.raw_hash,
                key.tool_profile_hash,
                key.r#gen as i64,
                key.manifest_hash
            ],
        )
        .map_err(|error| KioError::schema(error.to_string()))?;
    }
    tx.commit()
        .map_err(|error| KioError::schema(error.to_string()))
}

/// Build the eligible content/path relation for one scope.
///
/// `snapshot_commit` must already be resolved independently for the scope.  For
/// `--at`, that is the resolved operand; for every other mode it is the frozen
/// page-1 HEAD.  A `--since` cutoff is validated and carried through unchanged,
/// but the planner intentionally does not inspect `chunks.created_at`.
pub fn plan_search_history(
    repo: &Repository,
    snapshot_commit: &str,
    selector: &TimeSelector,
    since_cutoff: Option<&str>,
) -> Result<SearchHistoryPlan> {
    validate_cursor_cutoff(selector, since_cutoff)?;
    let reader = HistoryReader::new(repo.kio_dir());
    let mut shallow_skipped = Vec::new();
    let bindings = match selector {
        TimeSelector::Current | TimeSelector::At(_) => {
            let snapshot = reader.snapshot(snapshot_commit)?;
            let mut live_by_key = BTreeMap::<SearchContentKey, SearchHistoryBinding>::new();
            for entry in &snapshot.tree.entries {
                let Some(normalize) = &entry.normalize else {
                    continue;
                };
                let key = SearchContentKey {
                    raw_hash: entry.raw_hash.clone(),
                    tool_profile_hash: normalize.tool_profile_hash.clone(),
                    r#gen: normalize.r#gen,
                    manifest_hash: normalize.manifest_hash.clone(),
                };
                let candidate = SearchHistoryBinding {
                    raw_hash: entry.raw_hash.clone(),
                    tool_profile_hash: normalize.tool_profile_hash.clone(),
                    r#gen: normalize.r#gen,
                    manifest_hash: normalize.manifest_hash.clone(),
                    path_at_commit: entry.path.clone(),
                    pointer_commit: snapshot_commit.to_owned(),
                    current_paths: Vec::new(),
                    is_live: true,
                };
                // Ranking is path-independent.  Default/--at therefore retain
                // one deterministic display/evidence binding for live twins.
                live_by_key
                    .entry(key)
                    .and_modify(|current| {
                        if candidate.path_at_commit.as_bytes() < current.path_at_commit.as_bytes() {
                            *current = candidate.clone();
                        }
                    })
                    .or_insert(candidate);
            }
            live_by_key.into_values().collect()
        }
        // PC45/PC46 (05 §1.6/§2.2): a shallow ancestor beyond the walk's own
        // start commit is skipped and recorded rather than hard-failing the
        // whole scope — `walk_allow_shallowed` still
        // hard-fail exactly like the non-tolerant path when the *start*
        // commit itself (`snapshot_commit`) is shallow (PC47).
        TimeSelector::AllHistory | TimeSelector::Since(_) => {
            let (graph, skipped) = reader.walk_allow_shallowed(snapshot_commit)?;
            shallow_skipped = skipped;
            plan_all_history(&graph)?
        }
        TimeSelector::IncludeDeleted => {
            let (history, skipped) = reader.walk_allow_shallowed(snapshot_commit)?;
            shallow_skipped = skipped;
            plan_include_deleted(&history)?
        }
    };

    let mut plan = SearchHistoryPlan {
        snapshot_commit: snapshot_commit.to_owned(),
        since_cutoff: since_cutoff.map(str::to_owned),
        bindings,
        shallow_skipped,
    };
    sort_bindings(&mut plan.bindings);
    Ok(plan)
}

/// PC38/PC39 (05 §1.6): the ancestor-or-equal commit set of `--at`'s target
/// commit, used to enforce the "introduction is ancestor-or-equal of the
/// target" time-point condition against durable publication events.
/// Tolerant of a shallow ancestor beyond the
/// target itself, exactly like [`plan_search_history`]'s `--all-history` walk
/// (PC45's skip-and-continue policy) — `shallow_skipped` names what was
/// skipped; a shallow-skipped commit is conservatively absent from the
/// returned set (a chunk introduced there fails the ancestor check rather
/// than the whole `--at` query hard-failing over an unrelated shallow
/// ancestor).
pub fn at_target_ancestors(
    repo: &Repository,
    commit: &str,
) -> Result<(BTreeSet<String>, Vec<String>)> {
    let (graph, shallow_skipped) =
        HistoryReader::new(repo.kio_dir()).walk_allow_shallowed(commit)?;
    let ancestors = graph
        .nodes_in_visit_order()
        .map(|node| node.commit_hash.clone())
        .collect::<BTreeSet<_>>();
    Ok((ancestors, shallow_skipped))
}

fn plan_all_history(graph: &LinearHistory) -> Result<Vec<SearchHistoryBinding>> {
    // TreeBinding equality includes path, raw identity, and the exact persisted
    // normalize value.  Repeated appearances in later commits therefore collapse
    // before choosing the canonical ancestor-most introduction.
    let distinct = graph
        .bindings()
        .into_iter()
        .map(|appearance| appearance.binding)
        .filter(|binding| binding.normalize.is_some())
        .collect::<BTreeSet<_>>();
    let live_bindings = graph
        .node(graph.start_commit())
        .ok_or_else(|| KioError::schema("history graph is missing its snapshot commit"))?
        .tree
        .entries
        .iter()
        .map(TreeBinding::from)
        .collect::<BTreeSet<_>>();

    distinct
        .into_iter()
        .map(|binding| {
            let introduction = graph.canonical_introduction(&binding).ok_or_else(|| {
                KioError::schema("historical binding has no canonical introduction commit")
            })?;
            let normalize = binding
                .normalize
                .as_ref()
                .expect("bindings without normalize were filtered above");
            Ok(SearchHistoryBinding {
                raw_hash: binding.raw_hash.clone(),
                tool_profile_hash: normalize.tool_profile_hash.clone(),
                r#gen: normalize.r#gen,
                manifest_hash: normalize.manifest_hash.clone(),
                path_at_commit: binding.path.clone(),
                pointer_commit: introduction.commit_hash,
                current_paths: graph.snapshot_paths_for_raw(&binding.raw_hash),
                is_live: live_bindings.contains(&binding),
            })
        })
        .collect()
}

fn plan_include_deleted(history: &LinearHistory) -> Result<Vec<SearchHistoryBinding>> {
    let snapshot = history
        .node(history.start_commit())
        .ok_or_else(|| KioError::schema("linear history is missing its snapshot commit"))?;

    // A live semantic identity wins over every deleted-path alias.  When several
    // live paths share it, the contract selects the UTF-8-bytewise-smallest path.
    let mut live_by_key = BTreeMap::<SearchContentKey, SearchHistoryBinding>::new();
    for entry in &snapshot.tree.entries {
        let Some(normalize) = &entry.normalize else {
            continue;
        };
        let key = SearchContentKey {
            raw_hash: entry.raw_hash.clone(),
            tool_profile_hash: normalize.tool_profile_hash.clone(),
            r#gen: normalize.r#gen,
            manifest_hash: normalize.manifest_hash.clone(),
        };
        let candidate = SearchHistoryBinding {
            raw_hash: entry.raw_hash.clone(),
            tool_profile_hash: normalize.tool_profile_hash.clone(),
            r#gen: normalize.r#gen,
            manifest_hash: normalize.manifest_hash.clone(),
            path_at_commit: entry.path.clone(),
            pointer_commit: history.start_commit().to_owned(),
            current_paths: Vec::new(),
            is_live: true,
        };
        live_by_key
            .entry(key)
            .and_modify(|current| {
                if candidate.path_at_commit.as_bytes() < current.path_at_commit.as_bytes() {
                    *current = candidate.clone();
                }
            })
            .or_insert(candidate);
    }

    let live_keys = live_by_key.keys().cloned().collect::<BTreeSet<_>>();
    let mut planned = live_by_key.into_values().collect::<Vec<_>>();
    for deleted in history.final_deleted_bindings() {
        let Some(deleted) = searchable_final_deleted_binding(history, deleted) else {
            continue;
        };
        let normalize = deleted
            .binding
            .normalize
            .clone()
            .expect("searchable deleted binding has normalize metadata");
        let key = SearchContentKey {
            raw_hash: deleted.binding.raw_hash.clone(),
            tool_profile_hash: normalize.tool_profile_hash.clone(),
            r#gen: normalize.r#gen,
            manifest_hash: normalize.manifest_hash.clone(),
        };
        if live_keys.contains(&key) {
            continue;
        }
        planned.push(SearchHistoryBinding {
            raw_hash: deleted.binding.raw_hash,
            tool_profile_hash: normalize.tool_profile_hash,
            r#gen: normalize.r#gen,
            manifest_hash: normalize.manifest_hash,
            path_at_commit: deleted.binding.path,
            pointer_commit: deleted.commit_hash,
            current_paths: Vec::new(),
            is_live: false,
        });
    }
    Ok(planned)
}

/// A bare manual snapshot intentionally persists `normalize=None`. When that is
/// the final appearance before deletion, include-deleted may still use the newest
/// earlier normalized appearance of the *same path and raw bytes*. This neither
/// substitutes a different historical version nor consults mutable cache state;
/// the returned commit/tree attests the exact normalize identity used by the
/// pointer. If the final raw version was never normalized, it remains ineligible.
fn searchable_final_deleted_binding(
    history: &LinearHistory,
    deleted: HistoryBinding,
) -> Option<HistoryBinding> {
    if deleted.binding.normalize.is_some() {
        return Some(deleted);
    }
    history.nodes_in_visit_order().find_map(|node| {
        node.tree
            .entries
            .iter()
            .find(|entry| {
                entry.path == deleted.binding.path
                    && entry.raw_hash == deleted.binding.raw_hash
                    && entry.normalize.is_some()
            })
            .map(|entry| HistoryBinding {
                commit_hash: node.commit_hash.clone(),
                tree_hash: node.commit.tree.clone(),
                binding: TreeBinding::from(entry),
            })
    })
}

fn sort_bindings(bindings: &mut [SearchHistoryBinding]) {
    bindings.sort_by(|left, right| {
        left.raw_hash
            .as_bytes()
            .cmp(right.raw_hash.as_bytes())
            .then_with(|| {
                left.tool_profile_hash
                    .as_bytes()
                    .cmp(right.tool_profile_hash.as_bytes())
            })
            .then_with(|| left.r#gen.cmp(&right.r#gen))
            .then_with(|| {
                left.manifest_hash
                    .as_bytes()
                    .cmp(right.manifest_hash.as_bytes())
            })
            .then_with(|| {
                left.path_at_commit
                    .as_bytes()
                    .cmp(right.path_at_commit.as_bytes())
            })
            .then_with(|| {
                left.pointer_commit
                    .as_bytes()
                    .cmp(right.pointer_commit.as_bytes())
            })
    });
}

#[cfg(test)]
mod tests {
    use kio_core::cas::{ObjectKind, ObjectStore, hash_bytes};
    use kio_core::dag::{
        CommitObject, CommitStats, CommitType, NormalizeRef, TreeEntry, build_tree,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::search_time::PositiveDuration;

    struct Fixture {
        _temp: TempDir,
        repo: Repository,
        store: ObjectStore,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let repo = Repository::init(temp.path()).unwrap();
            let store = ObjectStore::new(repo.kio_dir());
            Self {
                _temp: temp,
                repo,
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

        fn commit(&self, label: &str, tree: &str, parent: Option<String>) -> String {
            let commit = CommitObject::new(
                tree.to_owned(),
                parent,
                "2026-07-13T00:00:00Z".to_owned(),
                label.to_owned(),
                hash_bytes(b"tool-lock"),
                CommitStats {
                    files_added: 0,
                    files_modified: 0,
                    files_deleted: 0,
                },
                CommitType::Manual,
            )
            .unwrap();
            self.store
                .write_json(ObjectKind::Commit, &serde_json::to_value(commit).unwrap())
                .unwrap()
                .0
        }
    }

    fn normalized(path: &str, body: &[u8], profile: &NormalizeRef) -> TreeEntry {
        let mut entry = TreeEntry::raw_file(path, hash_bytes(body)).unwrap();
        entry.normalize = Some(profile.clone());
        entry
    }

    fn raw_only(path: &str, body: &[u8]) -> TreeEntry {
        TreeEntry::raw_file(path, hash_bytes(body)).unwrap()
    }

    fn profile() -> NormalizeRef {
        NormalizeRef {
            tool_profile_hash: hash_bytes(b"profile"),
            r#gen: 3,
            manifest_hash: hash_bytes(b"manifest"),
        }
    }

    #[test]
    fn exact_snapshot_excludes_persisted_none_normalize() {
        let fixture = Fixture::new();
        let tree = fixture.tree(vec![
            normalized("a-included.md", b"included", &profile()),
            raw_only("not-normalized.md", b"raw"),
            normalized("z-included.md", b"included", &profile()),
        ]);
        let head = fixture.commit("head", &tree, None);

        let plan = plan_search_history(&fixture.repo, &head, &TimeSelector::At(head.clone()), None)
            .unwrap();
        assert_eq!(plan.bindings.len(), 1);
        assert_eq!(plan.bindings[0].path_at_commit, "a-included.md");
        assert_eq!(plan.bindings[0].pointer_commit, head);
        let groups = plan.grouped_bindings();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups.into_values().next().unwrap().len(), 1);
    }

    #[test]
    fn distinct_pinned_manifests_do_not_collapse_identity_groups() {
        let fixture = Fixture::new();
        let first = profile();
        let second = NormalizeRef {
            manifest_hash: hash_bytes(b"different manifest"),
            ..first.clone()
        };
        let tree = fixture.tree(vec![
            normalized("first.md", b"same raw", &first),
            normalized("second.md", b"same raw", &second),
        ]);
        let head = fixture.commit("head", &tree, None);

        let plan = plan_search_history(&fixture.repo, &head, &TimeSelector::Current, None).unwrap();
        assert_eq!(plan.bindings.len(), 2);
        assert_eq!(plan.grouped_bindings().len(), 2);
        assert_ne!(
            plan.bindings[0].manifest_hash,
            plan.bindings[1].manifest_hash
        );
    }

    #[test]
    fn all_history_keeps_linear_alias_and_canonical_introduction() {
        let fixture = Fixture::new();
        let empty = fixture.tree(Vec::new());
        let old = fixture.tree(vec![normalized("old.md", b"same", &profile())]);
        let current = fixture.tree(vec![
            normalized("copy.md", b"same", &profile()),
            normalized("new.md", b"same", &profile()),
        ]);
        let root = fixture.commit("root", &empty, None);
        let old_commit = fixture.commit("old", &old, Some(root));
        let head = fixture.commit("head", &current, Some(old_commit.clone()));

        let plan =
            plan_search_history(&fixture.repo, &head, &TimeSelector::AllHistory, None).unwrap();
        assert_eq!(
            plan.bindings
                .iter()
                .map(|binding| binding.path_at_commit.as_str())
                .collect::<Vec<_>>(),
            vec!["copy.md", "new.md", "old.md"]
        );
        let old_alias = plan
            .bindings
            .iter()
            .find(|binding| binding.path_at_commit == "old.md")
            .unwrap();
        assert_eq!(old_alias.pointer_commit, old_commit);
        assert_eq!(old_alias.current_paths, vec!["copy.md", "new.md"]);
        assert!(!old_alias.is_live);
        assert_eq!(old_alias.current_path(), None);
    }

    #[test]
    fn include_deleted_returns_final_versions_and_live_identity_wins() {
        let fixture = Fixture::new();
        let profile = profile();
        let old = fixture.tree(vec![
            normalized("deleted-a.md", b"deleted-old", &profile),
            normalized("renamed.md", b"live", &profile),
        ]);
        let final_deleted = fixture.tree(vec![
            normalized("deleted-a.md", b"deleted-final", &profile),
            normalized("deleted-b.md", b"deleted-final", &profile),
            normalized("renamed.md", b"live", &profile),
        ]);
        let bare_final_deleted = fixture.tree(vec![
            raw_only("deleted-a.md", b"deleted-final"),
            raw_only("deleted-b.md", b"deleted-final"),
            raw_only("renamed.md", b"live"),
        ]);
        let current = fixture.tree(vec![
            normalized("a-live.md", b"live", &profile),
            normalized("z-live.md", b"live", &profile),
        ]);
        let root = fixture.commit("old", &old, None);
        let middle = fixture.commit("final", &final_deleted, Some(root));
        let bare = fixture.commit("bare", &bare_final_deleted, Some(middle.clone()));
        let head = fixture.commit("head", &current, Some(bare));

        let plan =
            plan_search_history(&fixture.repo, &head, &TimeSelector::IncludeDeleted, None).unwrap();
        let mut paths = plan
            .bindings
            .iter()
            .map(|binding| binding.path_at_commit.as_str())
            .collect::<Vec<_>>();
        paths.sort_unstable();
        assert_eq!(paths, vec!["a-live.md", "deleted-a.md", "deleted-b.md"]);
        let live = plan
            .bindings
            .iter()
            .find(|binding| binding.path_at_commit == "a-live.md")
            .unwrap();
        assert_eq!(live.pointer_commit, head);
        assert!(live.is_live);
        assert!(
            plan.bindings
                .iter()
                .filter(|binding| !binding.is_live)
                .all(|binding| binding.pointer_commit == middle)
        );
        assert!(
            plan.bindings
                .iter()
                .all(|binding| binding.path_at_commit != "renamed.md")
        );
    }

    #[test]
    fn since_carries_the_frozen_cutoff_and_requires_it() {
        let fixture = Fixture::new();
        let tree = fixture.tree(vec![normalized("x.md", b"x", &profile())]);
        let head = fixture.commit("head", &tree, None);
        let selector = TimeSelector::Since(PositiveDuration::parse("7d").unwrap());

        let plan = plan_search_history(
            &fixture.repo,
            &head,
            &selector,
            Some("2026-07-06T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(plan.since_cutoff.as_deref(), Some("2026-07-06T00:00:00Z"));
        assert_eq!(
            plan_search_history(&fixture.repo, &head, &selector, None)
                .unwrap_err()
                .error_code(),
            "KIO-E-SEARCH-CURSOR-001"
        );
    }
}
