//! Context owners authenticated against exact retained manifests and chunk CAS.
//!
//! SQLite supplies current-config eligibility, never text or alias authority.
//! Supplied retained instances narrow the selected scope; they cannot add a
//! path to the immutable graph's exact manifest binding.

use super::*;
use std::sync::Arc;

type InstanceKey = (String, String, u64, String);

#[derive(Debug, Clone)]
pub(super) struct RetainedEmbeddingChunk {
    pub(super) chunk_id: String,
    pub(super) text: Arc<str>,
    pub(super) text_hash: String,
    pub(super) raw_path: String,
    pub(super) requires_secret_approval: bool,
    pub(super) is_head_owner: bool,
}

// Aligned with the durable TaskStore's MAX_TASK_RECORDS. The byte budget
// accounts string payload, not allocator capacity, Arc headers, or tree nodes.
const MAX_EMBEDDING_CANDIDATE_PAIRS: usize = 100_000;
const MAX_EMBEDDING_CANDIDATE_PAYLOAD_BYTES: usize = 128 * 1024 * 1024;

struct CandidateChunk {
    text: Arc<str>,
    paths: BTreeMap<String, RetainedEmbeddingChunk>,
}

struct CandidateAccumulator {
    chunks: BTreeMap<String, CandidateChunk>,
    pairs: usize,
    payload_bytes: usize,
    pair_limit: usize,
    payload_limit: usize,
}

fn checked_candidate_total(
    dimension: &str,
    current: usize,
    additions: &[usize],
    limit: usize,
) -> Result<usize> {
    let count = additions
        .iter()
        .try_fold(current, |sum, value| sum.checked_add(*value));
    match count {
        Some(count) if count <= limit => Ok(count),
        _ => Err(KioError::new(
            "KIO-E-EMBED-PLAN-LIMIT-001",
            "retained embedding candidate plan exceeds its resource limit",
            serde_json::json!({
                "dimension": dimension,
                "count": count.map_or_else(|| serde_json::json!("overflow"), |n| serde_json::json!(n)),
                "limit": limit,
            }),
            kio_core::ExitCode::PermanentFailure,
        )),
    }
}

impl CandidateAccumulator {
    fn new(pair_limit: usize, payload_limit: usize) -> Self {
        Self {
            chunks: BTreeMap::new(),
            pairs: 0,
            payload_bytes: 0,
            pair_limit,
            payload_limit,
        }
    }

    fn admit(
        &mut self,
        chunk_id: &str,
        canonical: &ChunkObject,
        path: &str,
        requires_secret_approval: bool,
        is_head_owner: bool,
    ) -> Result<()> {
        if let Some(candidate) = self
            .chunks
            .get_mut(chunk_id)
            .and_then(|chunk| chunk.paths.get_mut(path))
        {
            candidate.is_head_owner |= is_head_owner;
            return Ok(());
        }
        let pairs = checked_candidate_total("candidate_pairs", self.pairs, &[1], self.pair_limit)?;
        let existing = self.chunks.get(chunk_id);
        // Only an eligible, nonduplicate pair can trigger projection. This
        // staging body comes from authenticated CAS, bounded by MAX_CHUNK_OBJECT_BYTES.
        let projected = existing
            .is_none()
            .then(|| kio_index::project_search_text(&canonical.text));
        // Each pair owns chunk_id, text_hash, raw_path and the path map key.
        // The outer chunk key and shared projection are charged once per chunk.
        let payload_bytes = checked_candidate_total(
            "payload_bytes",
            self.payload_bytes,
            &[
                chunk_id.len(),
                canonical.text_hash.len(),
                path.len(),
                path.len(),
                if existing.is_none() {
                    chunk_id.len()
                } else {
                    0
                },
                projected.as_ref().map_or(0, String::len),
            ],
            self.payload_limit,
        )?;
        // Admission checks precede all candidate identity clones and insertion.
        let chunk = self
            .chunks
            .entry(chunk_id.to_owned())
            .or_insert_with(|| CandidateChunk {
                text: Arc::from(projected.expect("new chunk has a projected body")),
                paths: BTreeMap::new(),
            });
        chunk.paths.insert(
            path.to_owned(),
            RetainedEmbeddingChunk {
                chunk_id: chunk_id.to_owned(),
                text: Arc::clone(&chunk.text),
                text_hash: canonical.text_hash.clone(),
                raw_path: path.to_owned(),
                requires_secret_approval,
                is_head_owner,
            },
        );
        self.pairs = pairs;
        self.payload_bytes = payload_bytes;
        Ok(())
    }

