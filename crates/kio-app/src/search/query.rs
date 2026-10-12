use crate::commands::SearchArgs;
use crate::commands::SearchMode as SearchModeArg;
use crate::diagnostic_safe_text;
use crate::historical_reindex::pinned_done_units;
use crate::index::chunk_ledger::read_stored_chunks;
use crate::index::chunk_ledger::sqlite_path;
use crate::index_to_kio;
use crate::invocation::Invocation;
use crate::open_view::harden_open_cache_subtree;
use crate::open_view::write_open_cache_atomic;
use crate::scope_access::resolve_scope_id_in_registry;
use crate::search::scope::ExecScope;
use crate::search::scope::ParsedSearch;
use crate::search::scope::ScopeTarget;
use crate::search::scope::ScoredCandidate;
use crate::search::scope::SearchFailBehavior;
use crate::search::scope::SearchableChunk;
use crate::search_time::TimeSelector;
use crate::search_time::TimeSelectorFlags;
use crate::search_to_kio;
use crate::settings::append_jsonl_cli;
use crate::settings::cache_home;
use crate::settings::data_home;
use crate::settings::device_logs_retention_days;
use crate::settings::scope_logs_retention_days;
use crate::settings::user_config_toml_path;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::dag::NormalizeRef;
use kio_core::history::HistoryReader;
use kio_core::scope::Repository;
use kio_index::embedding_store::f32_from_le_bytes;
use kio_index::embedding_store::f32_to_le_bytes;
use kio_index::fts::ExistingSourceIndexOpenMode;
use kio_index::fts::FtsSchemaConfig;
use kio_index::fts::FtsTokenizer;
use kio_index::fts::open_existing_source_index_connection;
use kio_search::cursor::CursorToken;
use kio_search::cursor::ScopeMode;
use kio_search::mmr::MmrCandidate;
use kio_search::mmr::MmrConfig;
use kio_search::mmr::diversify_candidates;
use kio_search::object_uri::RelatedImage;
use kio_search::query::DiversifyRequest;
use kio_search::query::DiversifyStrategy;
use kio_search::query::ScopeSelectionMode;
use kio_search::query::SearchMode;
use kio_search::query::TimeTravelSelector;
#[cfg(test)]
use kio_search::rrf::BackendRank;
use kio_search::rrf::RrfConfig;
use rusqlite::Connection;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;

/// Resolve the one authoritative chunking configuration for a snapshot.
/// Every current tree carries this required field; there is no association-
/// inferred, live-config, or old-format fallback.
pub(crate) fn snapshot_chunking_config_hash(repo: &Repository, commit: &str) -> Result<String> {
    Ok(
        HistoryReader::new_with_store(repo.kio_dir(), repo.object_store())
            .snapshot(commit)?
            .tree
            .chunking_config_hash,
    )
}

#[cfg(test)]
type VectorLaneHits = Vec<(String, f64)>;

