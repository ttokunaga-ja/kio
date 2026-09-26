//! Embedding metadata and chunk_vec store contracts.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

use crate::fts::CHUNK_VEC_DIMENSIONS;
use crate::{EmbeddingDistance, EmbeddingModality, EmbeddingTargetType, IndexError, Result};

pub fn embedding_hash(
    target_type: EmbeddingTargetType,
    target_hash: &str,
    dimensions: u64,
    distance: EmbeddingDistance,
    modality: EmbeddingModality,
    profile_hash: &str,
    // 2026-07-24 (07 §5.3 contextual-embedding addendum): the humanized filename
    // context prepended to a chunk's embedding INPUT (`chunk_embedding_context`).
    // A chunk's vector is now a function of `(text_hash, context, profile)`, not
    // `text_hash` alone, so the content-addressed identity must fold the context
    // in — else two chunks with identical bodies but different filenames collide
    // on `id` and share one (wrong) vector. `None` omits the key ENTIRELY (not a
    // JSON null), so every pre-2026-07-24 caller (non-chunk targets, the frozen
    // CT3-EMBED-001 vector) hashes byte-for-byte as before.
    context: Option<&str>,
) -> Result<String> {
    let mut value = json!({
        "dimensions": dimensions,
        "distance": distance_name(distance),
        "modality": modality_name(modality),
        "profile_hash": profile_hash,
        "spec_version": 1,
        "target_hash": target_hash,
        "target_type": target_type_name(target_type),
    });
    if let Some(context) = context {
        value
            .as_object_mut()
            .expect("embedding_hash value is always a JSON object")
            .insert("context".to_owned(), json!(context));
    }
    hash_jcs(&value)
}

/// The humanized filename context prepended to a chunk's embedding input
/// (07 §5.3 contextual-embedding addendum, 2026-07-24). A file's own name is
/// user-authored intent (`recovery-window.md`, `control-coverage.png`); folding
/// it into the embedded text pulls the chunk's vector toward the document it
/// belongs to rather than leaving a thin 1-2 sentence body to fend for itself
/// in embedding space. Deterministic transform of the basename stem:
/// `-`/`_` → space, an ASCII camelCase boundary (`aA`/`9A`) → space, runs of
/// whitespace collapsed. Returns `None` when the stem carries no alphanumeric
/// character (a purely symbolic name), so such a chunk is embedded exactly as
/// before with no prefix and a byte-identical `embedding_hash`.
#[must_use]
pub fn chunk_embedding_context(raw_path: &str) -> Option<String> {
    let stem = std::path::Path::new(raw_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    let chars: Vec<char> = stem.chars().collect();
    let mut spaced = String::with_capacity(stem.len() + 4);
    for (index, &ch) in chars.iter().enumerate() {
        if ch == '-' || ch == '_' {
            spaced.push(' ');
            continue;
        }
        if ch.is_ascii_uppercase()
            && index > 0
            && (chars[index - 1].is_ascii_lowercase() || chars[index - 1].is_ascii_digit())
        {
            spaced.push(' ');
        }
        spaced.push(ch);
    }
    let collapsed = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .chars()
        .any(char::is_alphanumeric)
        .then_some(collapsed)
}

/// The exact text handed to the Embedding Adapter for a chunk: the humanized
/// filename `context` (if any) prepended to the chunk body, separated by a
/// blank line. Single source of truth so the online send path and any later
/// rebuild-time re-embed produce byte-identical adapter inputs — and therefore
/// identical vectors — for the same `(context, text)`.
#[must_use]
pub fn contextualized_embedding_input(context: Option<&str>, text: &str) -> String {
    match context {
        Some(context) => format!("{context}\n\n{text}"),
        None => text.to_owned(),
    }
}

/// A distinct embedding profile observed in the `embeddings` table (compat check
/// input, 03 §7).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EmbeddingProfileSummary {
    pub dimensions: u64,
    pub distance: String,
    pub modality: String,
    pub profile_hash: String,
}

impl EmbeddingProfileSummary {
    /// Whether this stored profile is compatible with the expected query profile
    /// (03 §7: dimensions / distance / modality / profile_hash all match).
    #[must_use]
    pub fn matches_profile(&self, expected: &EmbeddingProfileSummary) -> bool {
        self.dimensions == expected.dimensions
            && self.distance == expected.distance
            && self.modality == expected.modality
            && self.profile_hash == expected.profile_hash
    }
}