    fn into_candidates(self) -> Vec<RetainedEmbeddingChunk> {
        self.chunks
            .into_values()
            .flat_map(|chunk| chunk.paths.into_values())
            .collect()
    }
}

struct OwnerAuthority {
    paths: BTreeSet<String>,
    head_paths: BTreeSet<String>,
    units: AuthenticatedNormalizedUnits,
}

#[derive(Default)]
struct OwnerBindings {
    paths: BTreeMap<InstanceKey, BTreeSet<String>>,
    head_paths: BTreeMap<InstanceKey, BTreeSet<String>>,
    secret_raws: BTreeSet<String>,
}

impl OwnerBindings {
    fn record(&mut self, entry: &kio_core::dag::TreeEntry, is_head: bool) {
        // Raw-wide taint includes aliases without a normalize ref, ignored
        // aliases, and aliases outside the caller's selected manifest set.
        if classify_secret(&entry.path).is_some() {
            self.secret_raws.insert(entry.raw_hash.clone());
        }
        if let Some(normalize) = &entry.normalize {
            let key = instance_key(&entry.raw_hash, normalize);
            self.paths
                .entry(key.clone())
                .or_default()
                .insert(entry.path.clone());
            if is_head {
                self.head_paths
                    .entry(key)
                    .or_default()
                    .insert(entry.path.clone());
            }
        }
    }
}

fn instance_key(raw: &str, normalize: &NormalizeRef) -> InstanceKey {
    (
        raw.to_owned(),
        normalize.tool_profile_hash.clone(),
        normalize.r#gen,
        normalize.manifest_hash.clone(),
    )
}

fn exact_unit_authorities(
    repo: &Repository,
    raw: &str,
    normalize: &NormalizeRef,
) -> Result<AuthenticatedNormalizedUnits> {
    let mut authority = AuthenticatedNormalizedUnits::new();
    for unit in pinned_done_units(repo, raw, normalize)? {
        authority.insert(
            (
                raw.to_owned(),
                normalize.tool_profile_hash.clone(),
                normalize.r#gen,
                unit.unit_key,
                hash_bytes(unit.markdown.as_bytes()),
            ),
            AuthenticatedNormalizedUnit {
                markdown: unit.markdown,
                introductions: BTreeSet::new(),
            },
        );
    }
    Ok(authority)
}

fn chunk_body(row: &ChunkRow) -> ChunkObject {
    ChunkObject {
        spec_version: 1,
        raw_hash: row.raw_hash.clone(),
        tool_profile_hash: row.tool_profile_hash.clone(),
        r#gen: row.r#gen,
        unit_key: row.unit_key.clone(),
        unit_content_hash: row.unit_content_hash.clone(),
        heading_path: row.heading_path.clone().unwrap_or_default(),
        section_id: row.section_id.clone().filter(|value| !value.is_empty()),
        byte_start: row.byte_start,
        byte_end: row.byte_end,
        text_hash: row.text_hash.clone(),
        text: row.text.clone(),
    }
}

// SQLite text is a normalized search projection. Compare only its immutable
// identity metadata; embedding input is projected only from verified CAS text.
fn sql_identity_matches(sql: &ChunkObject, canonical: &ChunkObject) -> bool {
    sql.spec_version == canonical.spec_version
        && sql.raw_hash == canonical.raw_hash
        && sql.tool_profile_hash == canonical.tool_profile_hash
        && sql.r#gen == canonical.r#gen
        && sql.unit_key == canonical.unit_key
        && sql.unit_content_hash == canonical.unit_content_hash
        && sql.heading_path == canonical.heading_path
        && sql.section_id == canonical.section_id
        && sql.byte_start == canonical.byte_start
        && sql.byte_end == canonical.byte_end
        && sql.text_hash == canonical.text_hash
}

