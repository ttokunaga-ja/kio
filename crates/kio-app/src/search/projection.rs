use crate::authorized_local_image_object_hash;
use crate::historical_reindex::pinned_done_units;
use crate::historical_reindex::retained_history_instances;
use crate::image_authority;
use crate::index::chunk_ledger::rotate_index_generation_unconditionally;
use crate::index::chunk_ledger::sqlite_path;
use crate::index_to_kio;
use crate::invocation::Invocation;
use crate::scope_access::purge_blocks_rebuild_raw;
use crate::scope_access::scope_id;
use crate::search::query::TreeEntryProjection;
use crate::search::query::current_max_rowid;
use crate::search::query::ensure_snapshot_tree_entries;
use crate::search::query::snapshot_chunking_config_hash;
use crate::search::query::source_tree_entry_snapshot_commits;
use crate::search::query::token_equivalence_forms;
use crate::search::scope::ChunkMeta;
use crate::search::scope::GlobalRanks;
use crate::search::scope::ReplicaCandidateRequest;
use crate::search::scope::ResultPayload;
use crate::search::scope::ScoredCandidate;
use crate::search::scope::aggregator_path;
use crate::search::scope::aggregator_selector;
use crate::search::scope::current_embedding_policy;
use crate::search::scope::redact_current_policy_paths;
use crate::search_history::SearchContentKey;
use crate::search_history::SearchHistoryBinding;
use crate::search_history::SearchHistoryPlan;
use crate::search_history::at_target_ancestors;
use crate::search_history::current_history_plan_from_cache;
use crate::search_history::install_eligible_identities;
use crate::search_history::plan_search_history;
use crate::search_time::TimeSelector;
use crate::verify_objects;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::dag::CommitType;
use kio_core::dag::NormalizeRef;
use kio_core::dag::TreeObject;
use kio_core::history::HistoryReader;
use kio_core::scope::Repository;
#[cfg(debug_assertions)]
use kio_core::test_control::AggregatorProjectionFault;
#[cfg(debug_assertions)]
use kio_core::test_control::ReplicaFault;
use kio_index::embedding_store;
use kio_index::fts::ExistingSourceIndexOpenMode;
use kio_index::fts::FtsSchemaConfig;
use kio_index::fts::FtsTokenizer;
use kio_index::fts::open_existing_source_index_connection;
use kio_search::object_uri::extract_related_images;
use kio_search::query::ScopeSelectionMode;
use kio_search::query::SearchMode;
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

/// Ask the already-open device replica for the only candidate pool used by
/// `kio search`.  All source-index projection happens on writer paths; this
/// function never falls back to a per-scope SQLite read.
pub(crate) fn replica_candidates_for(
    request: ReplicaCandidateRequest<'_>,
) -> std::result::Result<(GlobalRanks, Vec<ScoredCandidate>), String> {
    let ReplicaCandidateRequest {
        replica,
        searched,
        current_policies,
        image_authorities,
        eligible_images,
        exec_scopes,
        retainable_all_scopes,
        authoritative_all_scope_registry,
        scope_mode,
        is_cursor_replay,
        time_selector,
        since_cutoff,
        match_expr,
        short_tokens,
        mode,
        query_vec,
        config,
    } = request;
    let participating = searched
        .iter()
        .map(|scope| scope.scope_id.clone())
        .collect::<BTreeSet<_>>();
    // `searched` is intentionally only the set that passed this request's
    // readiness/mode checks.  It must constrain candidate rows, but it is not
    // the live device registry: a temporarily rebuilding or vector-incompatible
    // registered scope still owns a coherent replica projection.  The caller
    // instead passes every registry target whose scope store was reachable;
    // only a proven-unreachable target is eligible for eviction.
    if scope_mode == ScopeSelectionMode::All
        && !is_cursor_replay
        && authoritative_all_scope_registry
    {
        replica
            // Only a fresh all-scopes enumeration defines the whole registry
            // collection. Cursor/narrowed queries, and an all-scopes request
            // that fell back because the registry was unavailable, operate on
            // subsets and must never evict their siblings.
            .retain_scopes(retainable_all_scopes)
            .map_err(|error| format!("retain failed: {error}"))?;
    }
    let selector = aggregator_selector(time_selector);
    let replica_snapshots = searched
        .iter()
        .map(|scope| (scope.scope_id.clone(), scope.snapshot_at.clone()))
        .collect::<BTreeMap<_, _>>();
    let collection_generation = replica
        .collection_generation()
        .map_err(|error| format!("collection generation failed: {error}"))?;
    let short_forms = short_tokens
        .iter()
        .map(|token| token_equivalence_forms(token))
        .collect::<Vec<_>>();
    let mut runtime_binding_filters = Vec::new();
    let mut has_runtime_binding_filter = false;
    let mut missing_runtime_binding_filter = false;
    for scope in searched {
        match &scope.runtime_binding_filter {
            Some(filters) => {
                has_runtime_binding_filter = true;
                runtime_binding_filters.extend(filters.iter().cloned());
            }
            None => missing_runtime_binding_filter = true,
        }
    }
    if has_runtime_binding_filter && missing_runtime_binding_filter {
        return Err("mixed runtime CAS eligibility across one replica query".to_owned());
    }
    // Keep `Some(&[])`: an empty CAS plan is a verified empty historical
    // answer, not permission to use every durable replica alias.
    let binding_filter = has_runtime_binding_filter.then_some(runtime_binding_filters.as_slice());
    let rows = replica
        .search_candidates(&kio_index::aggregator::AggSearchRequest {
            scopes: &participating,
            snapshots: &replica_snapshots,
            selector,
            binding_filter,
            eligible_images,
            since_cutoff,
            match_expr,
            short_token_forms: &short_forms,
            query_embedding: query_vec,
            search_text: mode != SearchMode::Vector,
            search_vector: matches!(mode, SearchMode::Hybrid | SearchMode::Vector),
            candidate_depth: config.candidate_depth.max(1),
        })
        .map_err(|error| format!("candidate query failed: {error}"))?;
    let scope_by_id = exec_scopes
        .iter()
        .enumerate()
        .map(|(index, scope)| (scope.target.scope_id.as_str(), (index, scope)))
        .collect::<BTreeMap<_, _>>();
    let mut candidates = Vec::with_capacity(rows.len());
    for row in rows {
        let Some((scope_index, exec)) = scope_by_id.get(row.scope_id.as_str()).copied() else {
            continue;
        };
        let payload = match (&row.image_id, &row.image_uri) {
            (Some(image_hash), Some(image_uri))
                if authorized_local_image_object_hash(
                    &row.scope_id,
                    image_authorities
                        .get(&row.scope_id)
                        .expect("candidate scope has current image authority"),
                    image_uri,
                )
                .as_deref()
                    == Some(image_hash.as_str()) =>
            {
                ResultPayload::Image {
                    image_hash: image_hash.clone(),
                    image_uri: image_uri.clone(),
                }
            }
            (Some(_), Some(_)) => continue,
            _ => ResultPayload::Chunk,
        };
        let policy = current_policies
            .get(&row.scope_id)
            .expect("candidate scope participates in the current policy set");
        let mut bindings = Vec::with_capacity(row.bindings.len());
        for binding in row.bindings {
            let current_paths = redact_current_policy_paths(policy, binding.current_paths)
                .map_err(|error| format!("current policy path redaction failed: {error}"))?;
            bindings.push(SearchHistoryBinding {
                raw_hash: binding.raw_hash,
                tool_profile_hash: binding.tool_profile_hash,
                r#gen: binding.r#gen,
                manifest_hash: binding.manifest_hash,
                path_at_commit: binding.path_at_commit,
                pointer_commit: binding.pointer_commit,
                current_paths,
                is_live: binding.is_live,
            });
        }
        let text_term = row
            .text_rank
            .map(|rank| config.w_text / (config.k + rank) as f64)
            .unwrap_or(0.0);
        let vector_term = row
            .vector_rank
            .map(|rank| config.w_vector / (config.k + rank) as f64)
            .unwrap_or(0.0);
        candidates.push(ScoredCandidate {
            scope_index,
            scope_id: row.scope_id,
            scope_path: exec.target.kio_dir.clone(),
            chunk_hash: row.chunk_id,
            payload,
            rrf_score: text_term + vector_term,
            meta: ChunkMeta {
                raw_hash: row.raw_hash,
                tool_profile_hash: row.tool_profile_hash,
                r#gen: row.r#gen,
                heading_path: row
                    .heading_path
                    .and_then(|value| serde_json::from_str::<Vec<String>>(&value).ok()),
                section_id: row.section_id.filter(|value| !value.is_empty()),
                byte_start: row.byte_start,
                byte_end: row.byte_end,
                text: row.text,
                unit_key: row.unit_key,
            },
            bindings,
            embedding: row.embedding,
        });
    }
    Ok((
        GlobalRanks {
            collection_generation,
        },
        candidates,
    ))
}