/// The chunk-embedding profiles present in a scope's `sqlite.db` (03 §7 compat).
/// Empty means the scope has no chunk embeddings (vector search unavailable).
pub fn chunk_embedding_profiles(conn: &Connection) -> Result<Vec<EmbeddingProfileSummary>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT dimensions, distance, modality, profile_hash
         FROM embeddings WHERE target_type = 'chunk'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(EmbeddingProfileSummary {
            dimensions: row.get::<_, i64>(0)? as u64,
            distance: row.get(1)?,
            modality: row.get(2)?,
            profile_hash: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Look up the stored vector BLOB for a content identity (`embedding_hash`) so an
/// unchanged chunk can reuse it without re-calling the Embedding Adapter
/// (CT3-EMBED-006, 04 §5.5). Returns the raw f32 LE bytes.
pub fn content_vector(conn: &Connection, embedding_hash: &str) -> Result<Option<Vec<u8>>> {
    let mut stmt = conn.prepare("SELECT vector, dimensions FROM embeddings WHERE id = ?1")?;
    let mut rows = stmt.query(params![embedding_hash])?;
    match rows.next()? {
        Some(row) => {
            let vector: Vec<u8> = row.get(0)?;
            let dimensions = sql_dimension(row.get(1)?)?;
            validate_embedding_vector(&vector, dimensions)?;
            Ok(Some(vector))
        }
        None => Ok(None),
    }
}

/// Write one chunk embedding: upsert the content-addressed `embeddings` row
/// (`id = embedding_hash`, `target_id = text_hash`; idempotent so shared content
/// is stored once) and map this `chunk_id` into `chunk_vec` (04 §4.3). The
/// `embeddings` table is the source of truth; `chunk_vec` is its derived KNN copy.
#[allow(clippy::too_many_arguments)]
pub fn write_chunk_embedding(
    conn: &Connection,
    embedding_hash: &str,
    text_hash: &str,
    chunk_id: &str,
    vector: &[u8],
    dimensions: u64,
    distance: &str,
    modality: &str,
    profile_hash: &str,
    // 2026-07-24 (07 §5.3 contextual-embedding addendum): the humanized filename
    // context this vector was embedded with (`chunk_embedding_context`), stored
    // so `rebuild_chunk_vec` can disambiguate several
    // rows sharing one `text_hash`. `None` for a symbolic-name chunk —
    // persisted as SQL NULL.
    context_key: Option<&str>,
) -> Result<()> {
    with_savepoint(conn, "kio_write_chunk_embedding_and_link", || {
        write_chunk_embedding_source(
            conn,
            embedding_hash,
            text_hash,
            vector,
            dimensions,
            distance,
            modality,
            profile_hash,
            context_key,
        )?;
        let _ = link_chunk_vec(conn, chunk_id, vector, dimensions)?;
        Ok(())
    })
}

/// Persist a canonical contextual source without choosing a chunk's scalar projection.
/// Retired-profile projections are invalidated atomically with source publication.
#[allow(clippy::too_many_arguments)]
pub fn write_chunk_embedding_source(
    conn: &Connection,
    embedding_hash: &str,
    text_hash: &str,
    vector: &[u8],
    dimensions: u64,
    distance: &str,
    modality: &str,
    profile_hash: &str,
    context_key: Option<&str>,
) -> Result<()> {
    validate_embedding_vector(vector, dimensions)?;
    with_savepoint(conn, "kio_write_chunk_embedding", || {
        let evicted = conn.execute(
            "DELETE FROM embeddings
             WHERE target_type = 'chunk' AND target_id = ?1 AND profile_hash <> ?2",
            params![text_hash, profile_hash],
        )?;
        // R25-11: when — and only when — that eviction removed something, drop
        // the `chunk_vec` rows derived from it.
        //
        // `chunk_vec` is defined as a derivation of `embeddings` (04 §4.3), so a
        // row whose backing embedding is gone is not stale data, it is invalid.
        // The chunk being written is re-linked below, but a chunk that shares
        // this `text_hash` and is not being re-sent (secrets-held,
        // budget-paused, failed) would otherwise keep a vector from the retired
        // profile and go on being cosine-ranked against queries embedded in a
        // different space. Dropping it costs that chunk vector search until it
        // is re-embedded, which is the honest outcome; keeping it is a silently
        // wrong ranking.
        //
        // Gated on `evicted` because the ordinary case — two chunks sharing one
        // `text_hash`, written one after another under the SAME profile — evicts
        // nothing, and an unconditional delete would have the second write wipe
        // the first chunk's row.
        if evicted > 0 {
            conn.execute(
                "DELETE FROM chunk_vec WHERE chunk_id IN (
                     SELECT chunk_id FROM chunks WHERE text_hash = ?1
                 )",
                params![text_hash],
            )?;
        }
        conn.execute(
            "INSERT INTO embeddings(id, target_type, target_id, modality, vector, dimensions, distance, profile_hash, context_key)
             VALUES (?1, 'chunk', ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO NOTHING",
            params![
                embedding_hash,
                text_hash,
                modality,
                vector,
                dimensions as i64,
                distance,
                profile_hash,
                context_key
            ],
        )?;
        let canonical = canonical_chunk_embedding(
            conn,
            embedding_hash,
            text_hash,
            dimensions,
            distance,
            modality,
            profile_hash,
            context_key,
        )?;
        if canonical.vector != vector {
            return Err(IndexError::Contract(
                "embedding identity already has a different canonical vector".to_owned(),
            ));
        }
        Ok(())
    })
}

/// Map a `chunk_id` to a vector in `chunk_vec` (idempotent). No-op if the vector
/// width is not the adopted `chunk_vec` dimension (incompatible-width embeddings
/// never enter the KNN table).
///
/// Reports whether the row was actually written, for the same reason
/// `link_chunk_vecs_to_content_vector` reports which ids it linked: the width
/// check is decided here, and the device replica must follow the outcome
/// instead of re-deriving the rule.
pub fn link_chunk_vec(
    conn: &Connection,
    chunk_id: &str,
    vector: &[u8],
    dimensions: u64,
) -> Result<bool> {
    link_chunk_vec_if_compatible(conn, chunk_id, vector, dimensions)
}

fn link_chunk_vec_if_compatible(
    conn: &Connection,
    chunk_id: &str,
    vector: &[u8],
    dimensions: u64,
) -> Result<bool> {
    if usize::try_from(dimensions).ok() != Some(CHUNK_VEC_DIMENSIONS) {
        return Ok(false);
    }
    validate_embedding_vector(vector, dimensions)?;
    // vec0 virtual tables do not support UPSERT; delete-then-insert is idempotent.
    conn.execute(
        "DELETE FROM chunk_vec WHERE chunk_id = ?1",
        params![chunk_id],
    )?;
    conn.execute(
        "INSERT INTO chunk_vec(chunk_id, embedding) VALUES (?1, ?2)",
        params![chunk_id, vector],
    )?;
    Ok(true)
}

/// Map a `chunk_id` to `chunk_vec` unless the caller has identified it as held.
/// This is the lower-crate publication guard; callers still own how the hold set
/// is derived from current policy.
pub fn link_chunk_vec_unless_held(
    conn: &Connection,
    chunk_id: &str,
    vector: &[u8],
    dimensions: u64,
    held_chunk_ids: &BTreeSet<String>,
) -> Result<bool> {
    if held_chunk_ids.contains(chunk_id) {
        return Ok(false);
    }
    link_chunk_vec_if_compatible(conn, chunk_id, vector, dimensions)
}

/// Fan one persisted content vector out to several chunk ids. This gives callers
/// a transactional primitive for duplicate same-batch identities: persist one
/// canonical row, then link every member from those persisted bytes.
///
/// Returns the ids that were actually linked, not merely how many. Two callers
/// need the identities: `link_chunk_vec_unless_held` silently drops a secrets-
/// held chunk (R20-10) and `link_chunk_vec_if_compatible` drops a width
/// mismatch, so "which members gained a vector" is a decision made HERE and
/// nowhere else. The device replica mirrors `chunk_vec` and must follow that
/// decision rather than re-evaluate the rule against its own copy of `held` —
/// a replica that re-derived it would expose a held chunk to vector search the
/// moment the two rules drifted (03 §4 invariant 8).
pub fn link_chunk_vecs_to_content_vector<'a>(
    conn: &Connection,
    embedding_hash: &str,
    chunk_ids: impl IntoIterator<Item = &'a str>,
    held_chunk_ids: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let chunk_ids = chunk_ids.into_iter().collect::<Vec<_>>();
    with_savepoint(conn, "kio_link_chunk_vecs_to_content_vector", || {
        let canonical = stored_embedding_vector(conn, embedding_hash)?.ok_or_else(|| {
            IndexError::Contract(format!(
                "missing canonical embedding vector for {embedding_hash}"
            ))
        })?;
        let mut linked = Vec::new();
        for chunk_id in &chunk_ids {
            if link_chunk_vec_unless_held(
                conn,
                chunk_id,
                &canonical.vector,
                canonical.dimensions,
                held_chunk_ids,
            )? {
                linked.push((*chunk_id).to_string());
            }
        }
        Ok(linked)
    })
}

/// Rebuild `chunk_vec` from `embeddings` joined to `chunks` on `text_hash`
/// (04 §4.3 rebuild order objects → embeddings → chunk_vec; `embeddings` is the
/// source of truth). Only adopted-width chunk embeddings are re-linked.
///
/// R20-10: `held_chunk_ids` are chunks whose embedding is currently on a secrets hold.
/// They are excluded from `chunk_vec` because the content-hash JOIN below would otherwise
/// link a held (Tier B) chunk to a vector produced by a NON-secret content-twin's online
/// send — exposing the held file in vector/semantic search without `--send-secrets`.
/// R19-4's Failed(retryable) content-twin convergence is unaffected: only Paused
/// secret-holds are passed here, never Failed tasks. Releasing the hold (`--send-secrets`)
/// drops the chunk from this set, so the next rebuild re-links it.
/// Rebuild only explicitly selected contextual projections. An absent chunk has
/// no scalar projection; an explicit `None` selects only SQL NULL context.
pub fn rebuild_chunk_vec_for_contexts(
    conn: &Connection,
    expected_profile: Option<&EmbeddingProfileSummary>,
    held_chunk_ids: &BTreeSet<String>,
    selected_contexts: &BTreeMap<String, Option<String>>,
) -> Result<()> {
    with_savepoint(conn, "kio_rebuild_chunk_vec", || {
        conn.execute_batch("DELETE FROM chunk_vec;")?;
        // 2026-07-24 (07 §5.3 contextual-embedding addendum): one `text_hash` can
        // now own several `embeddings` rows (same body, different filenames).
        // A rebuild may link only the NULL-safe exact contextual match. Absence
        // is an honest no-vector result; more than one matching row is corrupt
        // rather than permission to select an arbitrary profile or vector.
        let mut stmt = conn.prepare(
            "SELECT c.chunk_id, e.vector, e.dimensions, e.context_key
             FROM chunks c
             JOIN embeddings e ON e.target_type = 'chunk' AND e.target_id = c.text_hash
             WHERE (?1 = 1
                    AND e.dimensions = ?2
                    AND e.distance = ?3
                    AND e.modality = ?4
                    AND e.profile_hash = ?5)
             ORDER BY c.chunk_id, e.id",
        )?;
        let (profile_present, dimensions, distance, modality, profile_hash) = match expected_profile
        {
            Some(profile) => (
                1_i64,
                profile.dimensions as i64,
                profile.distance.as_str(),
                profile.modality.as_str(),
                profile.profile_hash.as_str(),
            ),
            None => (0_i64, 0_i64, "", "", ""),
        };
        let rows = stmt
            .query_map(
                params![
                    profile_present,
                    dimensions,
                    distance,
                    modality,
                    profile_hash
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // Keep all source candidates; only the caller's exact context selects one.
        #[allow(clippy::type_complexity)]
        let mut by_chunk: BTreeMap<String, Vec<(Option<String>, (Vec<u8>, i64))>> = BTreeMap::new();
        for (chunk_id, vector, dimensions, context_key) in rows {
            by_chunk
                .entry(chunk_id)
                .or_default()
                .push((context_key, (vector, dimensions)));
        }
        for (chunk_id, candidates) in by_chunk {
            let Some(wanted_context) = selected_contexts.get(&chunk_id) else {
                continue;
            };
            if held_chunk_ids.contains(&chunk_id) {
                continue;
            }
            let matching = candidates
                .iter()
                .filter(|(context, _)| context.as_deref() == wanted_context.as_deref())
                .collect::<Vec<_>>();
            match matching.as_slice() {
                [] => {}
                [(_, (vector, dimensions))] => {
                    link_chunk_vec_unless_held(
                        conn,
                        &chunk_id,
                        vector,
                        sql_dimension(*dimensions)?,
                        held_chunk_ids,
                    )?;
                }
                _ => {
                    return Err(IndexError::Schema(format!(
                        "multiple contextual chunk embeddings match {chunk_id}"
                    )));
                }
            }
        }
        Ok(())
    })
}

/// Return the subset of supplied chunk ids that currently have a materialized
/// `chunk_vec` row. Task-state callers can use this to distinguish an already
/// materialized budget pause from a still-unpublished authorization hold.
/// KNN over `chunk_vec`: the nearest `k` chunk_ids to `query_vector` ordered by
/// cosine distance (04 §4.3). Ties are re-broken by `chunk_id` ascending by the
/// caller. `query_vector` is raw f32 LE bytes of the adopted dimension.
pub fn knn_chunk_distances(
    conn: &Connection,
    query_vector: &[u8],
    k: u64,
) -> Result<Vec<(String, f64)>> {
    if query_vector.len() != CHUNK_VEC_DIMENSIONS * 4 {
        return Err(IndexError::Contract(
            "KIO-E-SEARCH-VEC-INCOMPAT-001: query vector width mismatch".to_owned(),
        ));
    }
    validate_embedding_vector(query_vector, CHUNK_VEC_DIMENSIONS as u64)?;
    let mut stmt = conn.prepare(
        "SELECT chunk_id, distance FROM chunk_vec
         WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![query_vector, k as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Number of `chunk_vec` rows (used to size the KNN over-fetch, and as vector
/// coverage evidence).
pub fn chunk_vec_count(conn: &Connection) -> Result<u64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM chunk_vec", [], |row| {
        row.get::<_, i64>(0)
    })? as u64)
}

/// Read one chunk's stored vector as f32 for the MMR cosine similarity (05 §1.4).
pub fn read_chunk_vector(conn: &Connection, chunk_id: &str) -> Result<Option<Vec<f32>>> {
    let mut stmt = conn.prepare("SELECT embedding FROM chunk_vec WHERE chunk_id = ?1")?;
    let mut rows = stmt.query(params![chunk_id])?;
    match rows.next()? {
        Some(row) => {
            let bytes: Vec<u8> = row.get(0)?;
            validate_embedding_vector(&bytes, CHUNK_VEC_DIMENSIONS as u64)?;
            Ok(Some(f32_from_le_bytes(&bytes)))
        }
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Image object embeddings (04 §4.3's `image_vec`).
//
// The chunk path has to rediscover which chunk row currently carries a given
// body, which is why it joins through `chunks.text_hash` and disambiguates on
// `context_key`. None of that applies here: an image IS the content-addressed
// object, so `embeddings.target_id` is already the `image_vec` key and there is
// no owner to look up. `chunk_filename_context_v1` is likewise a rule about
// chunk bodies and does not touch images, so no context key is stored.
// ---------------------------------------------------------------------------

/// Persist one image object's vector and link it into `image_vec`.
// Same shape as `write_chunk_embedding` above, and allowed for the same
// reason: every parameter is a distinct column of the row being written.
#[allow(clippy::too_many_arguments)]
pub fn write_image_embedding(
    conn: &Connection,
    embedding_hash: &str,
    image_hash: &str,
    vector: &[u8],
    dimensions: u64,
    distance: &str,
    modality: &str,
    profile_hash: &str,
) -> Result<()> {
    validate_embedding_vector(vector, dimensions)?;
    with_savepoint(conn, "kio_write_image_embedding", || {
        // Same eviction rule as the chunk path: a vector from a retired profile
        // is not stale data but invalid data, since it would go on being
        // cosine-ranked against queries embedded in a different space (03 §7).
        let evicted = conn.execute(
            "DELETE FROM embeddings
             WHERE target_type = 'image' AND target_id = ?1 AND profile_hash <> ?2",
            params![image_hash, profile_hash],
        )?;
        if evicted > 0 {
            conn.execute(
                "DELETE FROM image_vec WHERE image_id = ?1",
                params![image_hash],
            )?;
        }
        conn.execute(
            "INSERT INTO embeddings(id, target_type, target_id, modality, vector, dimensions, distance, profile_hash, context_key)
             VALUES (?1, 'image', ?2, ?3, ?4, ?5, ?6, ?7, NULL)
             ON CONFLICT(id) DO NOTHING",
            params![
                embedding_hash,
                image_hash,
                modality,
                vector,
                dimensions as i64,
                distance,
                profile_hash
            ],
        )?;
        link_image_vec(conn, image_hash, vector, dimensions)?;
        Ok(())
    })
}

/// Map an `image_hash` to a vector in `image_vec` (idempotent). No-op when the
/// width is not the adopted one — an incompatible-width vector never enters the
/// KNN table, exactly as on the chunk side.
pub fn link_image_vec(
    conn: &Connection,
    image_hash: &str,
    vector: &[u8],
    dimensions: u64,
) -> Result<bool> {
    if usize::try_from(dimensions).ok() != Some(CHUNK_VEC_DIMENSIONS) {
        return Ok(false);
    }
    validate_embedding_vector(vector, dimensions)?;
    // vec0 virtual tables do not support UPSERT; delete-then-insert is idempotent.
    conn.execute(
        "DELETE FROM image_vec WHERE image_id = ?1",
        params![image_hash],
    )?;
    conn.execute(
        "INSERT INTO image_vec(image_id, embedding) VALUES (?1, ?2)",
        params![image_hash, vector],
    )?;
    Ok(true)
}

/// Which image objects already carry a vector under `profile_hash`, so a
/// re-index embeds only what is missing (the image counterpart of the chunk
/// path's content-addressed reuse).
pub fn embedded_image_hashes(
    conn: &Connection,
    profile_hash: &str,
) -> Result<std::collections::BTreeSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT target_id FROM embeddings
         WHERE target_type = 'image' AND profile_hash = ?1",
    )?;
    let rows = stmt.query_map(params![profile_hash], |row| row.get::<_, String>(0))?;
    let mut out = std::collections::BTreeSet::new();
    for row in rows {
        out.insert(row?);
    }
    Ok(out)
}

/// KNN over `image_vec`, returning `(image_hash, distance)` nearest first.
pub fn knn_image_distances(
    conn: &Connection,
    query_vector: &[u8],
    k: u64,
) -> Result<Vec<(String, f64)>> {
    if query_vector.len() != CHUNK_VEC_DIMENSIONS * 4 {
        return Err(IndexError::Contract(
            "KIO-E-SEARCH-VEC-INCOMPAT-001: query vector width mismatch".to_owned(),
        ));
    }
    validate_embedding_vector(query_vector, CHUNK_VEC_DIMENSIONS as u64)?;
    let mut stmt = conn.prepare(
        "SELECT image_id, distance FROM image_vec
         WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![query_vector, k as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Read one image's stored vector as f32 (MMR cosine similarity, 05 §1.4).
pub fn read_image_vector(conn: &Connection, image_hash: &str) -> Result<Option<Vec<f32>>> {
    let mut stmt = conn.prepare("SELECT embedding FROM image_vec WHERE image_id = ?1")?;
    let mut rows = stmt.query(params![image_hash])?;
    match rows.next()? {
        Some(row) => {
            let bytes: Vec<u8> = row.get(0)?;
            validate_embedding_vector(&bytes, CHUNK_VEC_DIMENSIONS as u64)?;
            Ok(Some(f32_from_le_bytes(&bytes)))
        }
        None => Ok(None),
    }
}

pub fn image_vec_count(conn: &Connection) -> Result<u64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM image_vec", [], |row| {
        row.get::<_, i64>(0)
    })? as u64)
}

/// Rebuild `image_vec` from the `embeddings` rows that back it (04 §4.3's
/// `objects/` → `embeddings` → `chunk_vec` → `image_vec` order).
///
/// Restricted to `profile_hash` for the same reason the chunk rebuild is:
/// several profiles' rows can legitimately coexist, and linking all of them
/// would put two vector spaces in one KNN table.
pub fn rebuild_image_vec(
    conn: &Connection,
    expected_profile: Option<&EmbeddingProfileSummary>,
) -> Result<()> {
    with_savepoint(conn, "kio_rebuild_image_vec", || {
        conn.execute("DELETE FROM image_vec", [])?;
        let mut stmt = conn.prepare(
            "SELECT target_id, vector, dimensions FROM embeddings
             WHERE target_type = 'image'
               AND (?1 = 1
                    AND dimensions = ?2
                    AND distance = ?3
                    AND modality = ?4
                    AND profile_hash = ?5)
             ORDER BY target_id, id",
        )?;
        let (profile_present, dimensions, distance, modality, profile_hash) = match expected_profile
        {
            Some(profile) => (
                1_i64,
                profile.dimensions as i64,
                profile.distance.as_str(),
                profile.modality.as_str(),
                profile.profile_hash.as_str(),
            ),
            None => (0_i64, 0_i64, "", "", ""),
        };
        let rows = stmt.query_map(
            params![
                profile_present,
                dimensions,
                distance,
                modality,
                profile_hash
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)? as u64,
                ))
            },
        )?;
        let mut previous_image: Option<String> = None;
        for row in rows {
            let (image_hash, vector, dimensions) = row?;
            if previous_image.as_deref() == Some(image_hash.as_str()) {
                return Err(IndexError::Schema(format!(
                    "multiple compatible image embeddings match {image_hash}"
                )));
            }
            link_image_vec(conn, &image_hash, &vector, dimensions)?;
            previous_image = Some(image_hash);
        }
        Ok(())
    })
}