/// Rank `chunk_vec` and `image_vec` hits as the single vector lane they are.
///
/// 03 §7 fixes ONE multimodal embedding space and 04 §4.3 says so explicitly of
/// these two tables: the split is sqlite-vec's one-primary-key-type-per-table
/// constraint, not a semantic boundary. So the cosine distances are directly
/// comparable, and merging them before ranks are assigned is what stops the
/// nearest image in the corpus from tying the nearest chunk for rank 1 no
/// matter how far away it actually is.
///
/// Ordering is `(distance, id)` — the same shape as each backend's own
/// `ORDER BY`, which keeps a chunk-only corpus byte-identical to what it
/// produced before images existed. Ids are content hashes from different byte
/// streams, so the second key is a total order in practice as well as in form.
#[cfg(test)]
pub(crate) fn merge_vector_lane(
    chunk_hits: VectorLaneHits,
    image_hits: VectorLaneHits,
    candidate_depth: u64,
) -> Vec<BackendRank> {
    if image_hits.is_empty() {
        return chunk_hits
            .into_iter()
            .enumerate()
            .map(|(index, (id, _))| BackendRank {
                chunk_hash: id,
                rank: index as u64 + 1,
            })
            .collect();
    }
    let mut merged = chunk_hits;
    merged.extend(image_hits);
    merged.sort_by(|left, right| {
        left.1
            .total_cmp(&right.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    merged.truncate(candidate_depth as usize);
    merged
        .into_iter()
        .enumerate()
        .map(|(index, (id, _))| BackendRank {
            chunk_hash: id,
            rank: index as u64 + 1,
        })
        .collect()
}

/// U5 / §4.2 問題 A: an image has no text-lane presence of its own, so RRF adds
/// two reciprocal terms for a chunk and one for an image — images sink
/// structurally, whatever the vector lane thinks of them.
///
/// The fix is the plan's option (a) — "画像に text 表現を与える", using the body
/// of the chunk that cites it — implemented by **inheriting that chunk's rank
/// rather than indexing a copy of its text**.
///
/// Indexing a duplicate FTS document was the literal reading and is the wrong
/// one. BM25's statistics are corpus-wide: an extra document per image inflates
/// `N`, inflates `df` for every term in an image-bearing body, and shifts
/// `avgdl` — so adding figures to one document would quietly re-rank every
/// unrelated text hit on the device. That is the exact defect 05 §1.8 built the
/// replica to remove, and it would be reintroduced one layer down. Inheritance
/// gets the identical ranking outcome (the image's text-lane standing IS its
/// citing chunk's, which is what option (a) says) while leaving the corpus
/// alone.
///
/// The entry is inserted immediately AFTER its chunk so the list stays in rank
/// order for `fuse_rrf`'s `take(candidate_depth)` window — appending at the end
/// would drop every image's text term as soon as the text lane was full.
#[cfg(test)]
pub(crate) fn give_images_their_chunks_text_rank(
    text_ranks: &mut Vec<BackendRank>,
    image_rows: &BTreeMap<String, ImageCandidate>,
) {
    if image_rows.is_empty() || text_ranks.is_empty() {
        return;
    }
    let mut by_chunk = BTreeMap::<&str, Vec<&ImageCandidate>>::new();
    for image in image_rows.values() {
        by_chunk
            .entry(image.chunk_hash.as_str())
            .or_default()
            .push(image);
    }
    let mut widened = Vec::with_capacity(text_ranks.len());
    for entry in text_ranks.iter() {
        widened.push(entry.clone());
        for image in by_chunk
            .get(entry.chunk_hash.as_str())
            .into_iter()
            .flatten()
        {
            widened.push(BackendRank {
                chunk_hash: image.image_hash.clone(),
                rank: entry.rank,
            });
        }
    }
    *text_ranks = widened;
}

/// One image object that can be returned as a search result (05 §1.7).
#[cfg(test)]
#[allow(dead_code)]
#[derive(Clone)]
pub(crate) struct ImageCandidate {
    /// The `image_vec` key — this row's own identity.
    pub(crate) image_hash: String,
    /// The URI as the referencing body writes it, never re-derived (05 §1.7).
    pub(crate) image_uri: String,
    /// V6's choice of referencing chunk, whose pointer this row will carry.
    pub(crate) chunk_hash: String,
    pub(crate) meta: crate::search::scope::ChunkMeta,
}

/// The figure sizes one normalized unit recorded, keyed by image object hash.
pub(crate) struct UnitFigures {
    /// Area of each figure's bbox, in whatever units the provider's page
    /// coordinates use. Only the ratios between them are ever read.
    areas: BTreeMap<String, i64>,
    /// The largest figure anywhere in the unit — the denominator. Taken over
    /// the whole page rather than over the citing chunk, because a chunk that
    /// holds nothing but decoration must come back empty rather than promote
    /// its biggest sticker.
    largest: i64,
}

/// Memo of [`read_instance_figures`] across one search's result rows.
///
/// Keyed by the tree-pinned normalized INSTANCE, not by the unit: a page of
/// results from one scanned PDF is one instance and many units, and the loader
/// validates a whole instance per call. The manifest pin keeps a later
/// same-generation retry's metadata from leaking into a historical result.
pub(crate) type UnitFigureCache =
    BTreeMap<(String, String, u64, String), BTreeMap<String, UnitFigures>>;

/// Read every unit's figure sizes out of one normalized instance.
///
/// Both OCR routes write `metadata["images"]` in the same shape (07 §5.2), so
/// one reader serves the online and offline lanes alike. Anything unreadable is
/// simply absent from the result and its caller then keeps the unfiltered list:
/// a purged instance, a unit that predates the metadata, or a provider that
/// records no boxes are all reasons to know less, not reasons to hand the Agent
/// less.
pub(crate) fn read_instance_figures(
    repo: &Repository,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
    manifest_hash: &str,
) -> BTreeMap<String, UnitFigures> {
    let normalize = NormalizeRef {
        tool_profile_hash: tool_profile_hash.to_owned(),
        r#gen,
        manifest_hash: manifest_hash.to_owned(),
    };
    let Ok(units) = pinned_done_units(repo, raw_hash, &normalize) else {
        return BTreeMap::new();
    };
    let mut by_unit = BTreeMap::new();
    for unit in units {
        let Some(images) = unit.metadata.get("images").and_then(Value::as_array) else {
            continue;
        };
        let mut areas = BTreeMap::new();
        for image in images {
            let (Some(hash), Some(bbox)) = (
                image.get("hash").and_then(Value::as_str),
                image.get("bbox").and_then(Value::as_array),
            ) else {
                continue;
            };
            // Layout metadata may describe arbitrary provider-supplied hashes.
            // It can refine presentation only for an image this immutable unit
            // typed as actually decoded and persisted.
            if !unit.owned_image_hashes.contains(hash) {
                continue;
            }
            let [x0, y0, x1, y1] = bbox.as_slice() else {
                continue;
            };
            let (Some(x0), Some(y0), Some(x1), Some(y1)) =
                (x0.as_i64(), y0.as_i64(), x1.as_i64(), y1.as_i64())
            else {
                continue;
            };
            areas.insert(hash.to_owned(), (x1 - x0).max(0) * (y1 - y0).max(0));
        }
        let Some(largest) = areas.values().copied().max().filter(|area| *area > 0) else {
            continue;
        };
        by_unit.insert(unit.unit_key, UnitFigures { areas, largest });
    }
    by_unit
}

/// Drop the `related_images[]` entries too small to be worth an Agent's round
/// trip (05-runtime §1.7).
///
/// A chunk names every image its body references, and on a laid-out page most
/// of those are stickers next to text that already says the thing. Measured on
/// one real infographic, six queries returned 54 references of which 52 were
/// decoration; heading chunking makes this worse by putting half a page into
/// one chunk, so any hit in that half drags all of its decoration along.
///
/// Size is the discriminator that survives inspection. `block_label` does not:
/// the invoice capture's only figure is spelled `image` and is larger than
/// either chart on the infographic, and PaddleOCR-VL has no `figure` label for
/// real figures to land in.
///
/// Applied here rather than inside `extract_related_images` on purpose. That
/// extractor also answers *which objects must stay alive* (`verify_objects`,
/// purge reachability), and a decoration filtered out of it would become an
/// orphan and be collected. This is a presentation choice, so it lives at the
/// presentation site and nothing else observes it.
pub(crate) fn significant_related_images(
    images: Vec<RelatedImage>,
    figures: &UnitFigures,
    min_area_ratio: f64,
    scope_id: &str,
) -> Vec<RelatedImage> {
    let floor = (figures.largest as f64 * min_area_ratio).ceil() as i64;
    images
        .into_iter()
        // An image the unit recorded no box for is not judged — being
        // unmeasured is not the same as being small.
        .filter(|image| {
            let area = kio_search::object_uri::parse_object_uri(&image.image_uri)
                .ok()
                .filter(|uri| uri.scope_id() == scope_id)
                .and_then(|uri| figures.areas.get(uri.hash()));
            match area {
                Some(area) => *area >= floor,
                None => true,
            }
        })
        // `order` is documented as the position within this list, so it has to
        // be re-derived after a removal rather than carried through.
        .enumerate()
        .map(|(order, image)| RelatedImage { order, ..image })
        .collect()
}

pub(crate) fn current_max_rowid(conn: &Connection) -> Result<u64> {
    let value: i64 = conn
        .query_row("SELECT COALESCE(MAX(rowid), 0) FROM chunks", [], |row| {
            row.get(0)
        })
        .map_err(|err| KioError::schema(err.to_string()))?;
    Ok(value as u64)
}

/// Every commit for which the writer has retained a source tree projection.
///
/// These include commits no longer reachable from HEAD: a user can retain one
/// under a tag (or leave it disconnected after changing branches) and later
/// select it with `kio search --at`.  The writer materializes its replica
/// binding while source SQLite is available; readers must not recreate it.
pub(crate) fn source_tree_entry_snapshot_commits(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut statement = conn
        .prepare("SELECT DISTINCT commit_hash FROM tree_entries ORDER BY commit_hash")
        .map_err(|error| KioError::schema(error.to_string()))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| KioError::schema(error.to_string()))?;
    rows.collect::<std::result::Result<BTreeSet<_>, _>>()
        .map_err(|error| KioError::schema(error.to_string()))
}

pub(crate) enum SnapshotTreeEntries {
    Projected,
}

/// Ensure `tree_entries` rows for `commit_hash` exist in `conn`, projecting them
/// from the commit's tree object when absent (04 §4.5). Both the commit and tree
/// must be present in CAS even when cache rows already exist; SQLite is never a
/// substitute for immutable history truth.
pub(crate) fn ensure_snapshot_tree_entries(
    repo: &Repository,
    conn: &Connection,
    commit_hash: &str,
) -> Result<SnapshotTreeEntries> {
    // R11-11 (Spark): existence probe only — an EXISTS/LIMIT-1 stops at the first
    // matching row instead of a `COUNT(*)` PK-prefix scan over every tree_entries
    // row of the commit (wasteful for large commits). Functionally equivalent.
    let existing: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM tree_entries WHERE commit_hash = ?1)",
            rusqlite::params![commit_hash],
            |row| row.get(0),
        )
        .map_err(|err| KioError::schema(err.to_string()))?;
    let commit = repo.read_commit(commit_hash)?;
    let tree = repo.read_tree(&commit.tree)?;
    if existing {
        return Ok(SnapshotTreeEntries::Projected);
    }
    let mut rows: Vec<TreeEntryProjection> = Vec::new();
    for entry in &tree.entries {
        let (tool_profile_hash, r#gen, manifest_hash) =
            entry
                .normalize
                .as_ref()
                .map_or((None, None, None), |normalize| {
                    (
                        Some(normalize.tool_profile_hash.clone()),
                        Some(normalize.r#gen),
                        Some(normalize.manifest_hash.clone()),
                    )
                });
        rows.push(TreeEntryProjection {
            path: entry.path.clone(),
            raw_hash: entry.raw_hash.clone(),
            tool_profile_hash,
            r#gen,
            manifest_hash,
        });
    }
    insert_snapshot_tree_entries(conn, commit_hash, &rows)?;
    Ok(SnapshotTreeEntries::Projected)
}

/// One projected `tree_entries` row for [`insert_snapshot_tree_entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeEntryProjection {
    pub(crate) path: String,
    pub(crate) raw_hash: String,
    pub(crate) tool_profile_hash: Option<String>,
    pub(crate) r#gen: Option<u64>,
    pub(crate) manifest_hash: Option<String>,
}

/// R10-8: insert a commit's projected `tree_entries` rows in ONE transaction.
/// Writer-side snapshot materialization short-circuits when `existing > 0`; a
/// non-transactional loop that crashed mid-way left a partial row set that
/// `existing > 0` then refused to complete. Wrapping the inserts makes the
/// projection all-or-nothing, so an interruption keeps `existing = 0` for the
/// next writer-side materialization attempt.
pub(crate) fn insert_snapshot_tree_entries(
    conn: &Connection,
    commit_hash: &str,
    rows: &[TreeEntryProjection],
) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|err| KioError::schema(err.to_string()))?;
    for row in rows {
        tx.execute(
            "INSERT OR REPLACE INTO tree_entries(commit_hash, path, raw_hash, tool_profile_hash, gen, manifest_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                commit_hash,
                row.path,
                row.raw_hash,
                row.tool_profile_hash,
                row.r#gen,
                row.manifest_hash
            ],
        )
        .map_err(|err| KioError::schema(err.to_string()))?;
    }
    tx.commit()
        .map_err(|err| KioError::schema(err.to_string()))?;
    Ok(())
}