/// One scope's complete replica projection.
///
/// Chunks, their image vectors, and the resolver's answer are deliberately
/// separate lists. Every committed chunk remains in the one FTS corpus, while
/// `bindings` decides which rows a particular selector may return.
struct ScopeProjection {
    chunks: Vec<kio_index::aggregator::AggChunk>,
    images: Vec<kio_index::aggregator::AggImage>,
    bindings: Vec<kio_index::aggregator::AggBinding>,
    /// Header facts which direct search must read from the replica instead of
    /// reopening this source index.
    current_snapshot_commit: Option<String>,
    current_chunking_config_hash: Option<String>,
    embedding_profiles: Vec<embedding_store::EmbeddingProfileSummary>,
    /// Every selector/snapshot the source resolver actually completed.  This
    /// includes valid empty answers so search can distinguish them from a
    /// missing projection without a lazy source read.
    completions: Vec<kio_index::aggregator::AggProjectionCompletion>,
    max_rowid: u64,
    max_association_rowid: u64,
}

/// Project one scope's committed chunks and resolved visibility bindings into
/// the replica.
///
/// The replica does not copy source liveness tables and reconstruct their SQL
/// predicates. Instead, the existing scope-side history planner resolves every
/// selector and this function persists that answer against the materialized
/// chunk. `agg_chunks` can therefore remain one corpus for current and
/// historical rows without treating every committed row as current.
///
/// `requested_at_snapshots` is writer-only input from an explicit historical
/// operation. It lets that writer publish a completed `--at` marker for a
/// valid empty, disconnected tree, which has no source `tree_entries` row to
/// discover during the ordinary projection scan. Readers never supply this
/// relation and never reopen source SQLite to make it up.
fn collect_scope_projection(
    kio_dir: &Path,
    repository: Option<&Repository>,
    scope_id: &str,
    completed_at_snapshots: &BTreeSet<String>,
    requested_at_snapshots: &[&str],
    control: &ControlHandle,
) -> Result<ScopeProjection> {
    let db = kio_dir.join("index/sqlite.db");
    // Read-write so the resolver's TEMP identity tables can be created. They
    // live only in this connection's temp schema and never touch the source
    // store.
    let conn = open_existing_source_index_connection(
        &db,
        ExistingSourceIndexOpenMode::ReadWrite,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(|error| KioError::schema(format!("aggregator read {}: {error}", db.display())))?;
    // Projection is a derived-cache operation and must never repair source
    // refs as a side effect. In particular `repair replica` promises to leave
    // HEAD/CAS/source SQLite byte-for-byte under the source owner's control.
    let owned_repository = if repository.is_none() {
        Some(Repository::open_read_only_with_control(
            kio_dir.parent().unwrap_or(kio_dir),
            control.clone(),
        )?)
    } else {
        None
    };
    let repo = repository.unwrap_or_else(|| {
        owned_repository
            .as_ref()
            .expect("an absent projection repository is opened locally")
    });
    let embedding_profiles =
        embedding_store::chunk_embedding_profiles(&conn).map_err(index_to_kio)?;
    // HEAD is the only snapshot authority. A pending publication must be
    // recovered explicitly before a projection can be published.
    let Some(head) = repo.head_commit_hash()? else {
        return Ok(ScopeProjection {
            chunks: Vec::new(),
            images: Vec::new(),
            bindings: Vec::new(),
            current_snapshot_commit: None,
            current_chunking_config_hash: None,
            embedding_profiles,
            completions: Vec::new(),
            max_rowid: 0,
            max_association_rowid: 0,
        });
    };
    let image_policy = current_embedding_policy(repo)?;
    let retained_images = retained_history_instances(repo, &head)?;
    let image_authority =
        image_authority::retained_image_authority(repo, &retained_images, &image_policy)?;

    // A writer captures the source's current append boundaries. Cursor replay
    // never refreshes this replica; it validates its signed generation and uses
    // the already-published binding relation.
    let max_rowid = current_max_rowid(&conn)?;
    let max_association_rowid =
        kio_index::fts::max_chunk_config_association_rowid(&conn).map_err(index_to_kio)?;
    let current_tree_chunking_config_hash = snapshot_chunking_config_hash(repo, &head)?;
    let current_plan = current_history_plan_from_cache(&conn, &head)?;
    let mut completions = vec![kio_index::aggregator::AggProjectionCompletion {
        selector: kio_index::aggregator::AggSelector::Current,
        snapshot_commit: head.clone(),
        chunking_config_hash: Some(current_tree_chunking_config_hash.clone()),
        shallow_skipped: current_plan.shallow_skipped.len() as u64,
    }];
    let mut bindings = projection_bindings_for_plan(
        repo,
        &conn,
        &current_plan,
        "current",
        max_rowid,
        max_association_rowid,
    )?;

    // `--all-history` and `--since` share one visibility relation; the latter
    // applies its timestamp cutoff at query time. `--include-deleted` has a
    // distinct resolver policy, so it receives its own binding set.
    // A writer's current projection tolerates a shallow older ancestor: its
    // current resolver answer remains usable while optional historical
    // snapshots are omitted. Direct readers never perform this refresh;
    // explicit history selectors are rejected during scope preparation.
    match plan_search_history(repo, &head, &TimeSelector::AllHistory, None) {
        Ok(all_history_plan) => {
            let shallow_skipped = all_history_plan.shallow_skipped.len() as u64;
            bindings.extend(projection_bindings_for_plan(
                repo,
                &conn,
                &all_history_plan,
                "all_history",
                max_rowid,
                max_association_rowid,
            )?);
            completions.push(kio_index::aggregator::AggProjectionCompletion {
                selector: kio_index::aggregator::AggSelector::AllHistory,
                snapshot_commit: all_history_plan.snapshot_commit,
                chunking_config_hash: Some(current_tree_chunking_config_hash.clone()),
                shallow_skipped,
            });
        }
        Err(error) if error.error_code() == "KIO-E-COMMIT-SHALLOW-001" => {}
        Err(error) => return Err(error),
    }
    match plan_search_history(repo, &head, &TimeSelector::IncludeDeleted, None) {
        Ok(include_deleted_plan) => {
            let shallow_skipped = include_deleted_plan.shallow_skipped.len() as u64;
            bindings.extend(projection_bindings_for_plan(
                repo,
                &conn,
                &include_deleted_plan,
                "include_deleted",
                max_rowid,
                max_association_rowid,
            )?);
            completions.push(kio_index::aggregator::AggProjectionCompletion {
                selector: kio_index::aggregator::AggSelector::IncludeDeleted,
                snapshot_commit: include_deleted_plan.snapshot_commit,
                chunking_config_hash: Some(current_tree_chunking_config_hash.clone()),
                shallow_skipped,
            });
        }
        Err(error) if error.error_code() == "KIO-E-COMMIT-SHALLOW-001" => {}
        Err(error) => return Err(error),
    }

    // An `--at` lookup is exact to its target commit. Persist an answer for
    // every snapshot reachable from the scope's current HEAD, every commit
    // retained by the source tree-entry cache, and explicit target snapshots a
    // writer just reindexed. The cache set preserves tag-only/orphan side
    // commits when a later HEAD moves to another branch; the explicit set is
    // necessary for an otherwise invisible empty tree. Direct search must not
    // reopen this source index on demand. The scope planner, including its
    // target-ancestor gate and target config resolution, remains the authority
    // for each row.
    let mut at_snapshots = match HistoryReader::new_with_store(repo.kio_dir(), repo.object_store())
        .walk_allow_shallowed(&head)
    {
        Ok((history, _)) => history
            .nodes_in_visit_order()
            .map(|node| node.commit_hash.clone())
            .collect::<BTreeSet<_>>(),
        // Do not turn a current search with a cached shallow snapshot into a
        // replica-refresh failure just to precompute optional `--at` rows.
        // Requested `--at` snapshots remain below and are still resolved
        // exactly by their own planner invocation.
        Err(error) if error.error_code() == "KIO-E-COMMIT-SHALLOW-001" => BTreeSet::new(),
        Err(error) => return Err(error),
    };
    at_snapshots.extend(source_tree_entry_snapshot_commits(&conn)?);
    // The device replica already carries completed `--at` answers that are
    // outside this source's ordinary discovery set (notably valid empty,
    // disconnected snapshots).  A normal writer must retain those answers
    // without paying to re-plan them.  An explicit historical operation is
    // different: it intentionally revalidates and republishes its selected
    // target, even when a prior marker exists.
    at_snapshots.retain(|snapshot_commit| !completed_at_snapshots.contains(snapshot_commit));
    at_snapshots.extend(
        requested_at_snapshots
            .iter()
            .map(|snapshot_commit| (*snapshot_commit).to_owned()),
    );
    for snapshot_commit in at_snapshots {
        let selector = TimeSelector::At(snapshot_commit.clone());
        let plan = match plan_search_history(repo, &snapshot_commit, &selector, None) {
            Ok(plan) => plan,
            // A retained source row can outlive a discarded CAS commit/tree.
            // It cannot form a valid replica marker, and direct `--at` will
            // report the same shallow/not-found condition from CAS.
            Err(error)
                if matches!(
                    error.error_code(),
                    "KIO-E-COMMIT-SHALLOW-001" | "KIO-E-STORE-NOT-FOUND-001"
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let target_config = snapshot_chunking_config_hash(repo, &snapshot_commit)?;
        bindings.extend(projection_bindings_for_plan(
            repo,
            &conn,
            &plan,
            "at",
            max_rowid,
            max_association_rowid,
        )?);
        completions.push(kio_index::aggregator::AggProjectionCompletion {
            selector: kio_index::aggregator::AggSelector::At,
            snapshot_commit: plan.snapshot_commit,
            chunking_config_hash: Some(target_config),
            shallow_skipped: plan.shallow_skipped.len() as u64,
        });
    }

    // A refresh can visit a merge through more than one planner route. The
    // replica table has the same semantic key, so remove exact duplicates
    // before the transaction rather than relying on insertion order.
    bindings.sort_by(|left, right| {
        (
            &left.selector_kind,
            &left.snapshot_commit,
            &left.chunk_id,
            &left.path_at_commit,
            &left.pointer_commit,
        )
            .cmp(&(
                &right.selector_kind,
                &right.snapshot_commit,
                &right.chunk_id,
                &right.path_at_commit,
                &right.pointer_commit,
            ))
    });
    bindings.dedup_by(|left, right| left == right);

    // All committed chunks belong to the one corpus. `invalidated_commit` is
    // the refresh snapshot that first observed a row outside the current
    // resolver answer; exact historical eligibility comes from `bindings`.
    let mut stmt = conn
        .prepare(
            "SELECT c.chunk_id, c.raw_hash, c.tool_profile_hash, c.gen,
                    c.text, c.heading_path, c.section_id, c.byte_start,
                    c.byte_end, c.unit_key, c.created_at, v.embedding
             FROM chunks c
             LEFT JOIN chunk_vec v ON v.chunk_id = c.chunk_id
             WHERE EXISTS (
                 SELECT 1 FROM chunk_publications p
                 WHERE p.chunk_id = c.chunk_id
                   AND EXISTS (
                       SELECT 1 FROM chunk_config_generations cg
                       WHERE cg.chunk_id = c.chunk_id
                         AND cg.chunking_config_hash = p.chunking_config_hash
                   )
             )
               AND c.rowid <= ?1
             ORDER BY c.chunk_id",
        )
        .map_err(|error| KioError::schema(format!("aggregator query: {error}")))?;
    let rows = stmt
        .query_map(rusqlite::params![max_rowid as i64], |row| {
            let blob: Option<Vec<u8>> = row.get(11)?;
            Ok(kio_index::aggregator::AggChunk {
                chunk_id: row.get(0)?,
                raw_hash: row.get(1)?,
                tool_profile_hash: row.get(2)?,
                r#gen: row.get::<_, i64>(3)? as u64,
                text: row.get(4)?,
                heading_path: row.get(5)?,
                section_id: row.get(6)?,
                byte_start: row.get::<_, i64>(7)? as u64,
                byte_end: row.get::<_, i64>(8)? as u64,
                unit_key: row.get(9)?,
                created_at: row.get(10)?,
                invalidated_commit: None,
                embedding: blob
                    .as_deref()
                    .map(kio_index::embedding_store::f32_from_le_bytes),
            })
        })
        .map_err(|error| KioError::schema(format!("aggregator rows: {error}")))?;
    let mut chunks = Vec::new();
    for row in rows {
        chunks.push(row.map_err(|error| KioError::schema(format!("aggregator row: {error}")))?);
    }
    drop(stmt);

    let current_chunk_ids = bindings
        .iter()
        .filter(|binding| {
            binding.selector_kind == "current" && binding.snapshot_commit == head && binding.is_live
        })
        .map(|binding| binding.chunk_id.as_str())
        .collect::<BTreeSet<_>>();
    for chunk in &mut chunks {
        if !current_chunk_ids.contains(chunk.chunk_id.as_str()) {
            chunk.invalidated_commit = Some(head.clone());
        }
    }

    // Image vectors remain attached to every committed citing chunk. At query
    // time the same eligible chunk binding gates the image reference, so a
    // historical image never needs a second visibility rule. The strict
    // source-index schema guarantees that `image_vec` is present.
    let mut image_vectors = BTreeMap::<String, Option<Vec<f32>>>::new();
    let mut seen_references = BTreeSet::<(String, String)>::new();
    let mut images = Vec::new();
    for chunk in &chunks {
        for image in extract_related_images(&chunk.text) {
            let Some(image_id) =
                authorized_local_image_object_hash(scope_id, &image_authority, &image.image_uri)
            else {
                continue;
            };
            // `agg_image_refs` has one reference per image/chunk pair. Keep
            // the first URI deterministically if malformed non-canonical content
            // happens to name the same image twice with different authorities.
            if !seen_references.insert((image_id.clone(), chunk.chunk_id.clone())) {
                continue;
            }
            let vector = if let Some(vector) = image_vectors.get(&image_id) {
                vector.clone()
            } else {
                let vector = kio_index::embedding_store::read_image_vector(&conn, &image_id)
                    .map_err(index_to_kio)?;
                image_vectors.insert(image_id.clone(), vector.clone());
                vector
            };
            if let Some(embedding) = vector {
                images.push(kio_index::aggregator::AggImage {
                    image_id,
                    chunk_id: chunk.chunk_id.clone(),
                    image_uri: image.image_uri,
                    embedding,
                });
            }
        }
    }
    Ok(ScopeProjection {
        chunks,
        images,
        bindings,
        current_snapshot_commit: Some(head),
        current_chunking_config_hash: Some(current_tree_chunking_config_hash),
        embedding_profiles,
        completions,
        max_rowid,
        max_association_rowid,
    })
}

/// Materialize one planner answer into `(selector, snapshot, chunk)` rows.
///
/// The SQL only maps resolved normalized identities to their already-indexed
/// chunk rows and config associations. It never decides which identity is live
/// or which config governs a binding: those facts come from the immutable
/// `SearchHistoryPlan` and each binding's exact commit tree.
fn projection_bindings_for_plan(
    repo: &Repository,
    conn: &Connection,
    plan: &SearchHistoryPlan,
    selector_kind: &str,
    max_rowid: u64,
    max_association_rowid: u64,
) -> Result<Vec<kio_index::aggregator::AggBinding>> {
    let kio_dir = repo.kio_dir();
    // Purge visibility is authoritative over immutable history.  Filter a
    // blocked raw identity before opening its pinned manifest: purge removes
    // that manifest/object closure deliberately, so attempting validation
    // first would turn a successful purge into a replica-refresh failure.
    let mut grouped = BTreeMap::new();
    for (key, bindings) in plan.grouped_bindings() {
        if purge_blocks_rebuild_raw(kio_dir, &key.raw_hash, repo.control())? {
            continue;
        }
        grouped.insert(key, bindings);
    }
    if grouped.is_empty() {
        return Ok(Vec::new());
    }
    let mut binding_configs = BTreeMap::<String, String>::new();
    for binding in grouped.values().flatten() {
        if !binding_configs.contains_key(&binding.pointer_commit) {
            binding_configs.insert(
                binding.pointer_commit.clone(),
                snapshot_chunking_config_hash(repo, &binding.pointer_commit)?,
            );
        }
    }
    install_eligible_identities(conn, plan)?;
    // Validate each immutable manifest once. The SQL below may yield many
    // chunks for one identity, but a retry's Done set is an identity fact.
    let mut done_units = BTreeMap::<SearchContentKey, BTreeSet<(String, String)>>::new();
    for (key, bindings) in &grouped {
        let binding = bindings
            .first()
            .expect("a grouped history identity always has a binding");
        let normalize = NormalizeRef {
            tool_profile_hash: key.tool_profile_hash.clone(),
            r#gen: key.r#gen,
            manifest_hash: key.manifest_hash.clone(),
        };
        match pinned_done_units(repo, &binding.raw_hash, &normalize) {
            Ok(done) => {
                let done = done
                    .into_iter()
                    .map(|unit| {
                        let content_hash = kio_core::cas::hash_bytes(unit.markdown.as_bytes());
                        Ok((unit.unit_key, content_hash))
                    })
                    .collect::<Result<BTreeSet<_>>>()?;
                done_units.insert(key.clone(), done);
            }
            Err(error) => {
                if verify_objects::purge_explains_missing_retained_instance(
                    repo,
                    &binding.raw_hash,
                    &normalize,
                )? {
                    continue;
                }
                return Err(error);
            }
        }
    }
    // Current and `--at` answer a target time point; one ancestry walk serves
    // all chunks/bindings in this plan. Include-deleted's final appearances
    // each need their own pointer ancestry; all-history alone has the exact
    // canonical-introduction equality rule.
    let target_ancestors = if matches!(selector_kind, "current" | "at") {
        Some(at_target_ancestors(repo, &plan.snapshot_commit)?.0)
    } else {
        None
    };
    // All-history aliases keep their own path introduction as the Evidence
    // pointer commit, while one byte-identical chunk can have been published
    // earlier under another path (rename/copy). Walk the snapshot chain once and
    // memoize exact introduction→alias ancestry checks; exact equality alone
    // would drop every later valid alias, while accepting a descendant or an
    // incomparable publication would back-publish content.
    let all_history_graph = if selector_kind == "all_history" {
        Some(
            HistoryReader::new_with_store(repo.kio_dir(), repo.object_store())
                .walk_allow_shallowed(&plan.snapshot_commit)?
                .0,
        )
    } else {
        None
    };
    let mut all_history_ancestry = BTreeMap::<(String, String), bool>::new();
    let mut include_deleted_ranks = BTreeMap::<String, u64>::new();
    if selector_kind == "include_deleted" {
        let (graph, skipped) = HistoryReader::new_with_store(repo.kio_dir(), repo.object_store())
            .walk_allow_shallowed(&plan.snapshot_commit)?;
        let mut parents = graph
            .nodes_in_visit_order()
            .map(|node| (node.commit_hash.clone(), node.commit.parent.clone()))
            .collect::<BTreeMap<_, _>>();
        // Tolerant walks omit shallow tree nodes. Their readable commit object
        // still supplies the parent edge that connects the retained graph.
        for commit_hash in skipped {
            if !parents.contains_key(&commit_hash) {
                parents.insert(commit_hash.clone(), repo.read_commit(&commit_hash)?.parent);
            }
        }
        include_deleted_ranks = publication_ranks(&plan.snapshot_commit, &parents);
    }
    let sql =
        "SELECT c.chunk_id, c.raw_hash, c.tool_profile_hash, c.gen, c.unit_key, c.unit_content_hash,
                eligible.manifest_hash, cg.chunking_config_hash
         FROM chunks c
         JOIN kio_eligible_identity eligible
           ON eligible.raw_hash = c.raw_hash
          AND eligible.tool_profile_hash = c.tool_profile_hash
          AND eligible.gen = c.gen
         JOIN chunk_config_generations cg
           ON cg.chunk_id = c.chunk_id
          AND cg.association_rowid <= ?2
         WHERE c.rowid <= ?1
           AND EXISTS (
               SELECT 1 FROM chunk_publications p
               WHERE p.chunk_id = c.chunk_id
                 AND p.chunking_config_hash = cg.chunking_config_hash
           )
         ORDER BY c.chunk_id, cg.chunking_config_hash";
    let mut stmt = conn
        .prepare(sql)
        .map_err(|error| KioError::schema(format!("aggregator binding query: {error}")))?;
    let rows = stmt
        .query_map(
            rusqlite::params![max_rowid as i64, max_association_rowid as i64],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    SearchContentKey {
                        raw_hash: row.get(1)?,
                        tool_profile_hash: row.get(2)?,
                        r#gen: row.get::<_, i64>(3)? as u64,
                        manifest_hash: row.get(6)?,
                    },
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .map_err(|error| KioError::schema(format!("aggregator binding rows: {error}")))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| KioError::schema(format!("aggregator binding row: {error}")))?;
    drop(stmt);
    // Source publication is append-only and association-scoped. Missing rows
    // are deliberately ineligible; another config's event is never authority.
    let mut publications = BTreeMap::<(String, String), BTreeSet<String>>::new();
    let mut publication_stmt = conn
        .prepare("SELECT chunk_id, chunking_config_hash, introduction_commit FROM chunk_publications ORDER BY chunk_id, chunking_config_hash, introduction_commit")
        .map_err(|error| KioError::schema(error.to_string()))?;
    let publication_rows = publication_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| KioError::schema(error.to_string()))?;
    for row in publication_rows {
        let (chunk_id, config, introduction) =
            row.map_err(|error| KioError::schema(error.to_string()))?;
        publications
            .entry((chunk_id, config))
            .or_default()
            .insert(introduction);
    }
    let mut projected = Vec::new();
    for row in rows {
        let (chunk_id, unit_key, unit_content_hash, key, association_config) = row;
        let Some(resolved) = grouped.get(&key) else {
            continue;
        };
        for binding in resolved {
            if binding_configs.get(&binding.pointer_commit) != Some(&association_config) {
                continue;
            }
            // A chunk is searchable only when its specific unit is Done in
            // this tree binding's immutable manifest, never in mutable
            // `manifest.json` for a same-generation retry.
            if !done_units
                .get(&key)
                .is_some_and(|units| units.contains(&(unit_key.clone(), unit_content_hash.clone())))
            {
                continue;
            }
            let publication_ok = if selector_kind == "include_deleted" {
                publication_is_include_deleted_compatible(
                    publications.get(&(chunk_id.clone(), association_config.clone())),
                    include_deleted_ranks.get(&binding.pointer_commit),
                    &include_deleted_ranks,
                )
            } else if let Some(graph) = all_history_graph.as_ref() {
                publication_is_all_history_compatible(
                    publications.get(&(chunk_id.clone(), association_config.clone())),
                    &binding.pointer_commit,
                    graph,
                    &mut all_history_ancestry,
                )
            } else {
                publication_is_compatible(
                    publications.get(&(chunk_id.clone(), association_config.clone())),
                    target_ancestors.as_ref(),
                )
            };
            if !publication_ok {
                continue;
            }
            projected.push(kio_index::aggregator::AggBinding {
                selector_kind: selector_kind.to_owned(),
                snapshot_commit: plan.snapshot_commit.clone(),
                chunk_id: chunk_id.clone(),
                raw_hash: binding.raw_hash.clone(),
                tool_profile_hash: binding.tool_profile_hash.clone(),
                r#gen: binding.r#gen,
                manifest_hash: binding.manifest_hash.clone(),
                path_at_commit: binding.path_at_commit.clone(),
                pointer_commit: binding.pointer_commit.clone(),
                current_paths: binding.current_paths.clone(),
                is_live: binding.is_live,
            });
        }
    }
    Ok(projected)
}

/// Rank the one snapshot chain newest first. Publication is valid only at the
/// pointed-to commit or an ancestor; there are no merge-side thresholds.
pub(crate) fn publication_ranks(
    snapshot_commit: &str,
    parents: &BTreeMap<String, Option<String>>,
) -> BTreeMap<String, u64> {
    let mut ranks = BTreeMap::new();
    let mut cursor = Some(snapshot_commit);
    while let Some(commit) = cursor {
        if !parents.contains_key(commit) || ranks.contains_key(commit) {
            break;
        }
        ranks.insert(commit.to_owned(), ranks.len() as u64);
        cursor = parents.get(commit).and_then(Option::as_deref);
    }
    ranks
}

pub(crate) fn publication_is_include_deleted_compatible(
    introductions: Option<&BTreeSet<String>>,
    pointer_rank: Option<&u64>,
    thresholds: &BTreeMap<String, u64>,
) -> bool {
    let Some(pointer_rank) = pointer_rank else {
        return false;
    };
    introductions.is_some_and(|introductions| {
        introductions.iter().any(|introduction| {
            thresholds
                .get(introduction)
                .is_some_and(|threshold| pointer_rank <= threshold)
        })
    })
}

/// Publication eligibility over the one preloaded plan map. `None` means the
/// strict source publication contract was not met, so it cannot be projected.
pub(crate) fn publication_is_compatible(
    introductions: Option<&BTreeSet<String>>,
    target_ancestors: Option<&BTreeSet<String>>,
) -> bool {
    let Some(introductions) = introductions else {
        return false;
    };
    if let Some(ancestors) = target_ancestors {
        return introductions
            .iter()
            .any(|commit| ancestors.contains(commit));
    }
    false
}

fn publication_is_all_history_compatible(
    introductions: Option<&BTreeSet<String>>,
    pointer_commit: &str,
    graph: &kio_core::history::LinearHistory,
    memo: &mut BTreeMap<(String, String), bool>,
) -> bool {
    introductions.is_some_and(|introductions| {
        introductions.iter().any(|introduction| {
            let key = (introduction.clone(), pointer_commit.to_owned());
            *memo
                .entry(key)
                .or_insert_with(|| graph.is_ancestor(introduction, pointer_commit))
        })
    })
}

/// Bookkeeping only — `index_generation` decides staleness, so a clock that
/// jumps changes nothing but this timestamp.
pub(crate) fn replica_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// `(scope_id, index_generation)` as this scope's live index reports them right
/// now. `None` when either is unavailable — a scope with no index yet, or one
/// whose `scope.json` cannot be read. Probe callers may treat that as an
/// ineligible optimization; a writer-side projection must turn it into an
/// error so an older Ready header cannot be certified as current.
///
/// The generation is read here rather than passed in so that a write-through
/// always stamps the state the index is actually in, whatever rotations the
/// command ran before reaching this point. Callers hold `.kio/.lock`, so no
/// concurrent writer can rotate between this read and the replica write.
pub(crate) fn replica_scope_stamp(kio_dir: &Path) -> Option<(String, String)> {
    let scope_id = scope_id(kio_dir).ok()?;
    let db = sqlite_path(kio_dir);
    if !db.exists() {
        return None;
    }
    let conn = open_existing_source_index_connection(
        &db,
        ExistingSourceIndexOpenMode::ReadOnly,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .ok()?;
    let generation = kio_index::fts::read_index_metadata(&conn)
        .ok()
        .flatten()?
        .index_generation;
    Some((scope_id, generation))
}

/// Project this scope into the replica now that its index has been replaced
/// (05 §1.8 write-through).
///
/// Readers never repair a scope lazily. Writing here makes the replica track
/// the index because the writer told it, and leaves the header `Rebuilding`
/// (rather than falling back to the source index) if that projection fails.
///
/// A full projection rather than a delta: a rebuild builds a fresh temp db and
/// renames it over `sqlite.db` (P5), so every rowid is new and the live chunk
/// set may have changed arbitrarily.
///
/// Returns the reason it could not, so a caller that must not report success
/// without it (purge — 05 §3.5) can refuse, while every other caller logs and
/// carries on: losing this write leaves the scope marked Rebuilding, so direct
/// search fails closed until a later writer completes the projection. The
/// replica remains a cache and must never make a write fail (03 §4).
pub(crate) fn write_through_projection(
    ctx: &Invocation<'_>,
    kio_dir: &Path,
) -> std::result::Result<(), String> {
    match write_through_projection_full(kio_dir, ctx.control) {
        Ok(()) => Ok(()),
        Err(reason) => {
            // This variant is used by purge, which needs the original error to
            // reject the command.  It still shares the writer-wide fail-closed
            // invariant: a failed source -> replica publication must never
            // leave an older Ready header readable by a direct search.
            mark_replica_unavailable_or_log(ctx, kio_dir);
            Err(reason)
        }
    }
}

/// Full replacement used only by explicit replica repair and purge recovery.
/// Normal writers use the preserving variant below so their routine HEAD
/// refresh cannot discard completed disconnected `--at` projections.
pub(crate) fn write_through_projection_full(
    kio_dir: &Path,
    control: &kio_core::control::ControlHandle,
) -> std::result::Result<(), String> {
    write_through_projection_inner(kio_dir, None, &[], false, control)
}

/// Writer-side variant for an operation that explicitly resolved one or more
/// historical `--at` snapshots. Ordinary writers pass no extra snapshots: all
/// discoverable history still comes from HEAD reachability or source cache
/// rows. An explicitly selected empty tree has neither, so its writer must
/// carry that fact through to the completed replica projection.
pub(crate) fn write_through_projection_with_requested_at(
    kio_dir: &Path,
    requested_at_snapshots: &[&str],
    control: &kio_core::control::ControlHandle,
) -> std::result::Result<(), String> {
    write_through_projection_inner(kio_dir, None, requested_at_snapshots, true, control)
}

fn write_through_projection_inner(
    kio_dir: &Path,
    repository: Option<&Repository>,
    requested_at_snapshots: &[&str],
    preserve_completed_at: bool,
    control: &ControlHandle,
) -> std::result::Result<(), String> {
    let Some((scope_id, generation)) = replica_scope_stamp(kio_dir) else {
        // A caller reaches this after changing (or attempting to publish) the
        // source index. Treat a missing/unreadable stamp as a failed
        // projection, not as a harmless no-op: the wrapper marks any existing
        // Ready replica header Rebuilding so direct search cannot certify its
        // old rows as the new source state.
        return Err(format!(
            "write-through source index stamp is unavailable for {}",
            kio_dir.display()
        ));
    };
    // In preserve mode the existing completed `--at` set is read before the
    // source collection. This makes candidate selection incremental and lets
    // the refresh transaction retain the already-projected rows atomically.
    let mut replica =
        kio_index::aggregator::Aggregator::open_with_control(&aggregator_path(control), control)
            .map_err(|error| format!("write-through open failed: {error}"))?;
    let completed_at_snapshots = if preserve_completed_at {
        replica
            .completed_projection_snapshots(&scope_id, kio_index::aggregator::AggSelector::At)
            .map_err(|error| format!("write-through completed At read {scope_id} failed: {error}"))?
            .into_iter()
            .collect()
    } else {
        BTreeSet::new()
    };
    let projection = collect_scope_projection(
        kio_dir,
        repository,
        &scope_id,
        &completed_at_snapshots,
        requested_at_snapshots,
        control,
    )
    .map_err(|error| format!("write-through read {scope_id} failed: {error}"))?;
    let header = kio_index::aggregator::AggScopeHeader {
        current_snapshot_commit: projection.current_snapshot_commit,
        current_chunking_config_hash: projection.current_chunking_config_hash,
        index_generation: generation,
        max_rowid: projection.max_rowid,
        max_association_rowid: projection.max_association_rowid,
        embedding_profiles: projection.embedding_profiles,
        index_status: kio_index::aggregator::AggIndexStatus::Ready,
    };
    // Keep the projection-failure contract testable without weakening the
    // replica's strict schema gate through a SQLite trigger fixture.
    if aggregator_projection_refresh_fault_enabled(control) {
        return Err("injected aggregator projection refresh failure".to_owned());
    }
    let request = kio_index::aggregator::AggProjectionRequest {
        scope_id: &scope_id,
        header: &header,
        chunks: &projection.chunks,
        images: &projection.images,
        bindings: &projection.bindings,
        completions: &projection.completions,
        now_ms: replica_now_ms(),
    };
    if preserve_completed_at {
        replica
            .refresh_scope_with_projection_preserving_at(request)
            .map_err(|error| {
                format!("write-through incremental At refresh {scope_id} failed: {error}")
            })?
    } else {
        replica
            .refresh_scope_with_projection(request)
            .map_err(|error| format!("write-through refresh {scope_id} failed: {error}"))?
    }
    control
        .checkpoint(kio_core::durability::DurabilityPoint::ReplicaPublished)
        .map_err(|error| format!("write-through durable checkpoint failed: {error}"))
}

/// [`write_through_projection`] for the callers that treat a lost cache write
/// as survivable — every one except purge.
pub(crate) fn write_through_projection_or_log(ctx: &Invocation<'_>, kio_dir: &Path) {
    write_through_projection_or_log_with_requested_at(ctx, kio_dir, &[]);
}

pub(crate) fn write_through_projection_or_log_for_repo(ctx: &Invocation<'_>, repo: &Repository) {
    if let Err(reason) =
        write_through_projection_inner(repo.kio_dir(), Some(repo), &[], true, ctx.control)
    {
        log_aggregator_degraded(ctx, &reason);
        mark_replica_unavailable_or_log(ctx, repo.kio_dir());
    }
}

fn write_through_projection_or_log_with_requested_at(
    ctx: &Invocation<'_>,
    kio_dir: &Path,
    requested_at_snapshots: &[&str],
) {
    if let Err(reason) =
        write_through_projection_with_requested_at(kio_dir, requested_at_snapshots, ctx.control)
    {
        log_aggregator_degraded(ctx, &reason);
        // A direct reader deliberately has no source-index fallback.  Once a
        // writer has changed its source state, leaving the last Ready header in
        // place would certify stale corpus rows indefinitely.  Retain the rows
        // for diagnosis/recovery, but make the scope fail closed until a later
        // writer publishes a complete projection.
        mark_replica_unavailable_or_log(ctx, kio_dir);
    }
}

/// Publish the explicitly selected historical snapshot as part of a
/// writer-side reindex. In particular, this preserves a completion marker for
/// a valid empty disconnected tree without inventing a source-cache row.
pub(crate) fn write_through_projection_or_log_for_at_snapshot(
    ctx: &Invocation<'_>,
    kio_dir: &Path,
    snapshot_commit: &str,
) {
    write_through_projection_or_log_with_requested_at(ctx, kio_dir, &[snapshot_commit]);
}

/// A repaired commit preserves its parent's tree, but it has a distinct commit
/// identity. Populate the derived source cache for that new identity before a
/// full replica projection asks `current_history_plan_from_cache` for it.
///
/// This runs only after `verify-objects` has durably recovered bytes and
/// advanced HEAD. It is deliberately separate from manual `snapshot`: that
/// path may introduce unindexed content and must remain `Rebuilding` until a
/// real index pass replaces sqlite.db.
pub(crate) fn materialize_repaired_snapshot_entries(
    repo: &Repository,
    repaired_head: &str,
) -> Result<()> {
    let db = sqlite_path(repo.kio_dir());
    if !db.is_file() {
        return Err(KioError::schema(format!(
            "repair projection source index is unavailable: {}",
            db.display()
        )));
    }
    let conn = open_existing_source_index_connection(
        &db,
        ExistingSourceIndexOpenMode::ReadWrite,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(|error| {
        KioError::schema(format!("repair projection open {}: {error}", db.display()))
    })?;
    let _ = ensure_snapshot_tree_entries(repo, &conn, repaired_head)?;
    Ok(())
}

/// Prevent strict replica-only reads from serving a projection whose source
/// HEAD advanced before its next index rebuild. This is cache-only state: a
/// cache write failure remains non-fatal and the missing/stale header will make
/// the reader fail closed.
pub(crate) fn mark_replica_rebuilding_or_log(
    ctx: &Invocation<'_>,
    kio_dir: &Path,
    snapshot_commit: &str,
) {
    let Ok(scope_id) = scope_id(kio_dir) else {
        return;
    };
    let mut replica = match kio_index::aggregator::Aggregator::open_with_control(
        &aggregator_path(ctx.control),
        ctx.control,
    ) {
        Ok(replica) => replica,
        Err(error) => {
            log_aggregator_degraded(ctx, &format!("rebuilding header open failed: {error}"));
            return;
        }
    };
    let header = match replica.scope_header(&scope_id) {
        Ok(Some(mut header)) => {
            header.current_snapshot_commit = Some(snapshot_commit.to_owned());
            header.index_status = kio_index::aggregator::AggIndexStatus::Rebuilding;
            header
        }
        Ok(None) => return,
        Err(error) => {
            log_aggregator_degraded(
                ctx,
                &format!("rebuilding header read {scope_id} failed: {error}"),
            );
            return;
        }
    };
    if let Err(error) = replica.update_scope_header(&scope_id, &header, replica_now_ms()) {
        log_aggregator_degraded(
            ctx,
            &format!("rebuilding header {scope_id} failed: {error}"),
        );
    }
}

/// Test-only fault seams around the durability boundary immediately after HEAD
/// advances. `*_before_marker` proves readers reject a stale Ready header in
/// the narrow cross-store interval; the phase name without that suffix proves
/// the marker itself remains fail-closed before the source rebuild begins.
pub(crate) fn maybe_inject_replica_after_head_fault(
    phase: &str,
    control: &ControlHandle,
) -> Result<()> {
    if replica_after_head_fault_enabled(phase, control) {
        return Err(KioError::new(
            "KIO-E-REPLICA-AFTER-HEAD-FAULT-001",
            "injected replica post-HEAD publication fault",
            json!({ "phase": phase }),
            ExitCode::Failure,
        ));
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn aggregator_projection_refresh_fault_enabled(control: &ControlHandle) -> bool {
    matches!(
        control.debug().cli.aggregator_projection_fault.known(),
        Some(AggregatorProjectionFault::Refresh)
    )
}

#[cfg(not(debug_assertions))]
fn aggregator_projection_refresh_fault_enabled(_control: &ControlHandle) -> bool {
    false
}

#[cfg(debug_assertions)]
fn replica_after_head_fault_enabled(phase: &str, control: &ControlHandle) -> bool {
    let expected = match phase {
        "index_before_marker" => ReplicaFault::IndexBeforeMarker,
        "index" => ReplicaFault::Index,
        "reindex_before_marker" => ReplicaFault::ReindexBeforeMarker,
        "reindex" => ReplicaFault::Reindex,
        _ => return false,
    };
    control.debug().cli.replica_after_head_fault.known() == Some(&expected)
}

#[cfg(not(debug_assertions))]
fn replica_after_head_fault_enabled(_: &str, _control: &ControlHandle) -> bool {
    false
}

/// Fail closed without guessing a replacement snapshot.  This is used after a
/// write-through failure, when the source may already have advanced but the
/// replica could not observe enough of it to state an exact commit safely.
pub(crate) fn mark_replica_unavailable_or_log(ctx: &Invocation<'_>, kio_dir: &Path) {
    let Ok(scope_id) = scope_id(kio_dir) else {
        return;
    };
    let mut replica = match kio_index::aggregator::Aggregator::open_with_control(
        &aggregator_path(ctx.control),
        ctx.control,
    ) {
        Ok(replica) => replica,
        Err(error) => {
            log_aggregator_degraded(ctx, &format!("unavailable header open failed: {error}"));
            return;
        }
    };
    let header = match replica.scope_header(&scope_id) {
        Ok(Some(mut header)) => {
            header.index_status = kio_index::aggregator::AggIndexStatus::Rebuilding;
            header
        }
        Ok(None) => return,
        Err(error) => {
            log_aggregator_degraded(
                ctx,
                &format!("unavailable header read {scope_id} failed: {error}"),
            );
            return;
        }
    };
    if let Err(error) = replica.update_scope_header(&scope_id, &header, replica_now_ms()) {
        log_aggregator_degraded(
            ctx,
            &format!("unavailable header {scope_id} failed: {error}"),
        );
    }
}

/// The exact Ready projection that existed immediately before a manual
/// snapshot.  A manual snapshot normally has to leave it Rebuilding: it can
/// publish new raw bytes without a matching source-index update.  This saved
/// state is only used for the narrower no-content-change proof below.
#[derive(Debug, Clone)]
pub(crate) struct ReadyReplicaSnapshot {
    scope_id: String,
    snapshot_commit: String,
    header: kio_index::aggregator::AggScopeHeader,
}

/// Capture a reusable projection before a manual snapshot moves HEAD.  A
/// missing/corrupt cache is deliberately just ineligible for reuse: the
/// snapshot itself must remain a successful source write and will mark any
/// existing cache fail-closed afterward.
pub(crate) fn capture_ready_replica_snapshot(repo: &Repository) -> Option<ReadyReplicaSnapshot> {
    let snapshot_commit = repo.head_commit_hash().ok().flatten()?;
    let scope_id = scope_id(repo.kio_dir()).ok()?;
    let replica = kio_index::aggregator::Aggregator::open_with_control(
        &aggregator_path(repo.control()),
        repo.control(),
    )
    .ok()?;
    let header = replica.scope_header(&scope_id).ok().flatten()?;
    if !matches!(
        header.index_status,
        kio_index::aggregator::AggIndexStatus::Ready
    ) || header.current_snapshot_commit.as_deref() != Some(snapshot_commit.as_str())
    {
        return None;
    }
    Some(ReadyReplicaSnapshot {
        scope_id,
        snapshot_commit,
        header,
    })
}

/// A manual snapshot commonly produces a new tree object solely because it
/// omits normalization references that the preceding `kio index` auto
/// snapshot recorded.  It is safe to reuse the ready projection only when
/// every path/raw/type is unchanged and a target reference is either absent
/// (the normal manual form) or byte-for-byte the prior reference.  Any new or
/// changed normalization identity remains a normal unindexed snapshot and
/// therefore stays Rebuilding.
fn manual_snapshot_tree_matches_ready_projection(prior: &TreeObject, target: &TreeObject) -> bool {
    prior.entries.len() == target.entries.len()
        && prior
            .entries
            .iter()
            .zip(&target.entries)
            .all(|(prior, target)| {
                prior.path == target.path
                    && prior.entry_type == target.entry_type
                    && prior.raw_hash == target.raw_hash
                    && (target.normalize.is_none() || target.normalize == prior.normalize)
            })
}

/// Read the exact current-tree relation a Ready source projection uses.  This
/// is intentionally a writer-only helper: direct search must never open the
/// per-scope index to obtain or repair candidates.
fn source_tree_entry_projections(
    conn: &Connection,
    commit_hash: &str,
) -> Result<Vec<TreeEntryProjection>> {
    let mut statement = conn
        .prepare(
            "SELECT path, raw_hash, tool_profile_hash, gen, manifest_hash
             FROM tree_entries
             WHERE commit_hash = ?1
             ORDER BY path",
        )
        .map_err(|error| KioError::schema(error.to_string()))?;
    let rows = statement
        .query_map(rusqlite::params![commit_hash], |row| {
            Ok(TreeEntryProjection {
                path: row.get(0)?,
                raw_hash: row.get(1)?,
                tool_profile_hash: row.get(2)?,
                r#gen: row.get(3)?,
                manifest_hash: row.get(4)?,
            })
        })
        .map_err(|error| KioError::schema(error.to_string()))?;
    let mut projections = Vec::new();
    for row in rows {
        projections.push(row.map_err(|error| KioError::schema(error.to_string()))?);
    }
    Ok(projections)
}

/// The part of a tree whose normalization identity is immutable in CAS.  A
/// Ready predecessor is reusable only when its source cache says exactly this
/// relation; a fallback row inferred from mutable latest-normalize state
/// is deliberately not promoted through a new snapshot.
fn normalized_tree_entry_projections(tree: &TreeObject) -> Vec<TreeEntryProjection> {
    tree.entries
        .iter()
        .filter_map(|entry| {
            entry
                .normalize
                .as_ref()
                .map(|normalize| TreeEntryProjection {
                    path: entry.path.clone(),
                    raw_hash: entry.raw_hash.clone(),
                    tool_profile_hash: Some(normalize.tool_profile_hash.clone()),
                    r#gen: Some(normalize.r#gen),
                    manifest_hash: Some(normalize.manifest_hash.clone()),
                })
        })
        .collect()
}

/// Reuse a just-prior Ready projection after a semantically unchanged manual
/// snapshot.  This is a writer-side materialization of `tree_entries`, followed
/// by the normal full replica write-through; it never creates a reader-side
/// source-index fallback.
///
/// Returning `false` is an expected, safe rejection of the fast path.  An
/// actual I/O/SQLite/CAS failure returns `Err` so the caller can log it while
/// retaining the Rebuilding marker already written before this function runs.
pub(crate) fn republish_equivalent_manual_snapshot(
    repo: &Repository,
    before: &ReadyReplicaSnapshot,
    target_snapshot: &str,
) -> std::result::Result<bool, String> {
    let head = repo
        .head_commit_hash()
        .map_err(|error| format!("read HEAD: {error}"))?;
    if head.as_deref() != Some(target_snapshot) {
        return Ok(false);
    }
    let Some((scope_id, generation)) = replica_scope_stamp(repo.kio_dir()) else {
        return Ok(false);
    };
    if scope_id != before.scope_id || generation != before.header.index_generation {
        return Ok(false);
    }
    let target_config = snapshot_chunking_config_hash(repo, target_snapshot)
        .map_err(|error| format!("read target tree chunking config: {error}"))?;
    if before.header.current_chunking_config_hash.as_deref() != Some(target_config.as_str()) {
        return Ok(false);
    }

    // Confirm the earlier mark actually took the scope out of service before
    // touching source cache rows.  A competing writer or failed marker write
    // must leave this optimization disabled rather than silently re-certify a
    // projection we no longer own.
    let replica = kio_index::aggregator::Aggregator::open_with_control(
        &aggregator_path(repo.control()),
        repo.control(),
    )
    .map_err(|error| format!("open replica after snapshot: {error}"))?;
    let marked = replica
        .scope_header(&before.scope_id)
        .map_err(|error| format!("read replica after snapshot: {error}"))?;
    let Some(marked) = marked else {
        return Ok(false);
    };
    if !matches!(
        marked.index_status,
        kio_index::aggregator::AggIndexStatus::Rebuilding
    ) || marked.current_snapshot_commit.as_deref() != Some(target_snapshot)
    {
        return Ok(false);
    }
    drop(replica);

    let target_commit = repo
        .read_commit(target_snapshot)
        .map_err(|error| format!("read target commit: {error}"))?;
    if target_commit.commit_type != CommitType::Manual
        || target_commit.parent.as_deref() != Some(before.snapshot_commit.as_str())
    {
        return Ok(false);
    }
    let prior_commit = repo
        .read_commit(&before.snapshot_commit)
        .map_err(|error| format!("read prior commit: {error}"))?;
    let prior_tree = repo
        .read_tree(&prior_commit.tree)
        .map_err(|error| format!("read prior tree: {error}"))?;
    let target_tree = repo
        .read_tree(&target_commit.tree)
        .map_err(|error| format!("read target tree: {error}"))?;
    if !manual_snapshot_tree_matches_ready_projection(&prior_tree, &target_tree) {
        return Ok(false);
    }

    let db = sqlite_path(repo.kio_dir());
    let mut conn = open_existing_source_index_connection(
        &db,
        ExistingSourceIndexOpenMode::ReadWrite,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(|error| format!("open source index {}: {error}", db.display()))?;
    let max_rowid =
        current_max_rowid(&conn).map_err(|error| format!("read source chunk bound: {error}"))?;
    let max_association_rowid = kio_index::fts::max_chunk_config_association_rowid(&conn)
        .map_err(|error| format!("read source association bound: {error}"))?;
    if before.header.max_rowid != max_rowid
        || before.header.max_association_rowid != max_association_rowid
    {
        return Ok(false);
    }

    let expected = normalized_tree_entry_projections(&prior_tree);
    let source_rows = source_tree_entry_projections(&conn, &before.snapshot_commit)
        .map_err(|error| format!("read prior source tree entries: {error}"))?;
    if source_rows != expected {
        return Ok(false);
    }
    if !source_tree_entry_projections(&conn, target_snapshot)
        .map_err(|error| format!("read target source tree entries: {error}"))?
        .is_empty()
    {
        return Ok(false);
    }

    let tx = conn
        .transaction()
        .map_err(|error| format!("begin target tree materialization: {error}"))?;
    let copied = tx
        .execute(
            "INSERT INTO tree_entries(commit_hash, path, raw_hash, tool_profile_hash, gen, manifest_hash)
             SELECT ?1, path, raw_hash, tool_profile_hash, gen, manifest_hash
             FROM tree_entries
             WHERE commit_hash = ?2",
            rusqlite::params![target_snapshot, before.snapshot_commit],
        )
        .map_err(|error| format!("materialize target tree entries: {error}"))?;
    if copied != expected.len() {
        return Err(format!(
            "target tree materialization copied {copied} rows; expected {}",
            expected.len()
        ));
    }
    tx.commit()
        .map_err(|error| format!("commit target tree materialization: {error}"))?;

    // This is a normal writer publication, not replica repair. Preserve any
    // completed disconnected `--at` projections while publishing the reused
    // current-tree cache.
    write_through_projection_with_requested_at(repo.kio_dir(), &[], repo.control()).map(|()| true)
}

/// Publish an in-place index change: rotate the scope's `index_generation`,
/// then tell the replica what changed (05 §1.8 write-through, LC25 rotation).
///
/// This is the path for writers that mutate `sqlite.db` directly instead of
/// rebuilding it — the two embedding lanes. They may apply their known vector
/// delta as an internal fast path, but that delta is never a complete replica
/// publication: the function always follows it with a full projection carrying
/// the current header and selector markers.
///
/// Both halves belong to one function because doing either without the other is
/// a defect that has now been found twice:
///
/// - Rotating without completing the projection retires cursors and leaves the
///   scope Rebuilding, so direct search correctly fails closed.
/// - Replicating without rotating (R25-4) leaves no durable change marker for
///   cursor replay or a subsequent writer to validate. A rebuild path repairs
///   this by publishing a fresh generation together with a complete projection.
///
/// `before` is the generation the index carried when the delta was computed. If
/// the replica is not exactly there, the delta is not trusted; either result
/// still reaches the same full writer-side projection.
pub(crate) fn publish_in_place_delta(
    ctx: &Invocation<'_>,
    kio_dir: &Path,
    before: Option<&str>,
    delta: &kio_index::aggregator::ScopeDelta,
) -> Result<()> {
    if delta.is_empty() {
        return Ok(());
    }
    // Rotate FIRST, so the stamp the replica is about to receive is the one the
    // index now carries rather than one this function is about to invalidate.
    rotate_index_generation_unconditionally(kio_dir, ctx.control)?;
    // The source vector links have already changed.  Do not leave an old Ready
    // header observable while the writer finishes its full projection.
    mark_replica_unavailable_or_log(ctx, kio_dir);
    let (Some((scope_id, generation)), Some(before)) = (replica_scope_stamp(kio_dir), before)
    else {
        return Ok(());
    };
    let mut replica = match kio_index::aggregator::Aggregator::open_with_control(
        &aggregator_path(ctx.control),
        ctx.control,
    ) {
        Ok(replica) => replica,
        Err(error) => {
            log_aggregator_degraded(ctx, &format!("write-through open failed: {error}"));
            return Ok(());
        }
    };
    match replica.apply_delta(&scope_id, before, &generation, delta, replica_now_ms()) {
        Ok(true) | Ok(false) => {
            // `ScopeDelta` only carries chunk vectors. Its fast path cannot
            // certify the current HEAD/config/embedding-profile header or
            // selector markers, and a direct search may no longer consult the
            // source sqlite database to fill those gaps. Publish the complete
            // projection before returning to a reader.
            drop(replica);
            write_through_projection_or_log(ctx, kio_dir);
        }
        Err(error) => {
            log_aggregator_degraded(
                ctx,
                &format!("write-through delta {scope_id} failed: {error}"),
            );
        }
    }
    Ok(())
}

/// One stderr line for a non-fatal replica write-through failure.
pub(crate) fn log_aggregator_degraded(ctx: &Invocation<'_>, detail: &str) {
    ctx.interaction.diagnostic(&format!(
        "kio: aggregator write-through degraded ({detail})"
    ));
}