/// Decode a raw little-endian f32 BLOB into a vector.
pub fn f32_from_le_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}

/// Encode an f32 vector to a raw little-endian BLOB for `embeddings.vector` /
/// `chunk_vec.embedding`.
pub fn f32_to_le_bytes(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Validate raw little-endian f32 vector bytes for cosine search.
pub fn validate_embedding_vector(vector: &[u8], dimensions: u64) -> Result<()> {
    let expected = expected_vector_len(dimensions)?;
    if vector.len() != expected {
        return Err(IndexError::Contract(format!(
            "embedding vector width mismatch: expected {expected} bytes for {dimensions} dimensions, got {}",
            vector.len()
        )));
    }
    let mut norm_sq = 0.0f64;
    for chunk in vector.as_chunks::<4>().0 {
        let value = f32::from_le_bytes(*chunk);
        if !value.is_finite() {
            return Err(IndexError::Contract(
                "embedding vector component is not finite".to_owned(),
            ));
        }
        let value = f64::from(value);
        norm_sq += value * value;
    }
    if !norm_sq.is_finite() || norm_sq <= 0.0 {
        return Err(IndexError::Contract(
            "embedding vector norm must be positive and finite".to_owned(),
        ));
    }
    Ok(())
}

struct StoredEmbeddingVector {
    vector: Vec<u8>,
    dimensions: u64,
}

struct StoredChunkEmbedding {
    vector: Vec<u8>,
}

fn stored_embedding_vector(
    conn: &Connection,
    embedding_hash: &str,
) -> Result<Option<StoredEmbeddingVector>> {
    let Some((vector, dimensions)) = conn
        .query_row(
            "SELECT vector, dimensions FROM embeddings WHERE id = ?1",
            params![embedding_hash],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let dimensions = sql_dimension(dimensions)?;
    validate_embedding_vector(&vector, dimensions)?;
    Ok(Some(StoredEmbeddingVector { vector, dimensions }))
}

#[allow(clippy::too_many_arguments)]
fn canonical_chunk_embedding(
    conn: &Connection,
    embedding_hash: &str,
    text_hash: &str,
    dimensions: u64,
    distance: &str,
    modality: &str,
    profile_hash: &str,
    context_key: Option<&str>,
) -> Result<StoredChunkEmbedding> {
    let Some((
        target_type,
        target_id,
        stored_modality,
        vector,
        stored_dimensions,
        stored_distance,
        stored_profile_hash,
        stored_context_key,
    )) = conn
        .query_row(
            "SELECT target_type, target_id, modality, vector, dimensions, distance, profile_hash, context_key
             FROM embeddings WHERE id = ?1",
            params![embedding_hash],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            },
        )
        .optional()?
    else {
        return Err(IndexError::Contract(format!(
            "missing canonical embedding row for {embedding_hash}"
        )));
    };
    let stored_dimensions = sql_dimension(stored_dimensions)?;
    if target_type != "chunk"
        || target_id != text_hash
        || stored_dimensions != dimensions
        || stored_distance != distance
        || stored_modality != modality
        || stored_profile_hash != profile_hash
        || stored_context_key.as_deref() != context_key
    {
        return Err(IndexError::Contract(format!(
            "canonical embedding metadata mismatch for {embedding_hash}"
        )));
    }
    validate_embedding_vector(&vector, stored_dimensions)?;
    Ok(StoredChunkEmbedding { vector })
}

fn expected_vector_len(dimensions: u64) -> Result<usize> {
    usize::try_from(dimensions)
        .ok()
        .and_then(|dimensions| dimensions.checked_mul(4))
        .ok_or_else(|| IndexError::Contract("embedding vector dimensions overflow".to_owned()))
}

fn sql_dimension(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| {
        IndexError::Contract("embedding vector dimensions must be non-negative".to_owned())
    })
}