/// Apply diversify (05 §1.4/§1.8) to the merged candidate pool and report the
/// strategy actually applied. Text-only (no embeddings) means MMR is skipped and
/// only the `max_per_raw_hash` dedup runs — reported honestly as
/// `group_by_raw_hash`, never a phantom "mmr" (K2 fix for the false report).
pub(crate) fn diversify_merged<'a>(
    candidates: &'a [ScoredCandidate],
    resolved_mode: SearchMode,
    request: &DiversifyRequest,
) -> Result<(Vec<&'a ScoredCandidate>, Value)> {
    let mmr_candidates = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| MmrCandidate {
            // Composite id keeps candidates distinct even if two scopes carry the
            // same chunk_hash; raw_hash stays real so cross-scope dedup counts them
            // together (CT3-MULTI-003).
            chunk_hash: format!("{index}\u{0}{}", candidate.chunk_hash),
            raw_hash: candidate.meta.raw_hash.clone(),
            relevance: candidate.rrf_score,
            // Real embedding in hybrid/vector mode (05 §1.4). If any candidate
            // lacks one, `diversify_candidates` skips MMR and only dedups.
            embedding: candidate.embedding.clone(),
            heading_path: candidate.meta.heading_path.clone(),
            section_id: candidate.meta.section_id.clone(),
        })
        .collect::<Vec<_>>();

    let diversified = diversify_candidates(
        &mmr_candidates,
        MmrConfig {
            strategy: request.strategy,
            mmr_lambda: request.mmr_lambda.unwrap_or(0.7),
            max_per_raw_hash: request.max_per_raw_hash.unwrap_or(3),
            mmr_depth: request.mmr_depth.unwrap_or(100),
        },
    )
    .map_err(search_to_kio)?;

    let ordered = diversified
        .iter()
        .filter_map(|item| {
            item.chunk_hash
                .split_once('\u{0}')
                .and_then(|(index, _)| index.parse::<usize>().ok())
                .and_then(|index| candidates.get(index))
        })
        .collect::<Vec<_>>();

    // Report the strategy actually applied (05 §1.7). MMR needs an embedding for
    // *every* candidate (mmr.rs skips otherwise); text mode carries none, so only
    // the raw_hash dedup ran — reported honestly, never a phantom "mmr" (K2).
    let all_have_embeddings = !candidates.is_empty()
        && candidates
            .iter()
            .all(|candidate| candidate.embedding.is_some());
    let mmr_ran = matches!(resolved_mode, SearchMode::Hybrid | SearchMode::Vector)
        && request.strategy == DiversifyStrategy::Mmr
        && all_have_embeddings;
    let summary = match request.strategy {
        DiversifyStrategy::Off => json!({ "strategy": "off" }),
        DiversifyStrategy::Mmr if mmr_ran => {
            json!({ "strategy": "mmr", "mmr_lambda": request.mmr_lambda.unwrap_or(0.7) })
        }
        _ => json!({ "strategy": "group_by_raw_hash" }),
    };
    Ok((ordered, summary))
}

/// PC8/PC9 (05 §1.3 L110-115) + deterministic script-boundary segmentation
/// (05 §1.3 L120-134, 2026-07-22 spec feedback #2): the query's fixed
/// tokenization/unit-generation and the resulting MATCH-generation plan —
/// see [`build_query_plan`].
pub(crate) struct QueryPlan {
    /// The single OR-joined FTS5 MATCH expression over every UNIT (an
    /// original whitespace token, or one of its [`segment_script_runs`]
    /// pieces) with >= 3 Unicode scalars, or `None` when every unit is short
    /// (PC11: bounded-LIKE-only fallback — a PURE-SHORT query, which by
    /// construction means every original token was short too, since no
    /// segmented piece can be longer than its own token).
    pub(crate) match_expr: Option<String>,
    /// The bounded-LIKE `instr` eligibility set (PC11/PC14). **Empty** for a
    /// MIXED query (>= 1 long unit): 05 §1.3's 2026-07-22 feedback #2 drops
    /// every short unit outright rather than AND-filtering candidates on it
    /// (the predicate this replaced excluded documents that never spelled a
    /// natural-sentence particle/function word — eval M3-2/M3-3's dominant
    /// Recall@10 failure mode). **The RAW token list, first-occurrence
    /// deduplicated** (not the segmented unit list) for a PURE-SHORT query
    /// (R23-28 — 05 §1.3 L129-131's "全 unit 集合から重複を除く" was never
    /// exempted for this branch; dedup preserves the first token unchanged,
    /// so PC14's ORDER BY tie-break is unaffected).
    pub(crate) short_tokens: Vec<String>,
}

/// PC9 (05 §1.3 L113-115): tokenization is fixed and deterministic — split
/// the caller's NFC-normalized query (`query_nfc`, matching the NFC-normalized
/// index projection, F2) on Unicode whitespace (`split_whitespace` uses
/// `char::is_whitespace`, the Unicode White_Space property; consecutive
/// whitespace collapses to one boundary, matching "連続空白は1区切り").
/// Every non-empty resulting piece is one token, including a symbol-only
/// piece (e.g. `++`) — no character-class filtering. Token length is
/// Unicode SCALAR count (`chars().count()`), never UTF-8 byte length.
pub(crate) fn query_tokens(query_nfc: &str) -> Vec<String> {
    query_nfc.split_whitespace().map(str::to_owned).collect()
}

/// Quote a token as an FTS5 phrase (`"` doubled) so `=`, quotes, and operators in
/// user input are inert — arbitrary input can never raise an FTS5 syntax error
/// (brief-common #6).
fn quote_fts_phrase(unit: &str) -> String {
    format!("\"{}\"", unit.replace('"', "\"\""))
}

/// Unicode script class used by [`segment_script_runs`]'s deterministic
/// sub-token boundaries (05 §1.3 2026-07-22 spec feedback #2, "決定的
/// スクリプト境界分割"). An `Other` run is never emitted as a unit — it is a
/// pure separator (the original token, which the caller always keeps as its
/// own unit alongside the segmented pieces, already preserves a symbol like
/// `C++`'s trailing `+`s as an exact-phrase signal).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScriptClass {
    Hiragana,
    Katakana,
    Han,
    Alnum,
    Other,
}

/// `ch`'s script class, EXCLUDING U+30FC (ー, the katakana-block long vowel
/// mark) and the digit-neighbor `.`/`,` exception — both are
/// context-sensitive on neighboring characters and are resolved by
/// [`segment_script_runs`] itself before falling back to this function.
fn classify_script_char(ch: char) -> ScriptClass {
    let cp = ch as u32;
    if (0x3041..=0x309F).contains(&cp) {
        return ScriptClass::Hiragana;
    }
    // R23-06 (05 §1.3, 2026-07-22 spec feedback #2 correction): U+30FB (・,
    // katakana middle dot) is Unicode script=Common, NOT script=Katakana,
    // despite sitting inside the katakana Unicode BLOCK (U+30A0-U+30FF) —
    // excluded here so it falls through to `Other` (a separator) below.
    // Without this, "アクセス・トークン" stays one unsplittable unit and
    // never matches either half. U+30FD/U+30FE (ヽ/ヾ, katakana iteration
    // marks) and U+30F6 (ヶ) ARE script=Katakana and stay in this range
    // unchanged; ー (U+30FC) is handled specially by the caller before this
    // function is ever reached for it, per the doc comment above.
    if cp != 0x30FB && ((0x30A0..=0x30FF).contains(&cp) || (0x31F0..=0x31FF).contains(&cp)) {
        return ScriptClass::Katakana;
    }
    // cp == 0x3005 is 々 (iteration mark), cp == 0x3006 is 〆 (closing mark) —
    // both join an adjacent Han run per 05 §1.3's class list.
    if (0x3400..=0x4DBF).contains(&cp)
        || (0x4E00..=0x9FFF).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0x20000..=0x3FFFF).contains(&cp)
        || cp == 0x3005
        || cp == 0x3006
    {
        return ScriptClass::Han;
    }
    if ch.is_ascii_alphanumeric() {
        return ScriptClass::Alnum;
    }
    ScriptClass::Other
}