fn selected_paths(
    exact_paths: &BTreeSet<String>,
    requested_paths: &BTreeSet<String>,
) -> BTreeSet<String> {
    exact_paths.intersection(requested_paths).cloned().collect()
}

const ELIGIBLE_CHUNKS_SQL: &str =
    "SELECT c.chunk_id, c.raw_hash, c.tool_profile_hash, c.gen, c.unit_key,
            c.unit_content_hash, c.heading_path, c.section_id, c.byte_start,
            c.byte_end, c.text_hash, p.introduction_commit, p.chunking_config_hash
     FROM chunks c JOIN chunk_publications p ON p.chunk_id = c.chunk_id
     WHERE (?1 IS NULL OR p.chunking_config_hash = ?1) AND EXISTS (
         SELECT 1 FROM chunk_config_generations cg
         WHERE cg.chunk_id = c.chunk_id AND cg.chunking_config_hash = p.chunking_config_hash)
     ORDER BY c.chunk_id, p.chunking_config_hash, p.introduction_commit";

/// Callers may pass an authenticated rebuild's connection, but this collector
/// independently checks its current-config publication against the durable
/// ledger and immutable introduction. A SQLite-only association cannot admit
/// work even when its chunk happens to exist in CAS. `Some(config)` narrows
/// execution; `None` retains all authenticated configurations for CAS replay
/// and projection. Configuration eligibility never expands owner aliases.
pub(super) fn collect_retained_embedding_chunks(
    repo: &Repository,
    conn: &Connection,
    retained_instances: &[RetainedNormalizedInstance],
    chunking_config_hash: Option<&str>,
    current_policy: Option<&CurrentPolicyEvaluator>,
) -> Result<Vec<RetainedEmbeddingChunk>> {
    let Some(head) = repo.head_commit_hash()? else {
        return Ok(Vec::new());
    };
    let roots = durable_history_roots(repo)?;
    let (graph, _) = HistoryReader::new(repo.kio_dir()).walk_for_roots_allow_shallowed(&roots)?;
    let mut bindings = OwnerBindings::default();
    for node in graph.nodes_in_visit_order() {
        for entry in &node.tree.entries {
            bindings.record(entry, node.commit_hash == head);
        }
    }
    let mut blocked = BTreeMap::new();
    let mut owners = BTreeMap::<InstanceKey, OwnerAuthority>::new();
    for instance in retained_instances {
        let raw_blocked = match blocked.get(&instance.raw_hash) {
            Some(value) => *value,
            None => {
                let value = purge_blocks_rebuild_raw(repo.kio_dir(), &instance.raw_hash)?;
                blocked.insert(instance.raw_hash.clone(), value);
                value
            }
        };
        if raw_blocked {
            continue;
        }
        let key = instance_key(&instance.raw_hash, &instance.normalize);
        let exact_paths = bindings.paths.get(&key).ok_or_else(|| {
            KioError::schema("embedding owner instance is not an exact retained manifest binding")
        })?;
        let paths = selected_paths(exact_paths, &instance.policy_paths);
        if paths.is_empty() {
            continue;
        }
        if let Some(owner) = owners.get_mut(&key) {
            owner.paths.extend(paths);
        } else {
            let units = match exact_unit_authorities(repo, &instance.raw_hash, &instance.normalize)
            {
                Ok(units) => units,
                Err(error) => {
                    if crate::verify_objects::purge_explains_missing_retained_instance(
                        repo,
                        &instance.raw_hash,
                        &instance.normalize,
                    )? {
                        // Only this exact old owner's removed closure is
                        // explained. Raw-wide alias taint was already gathered
                        // from every retained tree and must remain in force.
                        continue;
                    }
                    return Err(error);
                }
            };
            owners.insert(
                key.clone(),
                OwnerAuthority {
                    paths,
                    head_paths: bindings.head_paths.get(&key).cloned().unwrap_or_default(),
                    units,
                },
            );
        }
    }
    // Index exact unit ownership once rather than comparing every chunk with
    // every retained manifest. One unit may legitimately have several owners.
    let mut owners_by_unit = BTreeMap::<NormalizedUnitKey, Vec<&OwnerAuthority>>::new();
    for owner in owners.values() {
        for key in owner.units.keys() {
            owners_by_unit.entry(key.clone()).or_default().push(owner);
        }
    }
    let creations = read_stored_chunks(repo.kio_dir())?;
    let by_creation = creations
        .iter()
        .filter(|creation| {
            chunking_config_hash.is_none_or(|config| creation.row.chunking_config_hash == config)
        })
        .map(|creation| {
            (
                (
                    creation.row.chunk_id.as_str(),
                    creation.row.chunking_config_hash.as_str(),
                ),
                creation,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut publication_cache = PublicationAuthenticationCache::default();
    let mut publications = BTreeSet::new();
    for event in read_chunk_publication_events(repo.kio_dir())? {
        if chunking_config_hash.is_some_and(|config| event.chunking_config_hash != config)
            || graph.node(&event.introduction_commit).is_none()
        {
            continue;
        }
        let creation = by_creation
            .get(&(event.chunk_id.as_str(), event.chunking_config_hash.as_str()))
            .ok_or_else(|| {
                KioError::schema("embedding publication has no durable chunk/config association")
            })?;
        if authenticate_publication_event_cached(
            repo,
            repo.kio_dir(),
            &event,
            creation,
            &mut publication_cache,
        )? {
            publications.insert((
                event.chunk_id,
                event.chunking_config_hash,
                event.introduction_commit,
            ));
        }
    }
    let mut statement = conn
        .prepare(ELIGIBLE_CHUNKS_SQL)
        .map_err(|error| KioError::schema(error.to_string()))?;
    let rows = statement
        .query_map([chunking_config_hash], |row| {
            let heading: String = row.get(6)?;
            let heading_path = serde_json::from_str(&heading).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            Ok((
                row.get::<_, String>(0)?,
                ChunkObject {
                    spec_version: 1,
                    raw_hash: row.get(1)?,
                    tool_profile_hash: row.get(2)?,
                    r#gen: row.get(3)?,
                    unit_key: row.get(4)?,
                    unit_content_hash: row.get(5)?,
                    heading_path,
                    section_id: row
                        .get::<_, Option<String>>(7)?
                        .filter(|value| !value.is_empty()),
                    byte_start: row.get(8)?,
                    byte_end: row.get(9)?,
                    text_hash: row.get(10)?,
                    text: String::new(),
                },
                row.get::<_, String>(11)?,
                row.get::<_, String>(12)?,
            ))
        })
        .map_err(|error| KioError::schema(error.to_string()))?;
    let store = repo.object_store();
    let mut verified_chunks = BTreeSet::new();
    let mut candidates = CandidateAccumulator::new(
        MAX_EMBEDDING_CANDIDATE_PAIRS,
        MAX_EMBEDDING_CANDIDATE_PAYLOAD_BYTES,
    );
    for row in rows {
        let (chunk_id, sql_body, introduction, config) =
            row.map_err(|error| KioError::schema(error.to_string()))?;
        if !publications.contains(&(chunk_id.clone(), config.clone(), introduction)) {
            return Err(KioError::schema(
                "embedding index publication lacks authenticated durable authority",
            ));
        }
        let creation = by_creation
            .get(&(chunk_id.as_str(), config.as_str()))
            .ok_or_else(|| KioError::schema("embedding index chunk has no durable creation"))?;
        let expected = chunk_body(&creation.row);
        if !sql_identity_matches(&sql_body, &expected) {
            return Err(KioError::schema(
                "embedding index chunk differs from its durable semantic identity",
            ));
        }
        if !verified_chunks.insert(chunk_id.clone()) {
            continue;
        }
        if store.read_chunk(&chunk_id)? != expected {
            return Err(KioError::schema(
                "embedding chunk CAS differs from its durable semantic identity",
            ));
        }
        let unit_key = (
            expected.raw_hash.clone(),
            expected.tool_profile_hash.clone(),
            expected.r#gen,
            expected.unit_key.clone(),
            expected.unit_content_hash.clone(),
        );
        for owner in owners_by_unit.get(&unit_key).into_iter().flatten() {
            authenticate_chunk_row(&creation.row, &owner.units)?;
            for path in &owner.paths {
                if let Some(policy) = current_policy
                    && !policy.allows_path(path).map_err(pipeline_to_kio)?
                {
                    continue;
                }
                let is_head_owner = owner.head_paths.contains(path);
                candidates.admit(
                    &chunk_id,
                    &expected,
                    path,
                    bindings.secret_raws.contains(&expected.raw_hash),
                    is_head_owner,
                )?;
            }
        }
    }
    Ok(candidates.into_candidates())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate_body() -> ChunkObject {
        ChunkObject {
            spec_version: 1,
            raw_hash: "raw".into(),
            tool_profile_hash: "profile".into(),
            r#gen: 0,
            unit_key: "unit".into(),
            unit_content_hash: "unit-hash".into(),
            heading_path: Vec::new(),
            section_id: None,
            byte_start: 0,
            byte_end: 4,
            text_hash: "hash".into(),
            text: "body".into(),
        }
    }

    fn assert_limit(error: KioError, dimension: &str, count: usize, limit: usize) {
        assert_eq!(error.error_code(), "KIO-E-EMBED-PLAN-LIMIT-001");
        assert_eq!(error.exit_code(), kio_core::ExitCode::PermanentFailure);
        assert_eq!(
            error.context(),
            &serde_json::json!({
                "dimension": dimension, "count": count, "limit": limit,
            })
        );
    }

    #[test]
    fn candidate_pair_limit_exact_boundary_and_one_over() {
        let body = candidate_body();
        let mut candidates = CandidateAccumulator::new(1, usize::MAX);
        candidates.admit("c", &body, "a", false, false).unwrap();
        let bytes = candidates.payload_bytes;
        assert_limit(
            candidates.admit("c", &body, "b", false, false).unwrap_err(),
            "candidate_pairs",
            2,
            1,
        );
        assert_eq!((candidates.pairs, candidates.payload_bytes), (1, bytes));
        assert_eq!(candidates.into_candidates().len(), 1);
    }

    #[test]
    fn candidate_payload_exact_boundary_and_one_over() {
        let body = candidate_body();
        // First pair: two chunk IDs (2), hash (4), two paths (2), body (4).
        let mut exact = CandidateAccumulator::new(2, 12);
        exact.admit("c", &body, "a", false, false).unwrap();
        assert_eq!(exact.payload_bytes, 12);
        let mut below = CandidateAccumulator::new(2, 11);
        assert_limit(
            below.admit("c", &body, "a", false, false).unwrap_err(),
            "payload_bytes",
            12,
            11,
        );
        assert_eq!((below.pairs, below.payload_bytes), (0, 0));
        assert!(below.chunks.is_empty());
        // A denied chunk must not leave cached text or charges behind.
        let mut smaller = body.clone();
        smaller.text = "new".into();
        below.admit("c", &smaller, "a", false, false).unwrap();
        assert_eq!(&*below.chunks["c"].text, "new");
        assert_eq!(below.payload_bytes, 11);
        assert_limit(
            exact.admit("c", &body, "b", false, false).unwrap_err(),
            "payload_bytes",
            19,
            12,
        );
        assert_eq!((exact.pairs, exact.payload_bytes), (1, 12));
    }

    #[test]
    fn duplicate_candidates_promote_head_without_charge_and_share_body_across_contexts() {
        let body = candidate_body();
        let mut candidates = CandidateAccumulator::new(2, 19);
        candidates.admit("c", &body, "a", true, false).unwrap();
        candidates.admit("c", &body, "b", true, false).unwrap();
        // Both limits are exhausted. A duplicate still promotes HEAD ownership.
        candidates.admit("c", &body, "a", true, true).unwrap();
        assert_eq!((candidates.pairs, candidates.payload_bytes), (2, 19));
        let rows = candidates.into_candidates();
        assert_eq!(rows.len(), 2);
        assert!(Arc::ptr_eq(&rows[0].text, &rows[1].text));
        assert!(rows[0].is_head_owner);
        assert!(!rows[1].is_head_owner);
        assert!(rows.iter().all(|row| row.requires_secret_approval));
        assert_ne!(
            embedding_store::chunk_embedding_context(&rows[0].raw_path),
            embedding_store::chunk_embedding_context(&rows[1].raw_path),
        );
    }

    #[test]
    fn shared_body_is_charged_once_per_chunk_not_across_chunks() {
        let body = candidate_body();
        let mut candidates = CandidateAccumulator::new(3, 24);
        candidates.admit("c", &body, "a", false, false).unwrap();
        candidates.admit("d", &body, "a", false, false).unwrap();
        assert_eq!(candidates.payload_bytes, 24);
        let rows = candidates.into_candidates();
        assert!(!Arc::ptr_eq(&rows[0].text, &rows[1].text));
    }

    #[test]
    fn candidate_checked_arithmetic_rejects_overflow_without_charging() {
        for dimension in ["candidate_pairs", "payload_bytes"] {
            let error =
                checked_candidate_total(dimension, usize::MAX, &[1], usize::MAX).unwrap_err();
            assert_eq!(error.error_code(), "KIO-E-EMBED-PLAN-LIMIT-001");
            assert_eq!(
                error.context(),
                &serde_json::json!({
                    "dimension": dimension, "count": "overflow", "limit": usize::MAX,
                })
            );
        }
        assert!(checked_candidate_total("payload_bytes", 0, &[usize::MAX, 1], usize::MAX).is_err());
        let body = candidate_body();
        let mut candidates = CandidateAccumulator::new(usize::MAX, usize::MAX);
        candidates.payload_bytes = usize::MAX;
        assert!(candidates.admit("c", &body, "a", false, false).is_err());
        assert_eq!(
            (candidates.pairs, candidates.payload_bytes),
            (0, usize::MAX)
        );
        assert!(candidates.chunks.is_empty());
        candidates.payload_bytes = 0;
        candidates.pairs = usize::MAX;
        assert!(candidates.admit("c", &body, "a", false, false).is_err());
        assert_eq!(
            (candidates.pairs, candidates.payload_bytes),
            (usize::MAX, 0)
        );
        assert!(candidates.chunks.is_empty());
    }

    fn pinned_instance(
        repo: &Repository,
        unit_key: &str,
        markdown: &str,
    ) -> (NormalizeRef, ChunkRow) {
        let raw = hash_bytes(b"same raw");
        let profile = hash_bytes(b"same profile");
        let unit = NormalizedUnitObject {
            unit_key: unit_key.to_owned(),
            unit_type: UnitType::Page,
            raw_hash: raw.clone(),
            prepared_hash: hash_bytes(b"prepared"),
            preparation_profile_hash: hash_bytes(b"prepare profile"),
            tool_profile_hash: profile.clone(),
            r#gen: 0,
            mode: MarkdownizeMode::Full,
            markdown: markdown.to_owned(),
            owned_image_hashes: BTreeSet::new(),
            metadata: BTreeMap::new(),
            reused_from: None,
            generated_at: "2026-09-26T00:00:00Z".to_owned(),
        };
        let manifest = NormalizedInstanceManifest {
            raw_hash: raw.clone(),
            tool_profile_hash: profile.clone(),
            r#gen: 0,
            parent_gen: None,
            run_id: "embedding-owner-test".to_owned(),
            units: vec![NormalizedUnitManifestEntry {
                order: 0,
                unit_key: unit_key.to_owned(),
                unit_ref: unit_ref(unit_key),
                unit_type: UnitType::Page,
                status: UnitStatus::Done,
                prepared_hash: unit.prepared_hash.clone(),
                preparation_profile_hash: unit.preparation_profile_hash.clone(),
                unit_object_hash: None,
                error_kind: None,
            }],
            generated_at: unit.generated_at.clone(),
        };
        let stamped = persist_normalized_instance(repo.kio_dir(), &manifest, &[unit]).unwrap();
        let manifest_hash = repo
            .object_store()
            .write_content_object(
                ContentObjectKind::Manifest,
                &canonical_json_bytes(&serde_json::to_value(&stamped).unwrap()).unwrap(),
            )
            .unwrap();
        let normalize = NormalizeRef {
            tool_profile_hash: profile.clone(),
            r#gen: 0,
            manifest_hash,
        };
        let mut row = ChunkRow {
            chunk_id: String::new(),
            raw_hash: raw,
            tool_profile_hash: profile,
            r#gen: 0,
            unit_key: unit_key.to_owned(),
            unit_content_hash: hash_bytes(markdown.as_bytes()),
            chunking_config_hash: hash_bytes(b"chunk config"),
            raw_path: "public.md".to_owned(),
            heading_path: None,
            section_id: None,
            byte_start: 0,
            byte_end: markdown.len() as u64,
            text_hash: hash_bytes(markdown.as_bytes()),
            text: markdown.to_owned(),
            created_at: "2026-09-26T00:00:00Z".to_owned(),
        };
        row.chunk_id = chunk_hash(&row).unwrap();
        repo.object_store().write_chunk(&chunk_body(&row)).unwrap();
        (normalize, row)
    }

    #[test]
    fn same_generation_manifest_cannot_borrow_another_manifests_unit_or_text() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let (first, first_row) = pinned_instance(&repo, "page:1", "first immutable text");
        let (second, second_row) = pinned_instance(&repo, "page:2", "second immutable text");
        assert_eq!(
            (&first.tool_profile_hash, first.r#gen),
            (&second.tool_profile_hash, second.r#gen)
        );
        assert_ne!(first.manifest_hash, second.manifest_hash);
        let first_units = exact_unit_authorities(&repo, &first_row.raw_hash, &first).unwrap();
        let second_units = exact_unit_authorities(&repo, &second_row.raw_hash, &second).unwrap();
        authenticate_chunk_row(&first_row, &first_units).unwrap();
        authenticate_chunk_row(&second_row, &second_units).unwrap();
        assert!(authenticate_chunk_row(&second_row, &first_units).is_err());
        assert!(authenticate_chunk_row(&first_row, &second_units).is_err());
        let mut forged = first_row.clone();
        forged.text = "substituted SQL text".to_owned();
        forged.text_hash = hash_bytes(forged.text.as_bytes());
        assert!(authenticate_chunk_row(&forged, &first_units).is_err());
        assert_ne!(
            chunk_body(&forged),
            repo.object_store().read_chunk(&first_row.chunk_id).unwrap()
        );
    }

    #[test]
    fn selected_aliases_cannot_cross_exact_manifest_bindings() {
        let shared_raw_paths = BTreeSet::from([
            "old.md".to_owned(),
            "new.md".to_owned(),
            "foreign.md".to_owned(),
        ]);
        let old_manifest_paths = BTreeSet::from(["old.md".to_owned()]);
        let new_manifest_paths = BTreeSet::from(["new.md".to_owned()]);
        assert_eq!(
            selected_paths(&old_manifest_paths, &shared_raw_paths),
            old_manifest_paths
        );
        assert_eq!(
            selected_paths(&new_manifest_paths, &shared_raw_paths),
            new_manifest_paths
        );
        assert!(selected_paths(&old_manifest_paths, &new_manifest_paths).is_empty());
        assert_eq!(
            selected_paths(&new_manifest_paths, &BTreeSet::from(["new.md".to_owned()])),
            new_manifest_paths
        );
    }
    #[test]
    fn raw_alias_taint_precedes_selection_and_head_requires_exact_manifest() {
        let raw = hash_bytes(b"one raw with aliases");
        let old = NormalizeRef {
            tool_profile_hash: hash_bytes(b"profile"),
            r#gen: 0,
            manifest_hash: hash_bytes(b"old manifest"),
        };
        let mut new = old.clone();
        new.manifest_hash = hash_bytes(b"new manifest");
        let mut bindings = OwnerBindings::default();
        for (path, normalize, is_head) in [
            ("old.md", Some(old.clone()), false),
            ("new.md", Some(new.clone()), true),
            (".env", None, true),
        ] {
            bindings.record(
                &kio_core::dag::TreeEntry {
                    path: path.to_owned(),
                    entry_type: "file".to_owned(),
                    raw_hash: raw.clone(),
                    normalize,
                },
                is_head,
            );
        }
        // The secret alias is outside both the selected path set and the
        // normalized identities; neither fact can remove raw-wide consent.
        let selected = BTreeSet::from(["old.md".to_owned(), "new.md".to_owned()]);
        let old_key = instance_key(&raw, &old);
        let new_key = instance_key(&raw, &new);
        assert_eq!(
            selected_paths(&bindings.paths[&old_key], &selected),
            BTreeSet::from(["old.md".to_owned()])
        );
        assert!(bindings.secret_raws.contains(&raw));
        assert!(!bindings.head_paths.contains_key(&old_key));
        assert_eq!(
            bindings.head_paths[&new_key],
            BTreeSet::from(["new.md".to_owned()])
        );
    }
    #[test]
    fn search_projection_is_not_embedding_text_authority() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let markdown = "escaped \\*markup\\* and cafe\u{301}\u{0}";
        let (normalize, row) = pinned_instance(&repo, "page:1", markdown);
        let canonical = repo.object_store().read_chunk(&row.chunk_id).unwrap();
        let authority = exact_unit_authorities(&repo, &row.raw_hash, &normalize).unwrap();
        authenticate_chunk_row(&row, &authority).unwrap();
        let mut projection = canonical.clone();
        projection.text = "poisoned SQLite search text".to_owned();
        assert_ne!(projection.text, canonical.text);
        assert!(sql_identity_matches(&projection, &canonical));
        let mut candidates = CandidateAccumulator::new(1, 1024);
        candidates
            .admit(&row.chunk_id, &canonical, "public.md", false, true)
            .unwrap();
        let candidate = candidates.into_candidates().pop().unwrap();
        assert_eq!(&*candidate.text, "escaped *markup* and café");
        assert_ne!(&*candidate.text, projection.text);
        assert_eq!(candidate.text_hash, canonical.text_hash);
        assert_eq!(
            repo.object_store().read_chunk(&row.chunk_id).unwrap(),
            canonical
        );
        assert_eq!(canonical.text, markdown);
        assert_eq!(canonical.text_hash, hash_bytes(markdown.as_bytes()));
        projection.text_hash = hash_bytes(projection.text.as_bytes());
        assert!(!sql_identity_matches(&projection, &canonical));
    }
    #[test]
    fn config_selector_requires_matching_publication_and_creation_association() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE chunks(chunk_id TEXT, raw_hash TEXT, tool_profile_hash TEXT, gen INTEGER,
            unit_key TEXT, unit_content_hash TEXT, heading_path TEXT, section_id TEXT,
            byte_start INTEGER, byte_end INTEGER, text_hash TEXT);
            CREATE TABLE chunk_publications(chunk_id TEXT, chunking_config_hash TEXT, introduction_commit TEXT);
            CREATE TABLE chunk_config_generations(chunk_id TEXT, chunking_config_hash TEXT);
            INSERT INTO chunks VALUES('chunk','raw','profile',0,'unit','content','[]',NULL,0,1,'text');
            INSERT INTO chunk_publications VALUES('chunk','config-a','intro-a'),('chunk','config-b','intro-b'),('chunk','publication-only','intro-c');
            INSERT INTO chunk_config_generations VALUES('chunk','config-a'),('chunk','config-b'),('chunk','association-only');").unwrap();
        let eligible = |selector: Option<&str>| {
            conn.prepare(ELIGIBLE_CHUNKS_SQL)
                .unwrap()
                .query_map([selector], |row| row.get::<_, String>(12))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(eligible(None), vec!["config-a", "config-b"]);
        assert_eq!(eligible(Some("config-a")), vec!["config-a"]);
        assert!(eligible(Some("publication-only")).is_empty());
        assert!(eligible(Some("association-only")).is_empty());
    }
}