fn with_savepoint<T>(
    conn: &Connection,
    name: &str,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    conn.execute_batch(&format!("SAVEPOINT {name};"))?;
    match operation() {
        Ok(value) => {
            conn.execute_batch(&format!("RELEASE {name};"))?;
            Ok(value)
        }
        Err(err) => {
            let _ = conn.execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name};"));
            Err(err)
        }
    }
}

fn target_type_name(value: EmbeddingTargetType) -> &'static str {
    match value {
        EmbeddingTargetType::Chunk => "chunk",
        EmbeddingTargetType::Image => "image",
    }
}

fn distance_name(value: EmbeddingDistance) -> &'static str {
    match value {
        EmbeddingDistance::Cosine => "cosine",
        EmbeddingDistance::L2 => "l2",
        EmbeddingDistance::InnerProduct => "inner_product",
    }
}

fn modality_name(value: EmbeddingModality) -> &'static str {
    match value {
        EmbeddingModality::Text => "text",
        EmbeddingModality::Image => "image",
        EmbeddingModality::Multimodal => "multimodal",
    }
}

fn hash_jcs(value: &Value) -> Result<String> {
    serde_jcs::to_vec(value)
        .map(|bytes| hash_bytes(&bytes))
        .map_err(|err| IndexError::Schema(err.to_string()))
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", lower_hex(&digest))
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct3_embed_001_embedding_hash_vector() {
        let profile_hash =
            "sha256:66aff638f38a099ff989ca97675ebd3c573a40ee53cc1cdfe05fb06102d2bb09";
        assert_eq!(
            embedding_hash(
                EmbeddingTargetType::Chunk,
                "sha256:c5e31f10da04b722769bdbbd60a55b94c177b5f3bf9c64e5341be7281d115c3d",
                768,
                EmbeddingDistance::Cosine,
                EmbeddingModality::Multimodal,
                profile_hash,
                // Contextual-embedding addendum (2026-07-24): `None` omits the
                // `context` key entirely, so this pre-addendum identity vector
                // is unchanged byte-for-byte.
                None,
            )
            .unwrap(),
            "sha256:7bd32d26ad2b721e32c99536513abf58c6aeee626d1edc65e30069abce01a975"
        );
    }

    /// Contextual-embedding addendum (2026-07-24): a `Some(context)` folds the
    /// filename into the identity, so the same `(target, profile)` with a
    /// context differs from the bare form and from a different context.
    #[test]
    fn contextual_embedding_hash_folds_filename_context() {
        let args = |context| {
            embedding_hash(
                EmbeddingTargetType::Chunk,
                "sha256:c5e31f10da04b722769bdbbd60a55b94c177b5f3bf9c64e5341be7281d115c3d",
                768,
                EmbeddingDistance::Cosine,
                EmbeddingModality::Multimodal,
                "sha256:66aff638f38a099ff989ca97675ebd3c573a40ee53cc1cdfe05fb06102d2bb09",
                context,
            )
            .unwrap()
        };
        let bare = args(None);
        let recovery = args(Some("recovery window"));
        let latency = args(Some("latency review"));
        assert_ne!(bare, recovery);
        assert_ne!(recovery, latency);
        assert_eq!(recovery, args(Some("recovery window")));
    }

    /// Contextual-embedding addendum: the humanizer turns filename stems into
    /// natural-language tokens and drops purely symbolic names to `None`.
    #[test]
    fn chunk_embedding_context_humanizes_filename_stems() {
        assert_eq!(
            chunk_embedding_context("a/b/recovery-window.md").as_deref(),
            Some("recovery window")
        );
        assert_eq!(
            chunk_embedding_context("Q4_reportFinal.docx").as_deref(),
            Some("Q4 report Final")
        );
        assert_eq!(
            chunk_embedding_context("control-coverage.png").as_deref(),
            Some("control coverage")
        );
        assert_eq!(chunk_embedding_context("____.md"), None);
        assert_eq!(
            contextualized_embedding_input(Some("recovery window"), "body"),
            "recovery window\n\nbody"
        );
        assert_eq!(contextualized_embedding_input(None, "body"), "body");
    }

    #[test]
    fn ct3_embed_004_profile_compat_requires_exact_summary_match() {
        let expected = EmbeddingProfileSummary {
            dimensions: 768,
            distance: "cosine".to_owned(),
            modality: "multimodal".to_owned(),
            profile_hash: "sha256:profile".to_owned(),
        };
        assert!(expected.matches_profile(&expected));
        let text_modality = EmbeddingProfileSummary {
            modality: "text".to_owned(),
            ..expected.clone()
        };
        assert!(!text_modality.matches_profile(&expected));
        let wrong_dims = EmbeddingProfileSummary {
            dimensions: 512,
            ..expected.clone()
        };
        assert!(!wrong_dims.matches_profile(&expected));
    }

    use crate::ChunkRow;
    use crate::fts::{CHUNK_VEC_DIMENSIONS, FtsSchemaConfig, FtsTokenizer, SqliteFtsIndex};
    use rusqlite::Connection;

    fn schema_conn() -> SqliteFtsIndex {
        SqliteFtsIndex::in_memory(FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        })
        .unwrap()
    }

    fn chunk_row(chunk_id: &str, text_hash: &str) -> ChunkRow {
        ChunkRow {
            chunk_id: chunk_id.to_owned(),
            raw_hash: "sha256:raw".to_owned(),
            tool_profile_hash: "sha256:tool".to_owned(),
            r#gen: 0,
            unit_key: "page:1".to_owned(),
            unit_content_hash:
                "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_owned(),
            chunking_config_hash: "sha256:cfg".to_owned(),
            raw_path: "a.md".to_owned(),
            heading_path: None,
            section_id: None,
            byte_start: 0,
            byte_end: 4,
            text_hash: text_hash.to_owned(),
            text: "body".to_owned(),
            created_at: "2026-07-04T00:00:00Z".to_owned(),
        }
    }

    /// Basis vector e_i as a 768-dim L2-normalized f32 BLOB.
    fn basis_vector(axis: usize) -> Vec<u8> {
        let mut v = vec![0f32; CHUNK_VEC_DIMENSIONS];
        v[axis] = 1.0;
        f32_to_le_bytes(&v)
    }

    fn zero_vector() -> Vec<u8> {
        f32_to_le_bytes(&vec![0f32; CHUNK_VEC_DIMENSIONS])
    }

    fn infinite_vector() -> Vec<u8> {
        let mut v = vec![0f32; CHUNK_VEC_DIMENSIONS];
        v[0] = f32::INFINITY;
        f32_to_le_bytes(&v)
    }

    fn insert_raw_embedding(
        conn: &Connection,
        embedding_hash: &str,
        text_hash: &str,
        vector: &[u8],
        dimensions: u64,
    ) {
        conn.execute(
            "INSERT INTO embeddings(id, target_type, target_id, modality, vector, dimensions, distance, profile_hash, context_key)
             VALUES (?1, 'chunk', ?2, 'multimodal', ?3, ?4, 'cosine', 'sha256:profile', 'a')",
            params![embedding_hash, text_hash, vector, dimensions as i64],
        )
        .unwrap();
    }

    fn write_basis(conn: &Connection, chunk_id: &str, text_hash: &str, axis: usize) {
        write_chunk_embedding(
            conn,
            &format!("sha256:emb-{text_hash}"),
            text_hash,
            chunk_id,
            &basis_vector(axis),
            CHUNK_VEC_DIMENSIONS as u64,
            "cosine",
            "multimodal",
            "sha256:profile",
            Some("a"),
        )
        .unwrap();
    }

    #[test]
    fn explicit_context_projection_is_independent_of_source_write_order() {
        for order in [[0, 1], [1, 0]] {
            let mut store = schema_conn();
            store.index_chunk(&chunk_row("c", "sha256:text")).unwrap();
            let conn = store.connection();
            let profile = EmbeddingProfileSummary {
                dimensions: CHUNK_VEC_DIMENSIONS as u64,
                distance: "cosine".to_owned(),
                modality: "multimodal".to_owned(),
                profile_hash: "sha256:profile".to_owned(),
            };
            for axis in order {
                write_chunk_embedding_source(
                    conn,
                    &format!("sha256:source-{axis}"),
                    "sha256:text",
                    &basis_vector(axis),
                    profile.dimensions,
                    "cosine",
                    "multimodal",
                    &profile.profile_hash,
                    Some(if axis == 0 { "a" } else { "renamed" }),
                )
                .unwrap();
                assert!(read_chunk_vector(conn, "c").unwrap().is_none());
            }
            let contexts = BTreeMap::from([("c".to_owned(), Some("renamed".to_owned()))]);
            rebuild_chunk_vec_for_contexts(conn, Some(&profile), &BTreeSet::new(), &contexts)
                .unwrap();
            assert_eq!(
                read_chunk_vector(conn, "c").unwrap().unwrap(),
                f32_from_le_bytes(&basis_vector(1))
            );
            rebuild_chunk_vec_for_contexts(
                conn,
                Some(&profile),
                &BTreeSet::new(),
                &BTreeMap::new(),
            )
            .unwrap();
            assert!(read_chunk_vector(conn, "c").unwrap().is_none());
            rebuild_chunk_vec_for_contexts(
                conn,
                Some(&profile),
                &BTreeSet::from(["c".to_owned()]),
                &contexts,
            )
            .unwrap();
            assert!(read_chunk_vector(conn, "c").unwrap().is_none());
            let missing = BTreeMap::from([("c".to_owned(), Some("unknown".to_owned()))]);
            rebuild_chunk_vec_for_contexts(conn, Some(&profile), &BTreeSet::new(), &missing)
                .unwrap();
            assert!(read_chunk_vector(conn, "c").unwrap().is_none());
        }
    }

    #[test]
    fn explicit_null_context_selects_only_context_free_source() {
        let mut store = schema_conn();
        store.index_chunk(&chunk_row("c", "sha256:text")).unwrap();
        let conn = store.connection();
        let profile = EmbeddingProfileSummary {
            dimensions: CHUNK_VEC_DIMENSIONS as u64,
            distance: "cosine".to_owned(),
            modality: "multimodal".to_owned(),
            profile_hash: "sha256:profile".to_owned(),
        };
        for (axis, context) in [(0, Some("a")), (1, None)] {
            write_chunk_embedding_source(
                conn,
                &format!("sha256:source-{axis}"),
                "sha256:text",
                &basis_vector(axis),
                profile.dimensions,
                "cosine",
                "multimodal",
                &profile.profile_hash,
                context,
            )
            .unwrap();
        }
        rebuild_chunk_vec_for_contexts(
            conn,
            Some(&profile),
            &BTreeSet::new(),
            &BTreeMap::from([("c".to_owned(), None)]),
        )
        .unwrap();
        assert_eq!(
            read_chunk_vector(conn, "c").unwrap().unwrap(),
            f32_from_le_bytes(&basis_vector(1))
        );
    }

    #[test]
    fn canonical_context_conflict_rolls_back_profile_eviction_and_projection() {
        let mut store = schema_conn();
        store.index_chunk(&chunk_row("c", "sha256:text")).unwrap();
        let conn = store.connection();
        write_basis(conn, "c", "sha256:text", 0);
        // A conflicting new-profile identity already exists with a different context.
        conn.execute("INSERT INTO embeddings(id,target_type,target_id,modality,vector,dimensions,distance,profile_hash,context_key)
            VALUES ('sha256:conflict','chunk','sha256:text','multimodal',?1,?2,'cosine','new-profile','other')",
            params![basis_vector(1), CHUNK_VEC_DIMENSIONS as i64]).unwrap();
        let error = write_chunk_embedding_source(
            conn,
            "sha256:conflict",
            "sha256:text",
            &basis_vector(1),
            CHUNK_VEC_DIMENSIONS as u64,
            "cosine",
            "multimodal",
            "new-profile",
            Some("selected"),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("canonical embedding metadata mismatch")
        );
        assert_eq!(
            read_chunk_vector(conn, "c").unwrap().unwrap(),
            f32_from_le_bytes(&basis_vector(0))
        );
        assert!(
            content_vector(conn, "sha256:emb-sha256:text")
                .unwrap()
                .is_some()
        );
        let context: String = conn
            .query_row(
                "SELECT context_key FROM embeddings WHERE id = 'sha256:conflict'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(context, "other");
    }

    #[test]
    fn invalid_vectors_are_rejected_at_store_link_reuse_and_query_boundaries() {
        let store = schema_conn();
        let conn = store.connection();
        let zero = zero_vector();
        let infinite = infinite_vector();

        let err = write_chunk_embedding(
            conn,
            "sha256:zero",
            "sha256:t-zero",
            "c-zero",
            &zero,
            CHUNK_VEC_DIMENSIONS as u64,
            "cosine",
            "multimodal",
            "sha256:profile",
            Some("a"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("positive and finite"));
        assert!(content_vector(conn, "sha256:zero").unwrap().is_none());

        let err =
            link_chunk_vec(conn, "c-inf", &infinite, CHUNK_VEC_DIMENSIONS as u64).unwrap_err();
        assert!(err.to_string().contains("not finite"));
        assert!(read_chunk_vector(conn, "c-inf").unwrap().is_none());

        let err = knn_chunk_distances(conn, &zero, 1).unwrap_err();
        assert!(err.to_string().contains("positive and finite"));

        write_basis(conn, "c-valid", "sha256:t-valid", 0);
        let knn = knn_chunk_distances(conn, &basis_vector(0), 1).unwrap();
        assert_eq!(knn[0].0, "c-valid");
        assert_eq!(knn[0].1, 0.0);
    }

    #[test]
    fn invalid_content_vector_is_not_reused() {
        let mut store = schema_conn();
        store
            .index_chunk(&chunk_row("c-invalid", "sha256:t-invalid"))
            .unwrap();
        let conn = store.connection();
        insert_raw_embedding(
            conn,
            "sha256:invalid",
            "sha256:t-invalid",
            &zero_vector(),
            CHUNK_VEC_DIMENSIONS as u64,
        );

        let err = content_vector(conn, "sha256:invalid").unwrap_err();
        assert!(err.to_string().contains("positive and finite"));
    }

    #[test]
    fn duplicate_identity_conflict_keeps_chunk_vec_tied_to_canonical_vector() {
        let store = schema_conn();
        let conn = store.connection();
        let first = basis_vector(0);
        let conflicting = basis_vector(1);

        write_chunk_embedding(
            conn,
            "sha256:dup",
            "sha256:t-dup",
            "c-a",
            &first,
            CHUNK_VEC_DIMENSIONS as u64,
            "cosine",
            "multimodal",
            "sha256:profile",
            None,
        )
        .unwrap();
        let err = write_chunk_embedding(
            conn,
            "sha256:dup",
            "sha256:t-dup",
            "c-b",
            &conflicting,
            CHUNK_VEC_DIMENSIONS as u64,
            "cosine",
            "multimodal",
            "sha256:profile",
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("different canonical vector"));
        assert_eq!(read_chunk_vector(conn, "c-b").unwrap(), None);

        let linked = link_chunk_vecs_to_content_vector(
            conn,
            "sha256:dup",
            ["c-a", "c-b"].iter().copied(),
            &std::collections::BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(linked, ["c-a", "c-b"]);
        assert_eq!(read_chunk_vector(conn, "c-a").unwrap().unwrap()[0], 1.0);
        let c_b = read_chunk_vector(conn, "c-b").unwrap().unwrap();
        assert_eq!(c_b[0], 1.0);
        assert_eq!(c_b[1], 0.0);
    }

    #[test]
    fn rebuild_rolls_back_when_invalid_vector_would_be_relinked() {
        let mut store = schema_conn();
        store
            .index_chunk(&chunk_row("c-valid", "sha256:t-valid"))
            .unwrap();
        store
            .index_chunk(&chunk_row("c-bad", "sha256:t-bad"))
            .unwrap();
        let conn = store.connection();
        write_basis(conn, "c-valid", "sha256:t-valid", 0);
        insert_raw_embedding(
            conn,
            "sha256:bad",
            "sha256:t-bad",
            &zero_vector(),
            CHUNK_VEC_DIMENSIONS as u64,
        );
        assert_eq!(chunk_vec_count(conn).unwrap(), 1);

        let err = rebuild_chunk_vec_for_contexts(
            conn,
            Some(&EmbeddingProfileSummary {
                dimensions: CHUNK_VEC_DIMENSIONS as u64,
                distance: "cosine".to_owned(),
                modality: "multimodal".to_owned(),
                profile_hash: "sha256:profile".to_owned(),
            }),
            &std::collections::BTreeSet::new(),
            &BTreeMap::from([
                ("c-valid".to_owned(), Some("a".to_owned())),
                ("c-bad".to_owned(), Some("a".to_owned())),
            ]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("positive and finite"));

        assert_eq!(
            chunk_vec_count(conn).unwrap(),
            1,
            "savepoint rollback preserves the pre-rebuild projection"
        );
        assert!(read_chunk_vector(conn, "c-valid").unwrap().is_some());
        assert!(read_chunk_vector(conn, "c-bad").unwrap().is_none());
    }

    #[test]
    fn a_profile_switch_drops_a_sibling_chunk_vec_it_cannot_re_link() {
        // R25-11: two chunks share a `text_hash`, so they share one `embeddings`
        // row and each carry their own `chunk_vec` row. The device switches
        // embedding profile and only ONE of them is re-sent — the other is
        // secrets-held, budget-paused or failed. Evicting the old `embeddings`
        // row without its derived `chunk_vec` rows left the un-re-sent sibling
        // holding a vector from the retired profile, still cosine-ranked against
        // queries embedded in a different space.
        let mut store = schema_conn();
        store
            .index_chunk(&chunk_row("c-resent", "sha256:t-shared"))
            .unwrap();
        store
            .index_chunk(&chunk_row("c-stranded", "sha256:t-shared"))
            .unwrap();
        let conn = store.connection();
        for chunk_id in ["c-resent", "c-stranded"] {
            write_chunk_embedding(
                conn,
                "sha256:old-embedding",
                "sha256:t-shared",
                chunk_id,
                &basis_vector(0),
                CHUNK_VEC_DIMENSIONS as u64,
                "cosine",
                "multimodal",
                "sha256:old-profile",
                None,
            )
            .unwrap();
        }
        assert!(read_chunk_vector(conn, "c-stranded").unwrap().is_some());

        // The new profile arrives, and only `c-resent` is re-embedded.
        write_chunk_embedding(
            conn,
            "sha256:new-embedding",
            "sha256:t-shared",
            "c-resent",
            &basis_vector(1),
            CHUNK_VEC_DIMENSIONS as u64,
            "cosine",
            "multimodal",
            "sha256:new-profile",
            None,
        )
        .unwrap();

        assert!(
            read_chunk_vector(conn, "c-stranded").unwrap().is_none(),
            "a chunk whose backing embedding was evicted must lose its derived \
             chunk_vec row, not keep ranking from a retired profile"
        );
        assert_eq!(
            read_chunk_vector(conn, "c-resent").unwrap(),
            Some(f32_from_le_bytes(&basis_vector(1))),
            "the re-sent chunk carries the new profile's vector"
        );
    }

    #[test]
    fn held_publication_guard_and_materialized_helper_preserve_secret_hold_control() {
        let mut store = schema_conn();
        store
            .index_chunk(&chunk_row("c-budget", "sha256:t-shared"))
            .unwrap();
        store
            .index_chunk(&chunk_row("c-secret", "sha256:t-shared"))
            .unwrap();
        let conn = store.connection();
        write_chunk_embedding(
            conn,
            "sha256:shared",
            "sha256:t-shared",
            "c-budget",
            &basis_vector(0),
            CHUNK_VEC_DIMENSIONS as u64,
            "cosine",
            "multimodal",
            "sha256:profile",
            Some("a"),
        )
        .unwrap();
        conn.execute_batch("DELETE FROM chunk_vec").unwrap();

        let held = std::collections::BTreeSet::from(["c-secret".to_owned()]);
        rebuild_chunk_vec_for_contexts(
            conn,
            Some(&EmbeddingProfileSummary {
                dimensions: CHUNK_VEC_DIMENSIONS as u64,
                distance: "cosine".to_owned(),
                modality: "multimodal".to_owned(),
                profile_hash: "sha256:profile".to_owned(),
            }),
            &held,
            &BTreeMap::from([
                ("c-budget".to_owned(), Some("a".to_owned())),
                ("c-secret".to_owned(), Some("a".to_owned())),
            ]),
        )
        .unwrap();
        let materialized = |chunk_id: &str| {
            conn.prepare("SELECT 1 FROM chunk_vec WHERE chunk_id = ?1 LIMIT 1")
                .unwrap()
                .query_row(params![chunk_id], |_| Ok(()))
                .is_ok()
        };
        assert!(materialized("c-budget"));
        assert!(!materialized("c-secret"));

        let linked = link_chunk_vec_unless_held(
            conn,
            "c-secret",
            &basis_vector(0),
            CHUNK_VEC_DIMENSIONS as u64,
            &held,
        )
        .unwrap();
        assert!(!linked);
        assert!(read_chunk_vector(conn, "c-secret").unwrap().is_none());

        let linked = link_chunk_vecs_to_content_vector(
            conn,
            "sha256:shared",
            ["c-secret"].iter().copied(),
            &std::collections::BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(linked, ["c-secret"]);
        assert!(read_chunk_vector(conn, "c-secret").unwrap().is_some());
    }

    #[test]
    fn incompatible_dimension_links_are_reported_as_no_op() {
        let store = schema_conn();
        let conn = store.connection();
        let dimensions = 512_u64;
        let mut values = vec![0_f32; dimensions as usize];
        values[0] = 1.0;
        let vector = f32_to_le_bytes(&values);
        let held = BTreeSet::new();

        let linked =
            link_chunk_vec_unless_held(conn, "c-direct", &vector, dimensions, &held).unwrap();
        assert!(!linked);
        assert!(read_chunk_vector(conn, "c-direct").unwrap().is_none());

        insert_raw_embedding(
            conn,
            "sha256:wrong-dim",
            "sha256:t-wrong-dim",
            &vector,
            dimensions,
        );
        let linked = link_chunk_vecs_to_content_vector(
            conn,
            "sha256:wrong-dim",
            ["c-fan-a", "c-fan-b"].iter().copied(),
            &held,
        )
        .unwrap();
        assert!(linked.is_empty());
        assert!(read_chunk_vector(conn, "c-fan-a").unwrap().is_none());
        assert!(read_chunk_vector(conn, "c-fan-b").unwrap().is_none());
    }

    #[test]
    fn rebuild_chunk_vec_requires_every_persisted_profile_field() {
        let mut store = schema_conn();
        store
            .index_chunk(&chunk_row("c-profile", "sha256:t-profile"))
            .unwrap();
        let conn = store.connection();
        let expected = EmbeddingProfileSummary {
            dimensions: CHUNK_VEC_DIMENSIONS as u64,
            distance: "cosine".to_owned(),
            modality: "multimodal".to_owned(),
            profile_hash: "sha256:expected".to_owned(),
        };
        let mismatches = [
            (512_i64, "cosine", "multimodal", "sha256:expected"),
            (
                CHUNK_VEC_DIMENSIONS as i64,
                "dot",
                "multimodal",
                "sha256:expected",
            ),
            (
                CHUNK_VEC_DIMENSIONS as i64,
                "cosine",
                "text",
                "sha256:expected",
            ),
            (
                CHUNK_VEC_DIMENSIONS as i64,
                "cosine",
                "multimodal",
                "sha256:other",
            ),
        ];
        for (index, (dimensions, distance, modality, profile_hash)) in
            mismatches.into_iter().enumerate()
        {
            conn.execute(
                "INSERT INTO embeddings(id, target_type, target_id, modality, vector, dimensions, distance, profile_hash, context_key)
                 VALUES (?1, 'chunk', 'sha256:t-profile', ?2, ?3, ?4, ?5, ?6, 'a')",
                params![
                    format!("sha256:mismatch-{index}"),
                    modality,
                    basis_vector(0),
                    dimensions,
                    distance,
                    profile_hash,
                ],
            )
            .unwrap();
        }

        let contexts = BTreeMap::from([("c-profile".to_owned(), Some("a".to_owned()))]);
        rebuild_chunk_vec_for_contexts(conn, Some(&expected), &BTreeSet::new(), &contexts).unwrap();
        assert!(read_chunk_vector(conn, "c-profile").unwrap().is_none());

        rebuild_chunk_vec_for_contexts(conn, None, &BTreeSet::new(), &contexts).unwrap();
        assert_eq!(chunk_vec_count(conn).unwrap(), 0);
    }

    #[test]
    fn rebuild_image_vec_requires_complete_profile_and_rejects_duplicates() {
        let store = schema_conn();
        let conn = store.connection();
        let expected = EmbeddingProfileSummary {
            dimensions: CHUNK_VEC_DIMENSIONS as u64,
            distance: "cosine".to_owned(),
            modality: "multimodal".to_owned(),
            profile_hash: "sha256:expected".to_owned(),
        };
        conn.execute(
            "INSERT INTO embeddings(id, target_type, target_id, modality, vector, dimensions, distance, profile_hash, context_key)
             VALUES ('sha256:image-wrong', 'image', 'sha256:image', 'multimodal', ?1, ?2, 'cosine', 'sha256:other', NULL)",
            params![basis_vector(0), CHUNK_VEC_DIMENSIONS as i64],
        )
        .unwrap();
        rebuild_image_vec(conn, Some(&expected)).unwrap();
        assert_eq!(image_vec_count(conn).unwrap(), 0);

        for id in ["sha256:image-good-a", "sha256:image-good-b"] {
            conn.execute(
                "INSERT INTO embeddings(id, target_type, target_id, modality, vector, dimensions, distance, profile_hash, context_key)
                 VALUES (?1, 'image', 'sha256:image', 'multimodal', ?2, ?3, 'cosine', 'sha256:expected', NULL)",
                params![id, basis_vector(0), CHUNK_VEC_DIMENSIONS as i64],
            )
            .unwrap();
        }
        let error = rebuild_image_vec(conn, Some(&expected)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("multiple compatible image embeddings")
        );
        assert_eq!(
            image_vec_count(conn).unwrap(),
            0,
            "savepoint rolls back partial image replay"
        );
    }

    #[test]
    fn adopted_dimension_links_report_publication() {
        let store = schema_conn();
        let conn = store.connection();
        let vector = basis_vector(0);
        let held = BTreeSet::new();

        let linked = link_chunk_vec_unless_held(
            conn,
            "c-direct",
            &vector,
            CHUNK_VEC_DIMENSIONS as u64,
            &held,
        )
        .unwrap();
        assert!(linked);
        assert!(read_chunk_vector(conn, "c-direct").unwrap().is_some());

        insert_raw_embedding(
            conn,
            "sha256:adopted-dim",
            "sha256:t-adopted-dim",
            &vector,
            CHUNK_VEC_DIMENSIONS as u64,
        );
        let linked = link_chunk_vecs_to_content_vector(
            conn,
            "sha256:adopted-dim",
            ["c-fan-a", "c-fan-b"].iter().copied(),
            &held,
        )
        .unwrap();
        assert_eq!(linked, ["c-fan-a", "c-fan-b"]);
        assert!(read_chunk_vector(conn, "c-fan-a").unwrap().is_some());
        assert!(read_chunk_vector(conn, "c-fan-b").unwrap().is_some());
    }

    #[test]
    fn write_read_and_knn_orders_by_distance() {
        let store = schema_conn();
        let conn = store.connection();
        write_basis(conn, "c-axis0", "sha256:t0", 0);
        write_basis(conn, "c-axis1", "sha256:t1", 1);
        assert_eq!(chunk_vec_count(conn).unwrap(), 2);

        // Query along axis 0: c-axis0 is nearest.
        let knn = knn_chunk_distances(conn, &basis_vector(0), 10).unwrap();
        assert_eq!(knn[0].0, "c-axis0");
        assert!(knn[0].1 <= knn[1].1);

        // MMR read round-trips the stored vector.
        let read = read_chunk_vector(conn, "c-axis0").unwrap().unwrap();
        assert_eq!(read.len(), CHUNK_VEC_DIMENSIONS);
        assert_eq!(read[0], 1.0);
    }

    #[test]
    fn knn_tie_break_is_chunk_id_ascending() {
        // Two chunks with the identical vector produce identical distance; the KNN
        // caller re-breaks ties by chunk_id ascending, so the raw rows just need to
        // both be returned (order among equal distances is then deterministic once
        // re-sorted by (distance, chunk_id)).
        let store = schema_conn();
        let conn = store.connection();
        write_basis(conn, "c-zzz", "sha256:tz", 0);
        write_basis(conn, "c-aaa", "sha256:ta", 0);
        let mut knn = knn_chunk_distances(conn, &basis_vector(0), 10).unwrap();
        knn.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        assert_eq!(knn[0].0, "c-aaa");
        assert_eq!(knn[1].0, "c-zzz");
        assert!((knn[0].1 - knn[1].1).abs() < 1e-9);
    }

    #[test]
    fn r10_1_knn_k_ceiling_is_4096_and_capped_k_works_over_large_table() {
        // R10-1: sqlite-vec rejects a KNN `k` above its hard 4096 ceiling. Pre-fix the
        // CLI passed the FULL chunk_vec row count as `k`, so a scope with >4096
        // embedded chunks exploded the whole (multi-scope) search with a spurious
        // CONFIG-SCHEMA exit 2. The fix caps `k` at 4096; this proves both halves —
        // the raw ceiling AND that a capped query still succeeds over a >4096 table.
        let store = schema_conn();
        let conn = store.connection();
        let total: u64 = 4200;
        for i in 0..total {
            let axis = (i as usize) % CHUNK_VEC_DIMENSIONS;
            write_basis(conn, &format!("c{i}"), &format!("sha256:t{i}"), axis);
        }
        assert_eq!(chunk_vec_count(conn).unwrap(), total);
        let query = basis_vector(0);
        // Asking for k == total (> 4096) is rejected: this is the crash the CLI hit.
        assert!(
            knn_chunk_distances(conn, &query, total).is_err(),
            "sqlite-vec must reject a k above its 4096 ceiling"
        );
        // The R10-1 cap (`total.min(4096)`) succeeds and returns the ceiling of rows.
        let capped = total.min(4096);
        let knn = knn_chunk_distances(conn, &query, capped).unwrap();
        assert_eq!(knn.len() as u64, capped);
    }

    #[test]
    fn ct3_embed_005_rebuild_chunk_vec_from_embeddings() {
        let mut store = schema_conn();
        // chunks feed chunk_vec on rebuild via the text_hash join.
        store.index_chunk(&chunk_row("c1", "sha256:t0")).unwrap();
        store.index_chunk(&chunk_row("c2", "sha256:t1")).unwrap();
        let conn = store.connection();
        write_basis(conn, "c1", "sha256:t0", 0);
        write_basis(conn, "c2", "sha256:t1", 1);

        // Drop the derived table, then rebuild it from embeddings (source of truth).
        conn.execute_batch("DELETE FROM chunk_vec;").unwrap();
        assert_eq!(chunk_vec_count(conn).unwrap(), 0);
        rebuild_chunk_vec_for_contexts(
            conn,
            Some(&EmbeddingProfileSummary {
                dimensions: CHUNK_VEC_DIMENSIONS as u64,
                distance: "cosine".to_owned(),
                modality: "multimodal".to_owned(),
                profile_hash: "sha256:profile".to_owned(),
            }),
            &std::collections::BTreeSet::new(),
            &BTreeMap::from([
                ("c1".to_owned(), Some("a".to_owned())),
                ("c2".to_owned(), Some("a".to_owned())),
            ]),
        )
        .unwrap();
        assert_eq!(chunk_vec_count(conn).unwrap(), 2);
        let knn = knn_chunk_distances(conn, &basis_vector(1), 10).unwrap();
        assert_eq!(knn[0].0, "c2");
    }
}