/// PC8's unit-generation input (05 §1.3 2026-07-22 spec feedback #2,
/// "決定的スクリプト境界分割"): split `token` at every Unicode-script-class
/// transition and return each non-`Other` run, in input order. This is the
/// deterministic sub-token piece list [`build_query_plan`] OR-alongside the
/// token itself, so an agglutinated particle (query `スコープが` vs. corpus
/// `スコープは`) or a symbol-joined enumeration (query `read/write/admin` vs.
/// corpus `read / write / admin`) no longer has to match the whitespace
/// token byte-for-byte.
///
/// Class rules (fixed at the `char` level — one Unicode scalar each):
/// - Hiragana (U+3041-U+309F), Katakana (U+30A0-U+30FF, U+31F0-U+31FF — ヶ/
///   ヽ/ヾ included, **U+30FB (・) excluded** — script=Common despite the
///   block, falls to `Other` below, R23-06), Han (U+3400-U+4DBF,
///   U+4E00-U+9FFF, U+F900-U+FAFF, U+20000-U+3FFFF, plus 々/〆), ASCII alnum
///   (`[0-9A-Za-z]`), else `Other` ([`classify_script_char`]).
/// - U+30FC (ー) is the one context-sensitive exception: it continues the
///   IMMEDIATELY PRECEDING character's own RESOLVED class when that class is
///   Hiragana or Katakana (so `ローテーション`'s two ー never split its run);
///   at the start of the token, or after any other class, it is Katakana
///   (its own block's default).
/// - `.`/`,` join an Alnum run only when the character immediately before
///   AND immediately after it are both ASCII digits (`99.9`, `3,600`,
///   `3.2GB` are each one run); otherwise they are `Other` (a bare
///   separator, dropped).
/// - `Other` runs are dropped entirely — never returned as a unit.
pub(crate) fn segment_script_runs(token: &str) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    let len = chars.len();
    let mut classes: Vec<ScriptClass> = Vec::with_capacity(len);
    for (index, &ch) in chars.iter().enumerate() {
        let class = if ch == '\u{30FC}' {
            match classes.last() {
                Some(ScriptClass::Hiragana) => ScriptClass::Hiragana,
                Some(ScriptClass::Katakana) => ScriptClass::Katakana,
                _ => ScriptClass::Katakana,
            }
        } else if (ch == '.' || ch == ',')
            && index > 0
            && index + 1 < len
            && chars[index - 1].is_ascii_digit()
            && chars[index + 1].is_ascii_digit()
        {
            ScriptClass::Alnum
        } else {
            classify_script_char(ch)
        };
        classes.push(class);
    }
    // Run-length grouping: every character in `[run_start, index)` shares
    // `classes[run_start]`'s class by construction (a new run only starts
    // right after a detected transition), so comparing the newest class
    // against `classes[run_start]` is equivalent to — and cheaper than —
    // comparing against `classes[index - 1]`.
    let mut units = Vec::new();
    let mut run_start = 0usize;
    for index in 1..=len {
        if index == len || classes[index] != classes[run_start] {
            if classes[run_start] != ScriptClass::Other {
                units.push(chars[run_start..index].iter().collect());
            }
            run_start = index;
        }
    }
    units
}

/// Deterministic query normalization (05 §1.3 L116-123, 2026-07-22 spec
/// feedback #1): the thousands-separator twin of a >= 4 digit numeral, in
/// EITHER direction — plain digits gain a comma-grouped form (`3600` ->
/// `3,600`) and a well-formed comma-grouped numeral loses its commas
/// (`3,600` -> `3600`). `None` when `token` doesn't qualify (fewer than 4
/// digits, or not a numeral at all), so a caller never OR-injects a
/// redundant arm equal to `token` itself. This restores (byte-for-byte,
/// forward direction) the grouping algorithm the pre-PC8
/// `fts_keyword_group`/`thousands_separated` pair used
/// (`git show e3f2a94^:crates/kio-cli/src/main.rs`) before PC8 (05 §1.3's
/// original, too-strict "query 由来でない追加語を含まない" reading) deleted
/// it — eval M3-2/M3-3 (09 §4.3's Recall@10 >= 0.8 gate) then measured 13/14
/// failures traced to exactly this dropped equivalence (query "3600" vs.
/// corpus "3,600").
pub(crate) fn numeric_equivalent_form(token: &str) -> Option<String> {
    if token.bytes().all(|b| b.is_ascii_digit()) {
        return (token.len() >= 4).then(|| thousands_separated(token));
    }
    if is_well_formed_grouped_numeral(token) {
        let digits: String = token.chars().filter(|ch| *ch != ',').collect();
        if digits.len() >= 4 {
            return Some(digits);
        }
    }
    None
}

/// A comma-grouped ASCII numeral in standard thousands form: 1-3 leading
/// digits, then one or more groups of EXACTLY 3 digits, each separated by a
/// single comma (`3,600` / `30,000` / `1,234,567`). Anything else (a bare
/// digit run with no comma at all, a group that isn't exactly 3 digits, a
/// leading/trailing/doubled comma) is not a numeral this rule recognizes —
/// returning `false` leaves `token` untouched rather than risk mangling it
/// into a bogus phrase.
fn is_well_formed_grouped_numeral(token: &str) -> bool {
    let mut groups = token.split(',');
    let Some(first) = groups.next() else {
        return false;
    };
    if first.is_empty() || first.len() > 3 || !first.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let mut saw_group = false;
    for group in groups {
        saw_group = true;
        if group.len() != 3 || !group.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    saw_group
}

/// The pre-PC8 grouping algorithm, recovered unchanged
/// (`git show e3f2a94^:crates/kio-cli/src/main.rs`): insert a comma every 3
/// digits counting from the right. `digits` must already be all-ASCII-digit
/// (callers only ever pass a token `numeric_equivalent_form` has already
/// classified as such).
fn thousands_separated(digits: &str) -> String {
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*byte as char);
    }
    out
}

/// Deterministic query normalization (05 §1.3 L116-123, 2026-07-22 spec
/// feedback #1): Kio's own fixed, release-pinned bilingual vocabulary —
/// recovered unchanged from the pre-PC8 `fts_keyword_expansions`
/// (`git show e3f2a94^:crates/kio-cli/src/main.rs`), which this restores.
/// Forward direction only (English keyword -> Japanese term, matching the
/// recovered content exactly): eval M3-2's failing query "chunk size was
/// 512 tokens in the retrieval pipeline doc" must reach a chunk whose only
/// text is "チャンクは 512 トークン、オーバーラップ 64。" — the direction the
/// recovered dictionary already provides. A Japanese-term -> English reverse
/// lookup was considered and deliberately left out: no eval query needs it,
/// and it would touch every existing "トークン"-only query fixture across
/// `step3_p0_contract.rs` (~80 occurrences) for zero Recall benefit.
const BILINGUAL_TERMS: &[(&str, &str)] = &[
    ("chunk", "チャンク"),
    ("chunks", "チャンク"),
    ("token", "トークン"),
    ("tokens", "トークン"),
    ("pipeline", "パイプライン"),
];

/// `token`'s fixed-dictionary translation(s), case-insensitive on the ASCII
/// side (matching `fts_keyword_expansions`'s original comparison). Empty
/// when `token` isn't one of `BILINGUAL_TERMS`'s English keys.
fn bilingual_equivalents(token: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    for (en, ja) in BILINGUAL_TERMS {
        if token.eq_ignore_ascii_case(en) && !out.contains(ja) {
            out.push(*ja);
        }
    }
    out
}

/// `token` together with every deterministic equivalence form it has (05
/// §1.3 L116-123): a >= 4 digit numeral's thousands-grouped twin
/// (`numeric_equivalent_form`) and/or a fixed-dictionary translation
/// (`bilingual_equivalents`). Always returns >= 1 element (`token` itself,
/// first — stable order, so the generated SQL text never depends on
/// iteration order) so a caller can treat "one form" and "several forms"
/// uniformly. These are the SAME forms injected on both sides the spec
/// names: the FTS5 MATCH expression (`build_query_plan`, below) and the
/// short-token eligibility predicate — one
/// rule, two call sites. A token can match at most one of the two rules in
/// practice (a numeral is never a `BILINGUAL_TERMS` key), but the function
/// stays generic rather than assuming that.
pub(crate) fn token_equivalence_forms(token: &str) -> Vec<String> {
    let mut forms = vec![token.to_owned()];
    if let Some(numeric) = numeric_equivalent_form(token) {
        forms.push(numeric);
    }
    forms.extend(bilingual_equivalents(token).into_iter().map(str::to_owned));
    forms
}

/// PC8 (05 §1.3 L110-115) + deterministic query normalization (L116-123,
/// 2026-07-22 spec feedback #1) + deterministic script-boundary segmentation
/// (L120-134, 2026-07-22 spec feedback #2): machine-generate the MATCH
/// expression — never interpret the query as FTS5 syntax (every form is
/// quoted as an inert phrase via [`quote_fts_phrase`]; FTS5 operators the
/// user typed are therefore literal query text, not directives) and never
/// inject a word with no fixed, deterministic derivation from the input
/// (PC8's "query 由来でない追加語を含まない" bans GUESSED words — synonym
/// injection, history, context — not a token's own numeral/dictionary
/// equivalence form or script-boundary sub-piece, both of which the spec
/// names explicitly as query-derived).
///
/// Unit generation (feedback #2): each whitespace token ([`query_tokens`])
/// contributes itself FIRST, then its [`segment_script_runs`] pieces —
/// preserving the token's own exact-phrase signal (`C++` stays matchable as
/// `C++`) while the segmented pieces absorb agglutinated particles (query
/// `スコープが` vs. corpus `スコープは`) and symbol-joined runs (query
/// `read/write/admin` vs. corpus `read / write / admin`). Every token's
/// candidates are concatenated in query order, then deduplicated keeping
/// each string's first occurrence — a token whose segmentation reproduces
/// itself exactly (e.g. `RAG`, one uniform Alnum run) contributes no
/// separate duplicate entry, since that second, identical insertion attempt
/// is simply absorbed by this same first-occurrence dedup (equivalent to,
/// and simpler than, special-casing "segmentation == original token" up
/// front). Each resulting unit then gets its own `token_equivalence_forms`
/// — segmentation runs BEFORE equivalence expansion, never the reverse
/// ("同値形は再細分しない").
///
/// long/short is decided PER UNIT (>= 3 Unicode scalars, PC9's trigram-MATCH
/// floor), not per original token:
/// - MIXED query (>= 1 long unit): every long unit — plus its own
///   equivalence forms — becomes an OR arm of the single MATCH expression
///   (OR being associative, this flat per-unit expansion is equivalent to,
///   and simpler than, wrapping each unit's own forms in their own
///   parenthesized group). Every short unit is dropped OUTRIGHT:
///   `short_tokens` comes back empty, so the caller's bounded `instr`
///   eligibility predicate never fires for this
///   query at all (05 §1.3 2026-07-22 feedback #2 — the AND-filter this
///   superseded excluded documents that never spelled a natural-sentence
///   particle/function word, eval M3-2/M3-3's dominant Recall@10 failure
///   mode).
/// - PURE-SHORT query (every unit < 3 Unicode scalars — equivalently, every
///   ORIGINAL token is < 3, since no segmented piece can ever be longer than
///   its own token): `match_expr` stays `None` (PC11's full bounded-LIKE
///   fallback) and `short_tokens` is the RAW token list from
///   [`query_tokens`] — NOT the segmented/deduplicated unit list — exactly
///   the pre-feedback-#2 behavior (PC14's deterministic LIKE-fallback
///   ordering is unaffected).
pub(crate) fn build_query_plan(query_nfc: &str) -> QueryPlan {
    let tokens = query_tokens(query_nfc);

    let mut units: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for token in &tokens {
        let mut candidates = vec![token.clone()];
        candidates.extend(segment_script_runs(token));
        for candidate in candidates {
            if seen.insert(candidate.clone()) {
                units.push(candidate);
            }
        }
    }

    let long_units: Vec<String> = units
        .into_iter()
        .filter(|unit| unit.chars().count() >= 3)
        .collect();

    if long_units.is_empty() {
        // Pure-short query: every unit (and therefore every original token)
        // is short. Preserve the pre-feedback-#2 behavior, EXCEPT for R23-28's
        // first-occurrence dedup (05 §1.3 L129-131 "全 unit 集合から重複を除く"
        // applies here too — a pure-short query was never exempted, but the
        // pre-feedback-#2 code never deduped this branch). A query repeating
        // one short token thousands of times used to generate one SQLite bind
        // parameter per repetition in the replica's short-token predicate,
        // risking the SQLite bound-variable limit. Dedup preserves
        // first-occurrence order, so PC14's "first token" ORDER BY tie-break
        // rule is unaffected — the first token survives dedup unchanged.
        let mut deduped_tokens: Vec<String> = Vec::with_capacity(tokens.len());
        let mut seen_tokens: BTreeSet<String> = BTreeSet::new();
        for token in tokens {
            if seen_tokens.insert(token.clone()) {
                deduped_tokens.push(token);
            }
        }
        return QueryPlan {
            match_expr: None,
            short_tokens: deduped_tokens,
        };
    }

    let match_expr = Some(
        long_units
            .iter()
            .flat_map(|unit| token_equivalence_forms(unit))
            .map(|form| quote_fts_phrase(&form))
            .collect::<Vec<_>>()
            .join(" OR "),
    );
    QueryPlan {
        match_expr,
        // Mixed query: short units are fully discarded (see doc comment
        // above) — never fed to the AND-instr eligibility predicate.
        short_tokens: Vec::new(),
    }
}

pub(crate) fn scope_selection_from_cursor(mode: ScopeMode) -> ScopeSelectionMode {
    match mode {
        ScopeMode::All => ScopeSelectionMode::All,
        ScopeMode::Scope => ScopeSelectionMode::Scope,
        ScopeMode::Descendants => ScopeSelectionMode::Descendants,
    }
}

pub(crate) fn selector_from_cursor(selector: &TimeTravelSelector) -> Result<TimeSelector> {
    TimeSelectorFlags {
        at: selector.at.clone(),
        all_history: selector.all_history,
        include_deleted: selector.include_deleted,
        since: selector.since.clone(),
    }
    .canonicalize()
}

pub(crate) fn selector_for_search(selector: &TimeSelector) -> TimeTravelSelector {
    TimeTravelSelector {
        at: selector.at().map(str::to_owned),
        all_history: selector.all_history(),
        include_deleted: selector.include_deleted(),
        since: selector.since().map(|duration| duration.canonical()),
    }
}

pub(crate) fn cursor_mode_from_selection(mode: ScopeSelectionMode) -> ScopeMode {
    match mode {
        ScopeSelectionMode::All => ScopeMode::All,
        ScopeSelectionMode::Scope => ScopeMode::Scope,
        ScopeSelectionMode::Descendants => ScopeMode::Descendants,
    }
}

/// Resolve a cursor's frozen per-scope sub-cursors into execution scopes. The
/// cursor scope set is authoritative (05 §1.8): scope_id -> path is resolved via
/// the registry. A scope_id the registry can no longer resolve is returned as an
/// `excluded_scopes` entry (reason `unreachable`) rather than dropped silently —
/// the replay then follows partial-failure semantics (exit 3, CT3-MULTI-005),
/// never a misleading cursor-mismatch error.
pub(crate) fn resolve_cursor_exec_scopes(
    ctx: &Invocation<'_>,
    cursor: &CursorToken,
) -> Result<(Vec<ExecScope>, Vec<Value>)> {
    let mut exec = Vec::new();
    let excluded = cursor
        .excluded_scopes
        .iter()
        .map(|scope| {
            json!({
                "scope_id": scope.scope_id,
                "scope_path": Value::Null,
                "reason": scope.reason,
            })
        })
        .collect::<Vec<_>>();
    for sub in &cursor.scopes {
        // O7: resolve through the shared registry resolver so a scope_id that a
        // `.kio` copy made ambiguous is reported KIO-E-REGISTRY-DUP-001
        // (like Evidence), not silently pinned to whichever row sorted first.
        match resolve_scope_id_in_registry(ctx, &sub.scope_id)? {
            Some(target) => exec.push(ExecScope {
                target,
                snapshot_commit: Some(sub.snapshot_commit.clone()),
                max_rowid: Some(sub.max_rowid),
                max_association_rowid: Some(sub.max_association_rowid),
                chunking_config_hash: Some(sub.chunking_config_hash.clone()),
                index_generation: Some(sub.index_generation.clone()),
                from_cursor: true,
            }),
            None => {
                return Err(KioError::new(
                    "KIO-E-SEARCH-CURSOR-001",
                    "search cursor active scope is no longer reachable; re-run without a cursor",
                    json!({
                        "reason": "active_scope_unavailable",
                        "scope_id": sub.scope_id,
                    }),
                    ExitCode::InvalidUsage,
                ));
            }
        }
    }
    Ok((exec, excluded))
}

/// Fold [`SearchArgs`] into the shape the search body consumes.
///
/// B (2026-07-24): this replaces a ~200-line hand-rolled flag parser. clap now
/// owns operand shape, `--flag=value` handling, mutual exclusion between the
/// mode shorthands, and the "inline value on a boolean flag is a usage error"
/// rule (R16-6) via `ArgAction::SetTrue`. The value-range checks that are not
/// expressible as clap attributes stay here.
///
/// One deliberate behavior change: a repeated flag is now last-wins (booleans
/// are idempotent) instead of a "may be specified once" usage error. That
/// matches every clap-declared command in this CLI — `kio index --online
/// --online` has always been accepted — so this removes an inconsistency
/// rather than introducing one.
pub(crate) fn parsed_search(args: SearchArgs) -> Result<ParsedSearch> {
    let Some(query) = args.query else {
        return Err(KioError::invalid_usage("search query is required"));
    };
    let (requested_mode, explicit_mode) = match args.mode {
        Some(SearchModeArg::Auto) => (SearchMode::Auto, true),
        Some(SearchModeArg::Text) => (SearchMode::Text, true),
        Some(SearchModeArg::Vector) => (SearchMode::Vector, true),
        Some(SearchModeArg::Hybrid) => (SearchMode::Hybrid, true),
        None => (SearchMode::Auto, false),
    };
    // R12-7: `--limit 0` is a meaningless value, not a silent clamp-to-1. The
    // upper cap of 100 is unchanged.
    if args.limit == 0 {
        return Err(KioError::invalid_usage("--limit must be at least 1"));
    }
    let time_selector_flags = TimeSelectorFlags {
        at: args.at,
        all_history: args.all_history,
        include_deleted: args.include_deleted,
        since: args.since,
    };
    // Validate selector exclusivity/duration before repository or DB access.
    time_selector_flags.canonicalize()?;
    // PC59/PC60 (06 §3): `--at` needs a single, non-`--descendants` `--scope`
    // — an explicit commit cannot be resolved against more than one
    // independent scope DAG (05 §1.6). Checked here (usage-level, before any
    // repository/registry access) rather than after scope enumeration, so it
    // is a uniform `KIO-E-CONFIG-USAGE-001`/exit 2 regardless of what is or
    // is not registered.
    if time_selector_flags.at.is_some() && (args.scope.is_none() || args.descendants) {
        return Err(KioError::invalid_usage(
            "--at requires a single --scope <path> (without --descendants); \
             multi-scope search cannot resolve one commit across independent scope DAGs",
        ));
    }
    Ok(ParsedSearch {
        query,
        requested_mode,
        explicit_mode,
        scope: args.scope,
        descendants: args.descendants,
        all_scopes: args.all_scopes,
        limit: args.limit.min(100),
        offset: args.offset,
        cursor: args.cursor,
        time_selector_flags,
        online: args.online,
        offline: args.offline,
    })
}

/// R11-7: `[search].default_mode` / `fail_behavior` from one config file
/// (config.schema.json §search). Both are independent and optional; an unknown or
/// absent value yields `None` (the config is schema-validated at startup, so an
/// out-of-enum value is already rejected before search runs).
fn read_search_config(path: &Path) -> Result<(Option<SearchMode>, Option<SearchFailBehavior>)> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok((None, None));
    };
    let value: toml::Value =
        toml::from_str(&text).map_err(|err| KioError::schema(err.to_string()))?;
    let search = value.get("search");
    let default_mode = search
        .and_then(|section| section.get("default_mode"))
        .and_then(toml::Value::as_str)
        .and_then(parse_search_mode_name);
    let fail_behavior = search
        .and_then(|section| section.get("fail_behavior"))
        .and_then(toml::Value::as_str)
        .and_then(parse_fail_behavior_name);
    Ok((default_mode, fail_behavior))
}

/// PC49/PC50 (05 §1.8 L384-387): effective `[search]` settings. A folder
/// config.toml wins over the user (device) config PER KEY only when
/// `single_scope_kio_dir` is `Some` — the search is a single, non-
/// `--descendants` `--scope <path>` (that scope's own folder value, which may
/// differ from the CWD's — PC50's `--scope /work/other` case). For every
/// multi-scope search (default / `--all-scopes` / `--descendants`),
/// `single_scope_kio_dir` is `None` and only the user (device) layer is ever
/// consulted — "scope 間で異なる folder 値の統合は定義しない". Multi-scope
/// execution settings ([search.multi_scope] itself) are a separate namespace,
/// resolved by the focused `multi_scope` module regardless of this rule.
pub(crate) fn effective_search_config(
    single_scope_kio_dir: Option<&Path>,
    control: &ControlHandle,
) -> Result<(Option<SearchMode>, Option<SearchFailBehavior>)> {
    let (scope_mode, scope_fail) = match single_scope_kio_dir {
        Some(kio_dir) => read_search_config(&kio_dir.join("config.toml"))?,
        None => (None, None),
    };
    let (user_mode, user_fail) = read_search_config(&user_config_toml_path(control))?;
    Ok((scope_mode.or(user_mode), scope_fail.or(user_fail)))
}

fn parse_search_mode_name(name: &str) -> Option<SearchMode> {
    match name {
        "auto" => Some(SearchMode::Auto),
        "text" => Some(SearchMode::Text),
        "vector" => Some(SearchMode::Vector),
        "hybrid" => Some(SearchMode::Hybrid),
        _ => None,
    }
}

/// R12-1: the tuning half of `[search]` parsed from ONE config file. Every key is
/// independent and optional (schema-validated at repo open, so an out-of-range /
/// unknown key is already rejected). `None` means "not set in this file".
#[derive(Default)]
pub(crate) struct SearchTuning {
    pub(crate) rrf_k: Option<u64>,
    pub(crate) rrf_w_text: Option<f64>,
    pub(crate) rrf_w_vector: Option<f64>,
    pub(crate) rrf_candidate_depth: Option<u64>,
    pub(crate) div_enabled: Option<bool>,
    pub(crate) div_strategy: Option<DiversifyStrategy>,
    div_mmr_lambda: Option<f64>,
    pub(crate) div_max_per_raw_hash: Option<u64>,
    div_mmr_depth: Option<u64>,
}

pub(crate) fn toml_u64(value: Option<&toml::Value>) -> Option<u64> {
    value
        .and_then(toml::Value::as_integer)
        .and_then(|integer| u64::try_from(integer).ok())
}

/// TOML distinguishes `1` (integer) from `1.0` (float); the schema types these as
/// `number`, so accept either as f64.
pub(crate) fn toml_f64(value: Option<&toml::Value>) -> Option<f64> {
    value.and_then(|value| {
        value
            .as_float()
            .or_else(|| value.as_integer().map(|integer| integer as f64))
    })
}

fn parse_diversify_strategy(name: &str) -> Option<DiversifyStrategy> {
    match name {
        "mmr" => Some(DiversifyStrategy::Mmr),
        "group_by_raw_hash" => Some(DiversifyStrategy::GroupByRawHash),
        "off" => Some(DiversifyStrategy::Off),
        _ => None,
    }
}

pub(crate) fn read_search_tuning(path: &Path) -> Result<SearchTuning> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(SearchTuning::default());
    };
    let value: toml::Value =
        toml::from_str(&text).map_err(|err| KioError::schema(err.to_string()))?;
    let search = value.get("search");
    let rrf = search.and_then(|section| section.get("rrf"));
    let diversify = search.and_then(|section| section.get("diversify"));
    Ok(SearchTuning {
        rrf_k: toml_u64(rrf.and_then(|rrf| rrf.get("k"))),
        rrf_w_text: toml_f64(rrf.and_then(|rrf| rrf.get("w_text"))),
        rrf_w_vector: toml_f64(rrf.and_then(|rrf| rrf.get("w_vector"))),
        rrf_candidate_depth: toml_u64(rrf.and_then(|rrf| rrf.get("candidate_depth"))),
        div_enabled: diversify
            .and_then(|diversify| diversify.get("enabled"))
            .and_then(toml::Value::as_bool),
        div_strategy: diversify
            .and_then(|diversify| diversify.get("strategy"))
            .and_then(toml::Value::as_str)
            .and_then(parse_diversify_strategy),
        div_mmr_lambda: toml_f64(diversify.and_then(|diversify| diversify.get("mmr_lambda"))),
        div_max_per_raw_hash: toml_u64(
            diversify.and_then(|diversify| diversify.get("max_per_raw_hash")),
        ),
        div_mmr_depth: toml_u64(diversify.and_then(|diversify| diversify.get("mmr_depth"))),
    })
}

/// R12-1 / PC49/PC50: effective `[search.rrf]` + `[search.diversify]` (05
/// §1.3/§1.4/§1.8). Same `single_scope_kio_dir` rule as
/// [`effective_search_config`] — `None` (multi-scope) consults only the user
/// (device) layer; `Some` additionally lets that one scope's folder value win
/// per key. These feed BOTH the ranking/dedup call sites AND the cursor
/// `query_hash` (05 §1.8 requires the effective values, so a tuning change
/// invalidates a stale cursor).
pub(crate) fn effective_search_tuning(
    single_scope_kio_dir: Option<&Path>,
    control: &ControlHandle,
) -> Result<(RrfConfig, DiversifyRequest)> {
    let scope = match single_scope_kio_dir {
        Some(kio_dir) => read_search_tuning(&kio_dir.join("config.toml"))?,
        None => SearchTuning::default(),
    };
    let user = read_search_tuning(&user_config_toml_path(control))?;
    let rrf = RrfConfig {
        k: scope.rrf_k.or(user.rrf_k).unwrap_or(60),
        w_text: scope.rrf_w_text.or(user.rrf_w_text).unwrap_or(1.0),
        w_vector: scope.rrf_w_vector.or(user.rrf_w_vector).unwrap_or(1.0),
        candidate_depth: scope
            .rrf_candidate_depth
            .or(user.rrf_candidate_depth)
            .unwrap_or(200),
    };
    // `enabled = false` means diversification is off entirely (05 §1.4); it maps to
    // the Off strategy, which is a TRUE no-op (no MMR, no dedup — R12-1). Otherwise
    // use the configured strategy, default MMR.
    let enabled = scope.div_enabled.or(user.div_enabled).unwrap_or(true);
    let strategy = if enabled {
        scope
            .div_strategy
            .or(user.div_strategy)
            .unwrap_or(DiversifyStrategy::Mmr)
    } else {
        DiversifyStrategy::Off
    };
    let diversify = DiversifyRequest {
        strategy,
        mmr_lambda: Some(scope.div_mmr_lambda.or(user.div_mmr_lambda).unwrap_or(0.7)),
        max_per_raw_hash: Some(
            scope
                .div_max_per_raw_hash
                .or(user.div_max_per_raw_hash)
                .unwrap_or(3),
        ),
        mmr_depth: Some(scope.div_mmr_depth.or(user.div_mmr_depth).unwrap_or(100)),
    };
    Ok((rrf, diversify))
}

fn parse_fail_behavior_name(name: &str) -> Option<SearchFailBehavior> {
    match name {
        "fallback" => Some(SearchFailBehavior::Fallback),
        "warn" => Some(SearchFailBehavior::Warn),
        "error" => Some(SearchFailBehavior::Error),
        _ => None,
    }
}

/// Materialize the live chunks of a scope for local Evidence Pointer short-hash
/// resolution. This is not a `kio search` ranking or candidate path: direct
/// search obtains both from the device-level replica.
pub(crate) fn load_searchable_chunks(
    target: &ScopeTarget,
    control: &ControlHandle,
    locks: &kio_core::scope::LockSession,
) -> Result<Vec<SearchableChunk>> {
    let repo = Repository::open_in_session(&target.repo_root, control.clone(), locks)?;
    let head = repo.head_commit_hash()?.unwrap_or_default();
    if head.is_empty() {
        return Ok(Vec::new());
    }
    // A bare `kio snapshot` can advance HEAD before the source cache has a
    // `tree_entries` projection for that commit. This local pointer-resolution
    // command materializes it before inspecting source rows. It is deliberately
    // separate from `kio search`, which never repairs or reads source SQLite.
    let db_path = sqlite_path(&target.kio_dir);
    if !db_path.exists() {
        // R23-33 (06 §7 L345-346, same class as R23-14(b)): a missing
        // sqlite.db on the short-hash/pointer resolution path is "the index
        // needs (re)building — retry", never a silent empty set that the
        // caller then misreports as "not found".
        return Err(KioError::new(
            "KIO-E-INDEX-REBUILDING-001",
            "this scope's projection in the device search replica is missing or being rebuilt; \
             retry, or run `kio repair replica` (or `kio repair all`) if the problem persists",
            json!({ "scope_path": target.kio_dir.display().to_string() }),
            ExitCode::PartialFailure,
        ));
    }
    let conn = open_existing_source_index_connection(
        &db_path,
        ExistingSourceIndexOpenMode::ReadWrite,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    ensure_snapshot_tree_entries(&repo, &conn, &head)?;
    let live = live_tree_entries_at(&conn, &head)?;
    Ok(read_stored_chunks(&target.kio_dir)?
        .into_iter()
        .filter_map(|stored| {
            let key = (
                stored.row.raw_hash.clone(),
                stored.row.tool_profile_hash.clone(),
                stored.row.r#gen,
            );
            live.get(&key).map(|path| SearchableChunk {
                row: stored.row,
                scope_id: target.scope_id.clone(),
                scope_path: target.repo_root.clone(),
                snapshot_at: head.clone(),
                path_at_commit: path.clone(),
            })
        })
        .collect())
}

/// Live tree-entry map for `head` read from SQLite: (raw_hash, tool_profile_hash,
/// gen) -> path_at_commit. Rows without a `tool_profile_hash` (raw-only) carry no
/// chunk and are skipped.
fn live_tree_entries_at(
    conn: &Connection,
    head: &str,
) -> Result<BTreeMap<(String, String, u64), String>> {
    let mut stmt = conn
        .prepare(
            "SELECT raw_hash, tool_profile_hash, gen, path
             FROM tree_entries WHERE commit_hash = ?1",
        )
        .map_err(|err| KioError::schema(err.to_string()))?;
    let rows = stmt
        .query_map(rusqlite::params![head], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|err| KioError::schema(err.to_string()))?;
    let mut map = BTreeMap::new();
    for row in rows {
        let (raw_hash, tool_profile_hash, r#gen, path) =
            row.map_err(|err| KioError::schema(err.to_string()))?;
        if let (Some(tool), Some(r#gen)) = (tool_profile_hash, r#gen) {
            map.insert((raw_hash, tool, r#gen as u64), path);
        }
    }
    Ok(map)
}

pub(crate) fn search_mode_json(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::Auto => "auto",
        SearchMode::Text => "text",
        SearchMode::Vector => "vector",
        SearchMode::Hybrid => "hybrid",
    }
}

/// PC24/PC25 (05 §1.5): `sha256(canonical query vector bytes)` — the same
/// little-endian float32 canonical form `chunk_vec` stores vectors in
/// (`f32_to_le_bytes`), so this digest is stable across device-local cache
/// reads.
pub(crate) fn query_vector_digest_hex(vector: &[f32]) -> String {
    let bytes = f32_to_le_bytes(vector);
    let digest = Sha256::digest(&bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("sha256:{hex}")
}

/// R23-01 (05 §1.5 L234 "vector / hybrid の replay は page 1 の query vector
/// を再利用する — query の再 embedding は行わない"): the device-local cache
/// directory for page-1 query vectors, keyed by [`query_vector_digest_hex`]'s
/// digest. It is digest-keyed and reverified on every read; a missing or
/// corrupt entry fails closed to `KIO-E-SEARCH-CURSOR-001` rather than
/// falling back to re-embedding. The device-local cache does not touch a
/// scope's `sqlite.db` schema.
fn query_vector_cache_path(digest: &str, control: &ControlHandle) -> PathBuf {
    cache_home(control)
        .join("kio/search-query-cache")
        .join(digest.trim_start_matches("sha256:"))
}

/// Persists page 1's query vector so a later cursor replay can reuse it
/// (never re-embed, R23-01). Best-effort by design: a write failure here
/// must not fail page 1 itself (the cache is a pure optimization/durability
/// aid for a FUTURE page) — a subsequent replay that finds nothing simply
/// fails closed with `KIO-E-SEARCH-CURSOR-001` (honest and retryable via a
/// fresh search), never silently re-sending the query.
pub(crate) fn persist_query_vector_cache(digest: &str, vector: &[f32], control: &ControlHandle) {
    let path = query_vector_cache_path(digest, control);
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    // R9-3-style hardening (mirrors `open_cache_path`'s tree): this cache
    // holds derived query-vector bytes, not secrets, but the containing
    // `$XDG_CACHE_HOME/kio` subtree convention is owner-only throughout.
    harden_open_cache_subtree(parent, control);
    let _ = write_open_cache_atomic(&path, &f32_to_le_bytes(vector));
}

/// R23-01: cursor replay's ONLY path to a query vector — reads page 1's
/// vector back from its device-local cache file, re-verifying the digest on
/// every read (an externally-modified or torn cache leaf must never be
/// trusted, mirroring the CAS open-cache's own M5 reuse posture — this
/// cache has no CAS backing to fsck, so a mismatch is handled by deleting
/// the untrustworthy leaf here instead). Missing or mismatched is
/// `KIO-E-SEARCH-CURSOR-001`, matching every other cursor-invalidity cause
/// in this module (05 §1.5's own text: "欠落・不一致は... 同じく
/// KIO-E-SEARCH-CURSOR-001"). Deliberately has **no** fallback to
/// `run_embedding_adapter`/`compute_query_embedding`: a page 2+ that resent
/// the query would bypass the sync-embedding cost-ledger guardrail entirely
/// (04 §5.4) and could return a DIFFERENT vector than page 1 used
/// (non-deterministic providers), corrupting the `consumed`-skip pagination
/// contract this cache exists to protect.
pub(crate) fn replay_query_vector(digest: &str, control: &ControlHandle) -> Result<Vec<f32>> {
    let path = query_vector_cache_path(digest, control);
    let bytes = fs::read(&path).map_err(|_| cursor_query_vector_error())?;
    let vector = f32_from_le_bytes(&bytes);
    if query_vector_digest_hex(&vector) != digest {
        let _ = fs::remove_file(&path);
        return Err(cursor_query_vector_error());
    }
    Ok(vector)
}

pub(crate) fn cursor_query_vector_error() -> KioError {
    KioError::new(
        "KIO-E-SEARCH-CURSOR-001",
        "search cursor's page-1 query vector is missing or no longer verifiable; re-run the \
         search without a cursor",
        json!({ "reason": "query_vector_unavailable" }),
        ExitCode::InvalidUsage,
    )
}

/// R12-5: observability logging must never break the search result. A metrics.jsonl
/// or access.jsonl append failure (read-only file, disk full — both device-global,
/// so one bad file would otherwise stop EVERY scope's search with exit 1 and discard
/// the results it had already computed) is downgraded to a stderr warning; the
/// result still returns. This makes the search logs symmetric with errors.jsonl,
/// which already ignores its own write failure (`let _ =` in main()). Success and
/// failure of the append are otherwise identical, so this returns `()`.
pub(crate) fn append_search_logs(
    ctx: &Invocation<'_>,
    repo: &Repository,
    response: &Value,
    started: Instant,
) {
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    let scope_count = response
        .get("searched_scopes")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let result_count = response
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    // Per-search latency record (05 §7, K8a): code KIO-M-SEARCH-001 / component
    // "search" / metric search.latency_ms. redact_logs default omits query & path.
    if let Err(error) = append_jsonl_cli(
        ctx.locks,
        &data_home(ctx.control).join("kio/logs/metrics.jsonl"),
        &json!({
            "ts": ctx.control.now_utc_seconds(),
            "level": "info",
            "code": "KIO-M-SEARCH-001",
            "component": "search",
            "message": "search completed",
            "metric": "search.latency_ms",
            "value": latency_ms,
            "context": {
                "mode": response.get("resolved_mode").and_then(Value::as_str).unwrap_or("text"),
                "scope_count": scope_count,
                "result_count": result_count,
            },
        }),
        device_logs_retention_days(ctx.control),
        ctx.control,
    ) {
        ctx.interaction.diagnostic(&format!(
            "warning: failed to append search metrics log: {}",
            diagnostic_safe_text(&error.to_string(), false)
        ));
    }
    // R12-2: access.jsonl is scope-local, so its redact_logs is governed by the
    // scope config first, falling back to the device-global user config, then the
    // secure default (true). Under redaction (the default) the query is masked;
    // with an explicit `redact_logs = false` the real query text is recorded.
    let query_field = if access_log_redact(repo) {
        json!("[redacted]")
    } else {
        response
            .get("query")
            .cloned()
            .unwrap_or_else(|| json!("[redacted]"))
    };
    if let Err(error) = append_jsonl_cli(
        ctx.locks,
        &repo.kio_dir().join("logs/access.jsonl"),
        &json!({
            "ts": ctx.control.now_utc_seconds(),
            "level": "info",
            "code": "KIO-I-SEARCH-ACCESS-001",
            "component": "kio-cli",
            "message": "search access",
            "context": {
                "query": query_field,
                "mode": response.get("resolved_mode").and_then(Value::as_str).unwrap_or("text"),
                "result_count": result_count,
            },
        }),
        scope_logs_retention_days(repo),
        ctx.control,
    ) {
        ctx.interaction.diagnostic(&format!(
            "warning: failed to append search access log: {}",
            diagnostic_safe_text(&error.to_string(), false)
        ));
    }
}

/// R12-4: the per-search metrics.jsonl line for a FAILED search (result_count 0 +
/// the failure's error_code). Best-effort — a metrics write failure must never turn
/// a search error into a different one (R12-5), so the result is discarded.
pub(crate) fn append_failed_search_metrics(
    started: Instant,
    error: &KioError,
    control: &ControlHandle,
    session: &kio_core::scope::LockSession,
) {
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    let _ = append_jsonl_cli(
        session,
        &data_home(control).join("kio/logs/metrics.jsonl"),
        &json!({
            "ts": control.now_utc_seconds(),
            "level": "info",
            "code": "KIO-M-SEARCH-001",
            "component": "search",
            "message": "search failed",
            "metric": "search.latency_ms",
            "value": latency_ms,
            "context": {
                "result_count": 0,
                "error_code": error.error_code(),
            },
        }),
        device_logs_retention_days(control),
        control,
    );
}

/// R12-2: effective `redact_logs` for the scope-local `access.jsonl`. The scope
/// config wins over the user config (the log lives in `.kio/`, and 07 §7 attributes
/// `[adapter.policy]` to `.kio/config.toml`); the device-global events/metrics/
/// errors logs stay user-config-governed via `redact_logs_enabled` in kio-core.
/// Absent everywhere → the secure default (true).
fn access_log_redact(repo: &Repository) -> bool {
    read_redact_logs_config(&repo.kio_dir().join("config.toml"))
        .or_else(|| read_redact_logs_config(&user_config_toml_path(repo.control())))
        .unwrap_or(true)
}

fn read_redact_logs_config(path: &Path) -> Option<bool> {
    let text = fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("adapter")
        .and_then(|adapter| adapter.get("policy"))
        .and_then(|policy| policy.get("redact_logs"))
        .and_then(toml::Value::as_bool)
}
