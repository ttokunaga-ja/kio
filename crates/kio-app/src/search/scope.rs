use crate::embedding::admission::embedding_execution;
use crate::historical_reindex::retained_history_instances;
use crate::image_authority;
use crate::management::binding_for_repo;
use crate::management::open_existing_managed_root;
use crate::multi_scope;
use crate::object_persistence::pipeline_to_kio;
use crate::response::search::ExcludedScopeResponse;
use crate::search_history::SearchHistoryBinding;
use crate::search_history::plan_search_history;
use crate::search_time::TimeSelector;
use crate::search_time::TimeSelectorFlags;
use crate::settings::cache_home;
use kio_adapter::catalog::adopted_embedding_profile;
use kio_adapter::catalog::embedding_adapter_profile_with_control;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::history::HistoryReader;
use kio_core::purge::PurgeState;
use kio_core::scope::Repository;
use kio_index::ChunkRow;
use kio_index::embedding_store;
use kio_index::fts::CHUNK_VEC_DIMENSIONS;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_search::query::ScopeSelectionMode;
use kio_search::query::SearchMode;
use kio_search::rrf::RrfConfig;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(crate) struct ScopeTarget {
    pub(crate) repo_root: PathBuf,
    pub(crate) kio_dir: PathBuf,
    pub(crate) scope_id: String,
}

/// A live chunk plus its scope/snapshot metadata. Used by the Evidence Pointer
/// resolution path (short-hash lookup). Direct `kio search` never materializes
/// these; its ranked candidates and Evidence Pointer metadata come from
/// `aggregator.sqlite`.
#[derive(Debug, Clone)]
pub(crate) struct SearchableChunk {
    pub(crate) row: ChunkRow,
    pub(crate) scope_id: String,
    pub(crate) scope_path: PathBuf,
    pub(crate) snapshot_at: String,
    pub(crate) path_at_commit: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedSearch {
    pub(crate) query: String,
    pub(crate) requested_mode: SearchMode,
    /// R11-7: whether a `--text`/`--vector`/`--hybrid` flag set `requested_mode`
    /// explicitly. When false, `[search].default_mode` supplies the mode instead.
    pub(crate) explicit_mode: bool,
    pub(crate) scope: Option<PathBuf>,
    pub(crate) descendants: bool,
    pub(crate) all_scopes: bool,
    pub(crate) limit: u64,
    pub(crate) offset: Option<u64>,
    pub(crate) cursor: Option<String>,
    pub(crate) time_selector_flags: TimeSelectorFlags,
    /// Select remote processing for this invocation; a matching explicit
    /// device grant remains required. Mutually exclusive with `offline`.
    pub(crate) online: bool,
    /// PC5: forces the new-send prohibition for this invocation regardless of
    /// any recorded approval — auto/`--hybrid` fall back to text
    /// (`fallback_reason="offline"`), `--vector` explicit errors.
    pub(crate) offline: bool,
}

/// Resolved search mode plus the honest fallback reporting fields (05 §1.1/§1.7).
pub(crate) struct ResolvedMode {
    pub(crate) requested: SearchMode,
    pub(crate) resolved: SearchMode,
    pub(crate) fallback: bool,
    pub(crate) fallback_reason: Option<String>,
    pub(crate) error_code: Option<String>,
    /// PC3 / §R note-1 ruling (2026-07-22): `warnings[]` is the array form the
    /// 05 §1.1 regnorm text specifies ("構造化 warning を...`warnings[]` へ出す")
    /// — the pre-ruling singular `warning: Option<String>` field is retired.
    /// Empty when `[search].fail_behavior = "warn"` did not fire (the silent
    /// `fallback` default, or no fallback at all); MVP is pre-freeze so this is
    /// a breaking `--json` shape change, not a compatibility shim.
    pub(crate) warnings: Vec<String>,
}

/// R11-7: `[search].fail_behavior` (config.schema.json §search) — what an auto or
/// `--hybrid` search does when no compatible vector backend is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchFailBehavior {
    /// Default (05 §1.7): silently fall back to text.
    Fallback,
    /// Fall back to text but surface a `warnings[]` entry in the response.
    Warn,
    /// Hard error, identical to the explicit `--vector` path (KIO-E-SEARCH-VEC-*).
    Error,
}

/// Vector backend availability across the searched scopes (the K4 embedding
/// seam, now live). Resolved from the actual per-scope embedding state and query
/// embedding availability (03 §7 / 05 §1.1).
///
/// PC1 (05 §1.1 L25-36): variant order here mirrors the spec's 7-line
/// resolution order, which is also judgment order — `resolve_search_mode`
/// consults the first-listed condition that holds. `Offline` and
/// `Unauthorized` are pulled out of the old single `Unavailable` bucket
/// because PC2 gives them different `fail_behavior` semantics (never
/// escalated to a hard error under `fail_behavior = "error"`, unlike every
/// `Unavailable`/`Incompatible` reason).
pub(crate) enum VectorAvailability {
    /// Every searched scope has a compatible embedding index and a query
    /// embedding is obtainable → hybrid is offered.
    Available,
    /// PC1 line (a) / PC5: `--offline` was given for this invocation — no
    /// query embedding is sent regardless of any recorded approval. No
    /// dedicated `error_code` (05 §1.1 names one only for INCOMPAT/
    /// UNAUTHORIZED); `--vector` explicit still hard-errors
    /// (KIO-E-SEARCH-VEC-UNAVAIL-001, the shared fallback code).
    Offline,
    /// PC1 line (b): embedding present but the profile is incompatible, or
    /// scopes disagree on embedding profile (03 §7 / 05 §1.8(5)) → text
    /// fallback + fallback_reason (KIO-E-SEARCH-VEC-INCOMPAT-001).
    Incompatible,
    /// PC1 line (c) / PC4/PC6: no participating scope satisfies the exact
    /// device+scope send grant (05 §1.1 — an OR across scopes). `--online`
    /// selects an online adapter but never creates that grant.
    /// KIO-E-SEARCH-VEC-UNAUTHORIZED-001.
    Unauthorized,
    /// PC1 lines (d)-(f): a technical, transient/structural unavailability —
    /// `reason` names the actual cause (05 §1.7 fallback_reason):
    /// `embedding_endpoint_not_configured` (no adapter env/config at all),
    /// `embedding_index_missing` (endpoint configured but no searched scope
    /// carries chunk embeddings), `query_embedding_unavailable` (endpoint +
    /// index fine, but the query embedding could not be computed), or
    /// `embedding_in_flight` (04 §5.4 §H / CL54: another process already
    /// holds a live claim on this exact query's device row — page 1 only) or
    /// `embedding_contract_violation` (PC7 — the adapter response failed its
    /// 07 §5.3 acceptance check). PC2: unlike `Offline`/`Unauthorized`, ALL of
    /// these (including `Incompatible` above) are subject to `fail_behavior`
    /// for auto/`--hybrid`.
    Unavailable { reason: &'static str },
}

/// One scope's chunk-embedding disposition (03 §7 compat).
#[derive(Clone, Copy)]
pub(crate) enum ScopeEmbedState {
    Compatible,
    Incompatible,
    Absent,
}

/// The vector space the ACTIVE embedding adapter writes into, which is what a
/// stored vector has to match to be usable (03 §7).
///
/// This used to name the adopted online profile unconditionally. With a second
/// implementation that is wrong in the direction that matters: a scope indexed
/// by the offline adapter stores the offline profile hash, and comparing it
/// against Gemini's reports every one of its vectors as incompatible — the
/// index is fine and the expectation is what is stale.
///
/// The hash comes from the ADAPTER's own profile, not from
/// [`declared_embedding_profile`]. The two differ on purpose for the
/// `IncompatibleProfile` seam, which simulates a stale or foreign index by
/// declaring a hash the adapter would never produce — reading the declared
/// value here would make that seam agree with itself and report compatible.
/// The adapter's profile is the honest answer to "what space do vectors
/// written right now land in".
///
/// When nothing is active there is no space to be compatible WITH, so the
/// online profile stays the answer; `resolve_vector_availability` has already
/// declined on `endpoint_configured` before this can matter.
fn active_embedding_profile_summary(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    control: &ControlHandle,
) -> embedding_store::EmbeddingProfileSummary {
    let profile_hash = embedding_execution(adapter_settings, control)
        .and_then(|execution| {
            embedding_adapter_profile_with_control(adapter_settings, execution, control).ok()
        })
        .map_or_else(
            || adopted_embedding_profile().tool_profile_hash,
            |profile| profile.tool_profile_hash,
        );
    embedding_store::EmbeddingProfileSummary {
        dimensions: CHUNK_VEC_DIMENSIONS as u64,
        distance: "cosine".to_owned(),
        modality: "multimodal".to_owned(),
        profile_hash,
    }
}

/// Read the already-projected embedding profile summary rather than reopening a
/// scope's source index.  A replica header is written atomically with its
/// searchable rows, so this is the same compatibility fact a direct search
/// needs for vector-mode resolution.
pub(crate) fn replica_embedding_state(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    header: Option<&kio_index::aggregator::AggScopeHeader>,
    control: &ControlHandle,
) -> ScopeEmbedState {
    let Some(header) = header else {
        return ScopeEmbedState::Absent;
    };
    if header.index_status != kio_index::aggregator::AggIndexStatus::Ready
        || header.embedding_profiles.is_empty()
    {
        return ScopeEmbedState::Absent;
    }
    let expected = active_embedding_profile_summary(adapter_settings, control);
    if header
        .embedding_profiles
        .iter()
        .all(|profile| profile.matches_profile(&expected))
    {
        ScopeEmbedState::Compatible
    } else {
        ScopeEmbedState::Incompatible
    }
}

/// K4: aggregate embedding availability across the searched scopes (PC1, 05
/// §1.1's 7-line resolution order — this function's own `if`/`else if` chain
/// IS that judgment order, first-listed-condition-wins per PC1's "解決順の
/// 列挙は判定順序でもある"). Vector search is offered only when: not
/// `--offline`; the endpoint is configured; every searched scope has a
/// compatible embedding index (03 §7, cross-scope inconsistency also counts
/// as incompatible per 05 §1.8(5)); the send consent gate is satisfied
/// (PC4 — OR across scopes, computed by the caller via
/// `embedding_opt_in_for_scopes`); AND a query embedding is obtainable.
pub(crate) fn resolve_vector_availability(
    requested: SearchMode,
    scope_embedding_states: &[ScopeEmbedState],
    offline: bool,
    endpoint_configured: bool,
    embedding_opt_in: bool,
    query_embeddable: bool,
) -> VectorAvailability {
    // PC1 line (a) / PC5: checked first, unconditionally — offline is a user
    // decision, not a technical probe result.
    if offline {
        return VectorAvailability::Offline;
    }
    if !endpoint_configured {
        return VectorAvailability::Unavailable {
            reason: "embedding_endpoint_not_configured",
        };
    }
    let mut any_compatible = false;
    let mut any_incompatible = false;
    let mut any_absent = false;
    for state in scope_embedding_states {
        match state {
            ScopeEmbedState::Compatible => any_compatible = true,
            ScopeEmbedState::Incompatible => any_incompatible = true,
            ScopeEmbedState::Absent => any_absent = true,
        }
    }
    // PC52 (05 §1.8 L390): explicit `--vector` does not fold a per-scope
    // profile mismatch into this device-wide aggregate the way auto/
    // `--hybrid` do (PC1 line (b), unchanged below) — as long as at least
    // one scope IS compatible, resolution proceeds past the `Incompatible`
    // branch here, and replica-header preparation's own per-scope compat gate
    // excludes just the mismatched scope(s) instead of this function
    // forcing a device-wide `Incompatible` that would hard-error the WHOLE
    // command (PC30's existing single-scope `--vector` + incompatible hard
    // error is unaffected: with only one scope, "at least one compatible"
    // failing IS "zero compatible", so it still falls through to
    // `Incompatible` below exactly as before).
    let vector_explicit_partial_compat =
        requested == SearchMode::Vector && any_compatible && (any_incompatible || any_absent);
    // PC1 line (b): profile incompatibility precedes the PC1 line (c) consent
    // check below (an incompatible scope's approval state is moot).
    if (any_incompatible || (any_compatible && any_absent)) && !vector_explicit_partial_compat {
        VectorAvailability::Incompatible
    } else if !any_compatible {
        VectorAvailability::Unavailable {
            reason: "embedding_index_missing",
        }
    } else if !embedding_opt_in {
        // PC1 line (c) / PC4/PC6: a compatible index exists, but sending the
        // query embedding needs the OR-across-scopes send consent gate (07
        // §3). Without it, offer text, never a send — this is a user-consent
        // gap, not a technical failure (PC2).
        VectorAvailability::Unauthorized
    } else if !query_embeddable {
        VectorAvailability::Unavailable {
            reason: "query_embedding_unavailable",
        }
    } else {
        VectorAvailability::Available
    }
}

pub(crate) fn resolve_search_mode(
    requested: SearchMode,
    vector: &VectorAvailability,
    fail_behavior: SearchFailBehavior,
) -> Result<ResolvedMode> {
    let vector_ok = matches!(vector, VectorAvailability::Available);
    // PC2: whether this cause is subject to `fail_behavior` at all for
    // auto/`--hybrid`. `Offline` and `Unauthorized` are user-intent
    // degradations ("ユーザー意思由来の text fallback は fail_behavior の対象外
    // である") — always a silent text fallback for auto/`--hybrid`, never
    // escalated to a hard error by `fail_behavior = "error"`. Every other
    // cause (`Incompatible`, and every `Unavailable` reason including
    // `embedding_in_flight`/`embedding_contract_violation`) is a technical
    // failure and IS governed by `fail_behavior`.
    let (reason, error_code, user_intent) = match vector {
        VectorAvailability::Available => (None, None, false),
        VectorAvailability::Offline => (Some("offline".to_owned()), None, true),
        VectorAvailability::Unauthorized => (
            Some("embedding_not_authorized".to_owned()),
            Some("KIO-E-SEARCH-VEC-UNAUTHORIZED-001".to_owned()),
            true,
        ),
        VectorAvailability::Incompatible => (
            Some("embedding_profile_incompatible".to_owned()),
            Some("KIO-E-SEARCH-VEC-INCOMPAT-001".to_owned()),
            false,
        ),
        VectorAvailability::Unavailable { reason } => (
            Some((*reason).to_owned()),
            Some("KIO-E-SEARCH-VEC-UNAVAIL-001".to_owned()),
            false,
        ),
    };
    // The hard-error envelope shared by explicit `--vector` and R11-7's
    // `fail_behavior = "error"` (same error_code taxonomy, 03 §7 / 05 §1.2).
    // `--vector` explicit errors unconditionally for EVERY non-Available cause
    // (05 §1.2), including offline/unauthorized — only auto/`--hybrid` treat
    // those two as immune to escalation.
    let vector_unavailable_error = || {
        // R23-14(a) (05 §1.8 L425 / 06 §7 L330 "INCOMPAT → exit 8"): an
        // incompatible embedding profile is `IncompatibleProfile` (8) even
        // through this shared hard-error envelope -- every other cause here
        // (UNAVAIL / UNAUTHORIZED reached via `fail_behavior = "error"`)
        // keeps the generic exit 1.
        let exit = if error_code.as_deref() == Some("KIO-E-SEARCH-VEC-INCOMPAT-001") {
            ExitCode::IncompatibleProfile
        } else {
            ExitCode::Failure
        };
        KioError::new(
            error_code
                .as_deref()
                .unwrap_or("KIO-E-SEARCH-VEC-UNAVAIL-001"),
            "vector search requested but no compatible embedding index is available",
            json!({ "fallback_reason": reason }),
            exit,
        )
    };
    match requested {
        SearchMode::Text => Ok(ResolvedMode {
            requested,
            resolved: SearchMode::Text,
            fallback: false,
            fallback_reason: None,
            error_code: None,
            warnings: Vec::new(),
        }),
        SearchMode::Vector => {
            if vector_ok {
                Ok(ResolvedMode {
                    requested,
                    resolved: SearchMode::Vector,
                    fallback: false,
                    fallback_reason: None,
                    error_code: None,
                    warnings: Vec::new(),
                })
            } else {
                // --vector with no usable vector backend is a hard error (05 §1.2);
                // the code distinguishes "unavailable" from "incompatible" (03 §7).
                Err(vector_unavailable_error())
            }
        }
        SearchMode::Auto | SearchMode::Hybrid => {
            if vector_ok {
                Ok(ResolvedMode {
                    requested,
                    resolved: SearchMode::Hybrid,
                    fallback: false,
                    fallback_reason: None,
                    error_code: None,
                    warnings: Vec::new(),
                })
            } else if user_intent {
                // PC2: offline/unauthorized always silently fall back for
                // auto/--hybrid, regardless of fail_behavior.
                Ok(ResolvedMode {
                    requested,
                    resolved: SearchMode::Text,
                    fallback: true,
                    fallback_reason: reason,
                    error_code,
                    warnings: Vec::new(),
                })
            } else {
                // R11-7: auto / --hybrid vector-unavailable behavior for every
                // TECHNICAL cause is governed by `[search].fail_behavior`
                // (default = silent text fallback).
                match fail_behavior {
                    // The user asked for vectors and declared "error on failure" — make
                    // it the same hard error the explicit --vector path already returns,
                    // instead of a silent exit-0 text result.
                    SearchFailBehavior::Error => Err(vector_unavailable_error()),
                    // Fall back to text but surface a loud warnings[] entry (PC3 / §R
                    // note-1 ruling: array, not a singular field).
                    SearchFailBehavior::Warn => {
                        let warning = format!(
                            "vector search unavailable ({}); fell back to text",
                            reason.as_deref().unwrap_or("unknown")
                        );
                        Ok(ResolvedMode {
                            requested,
                            resolved: SearchMode::Text,
                            fallback: true,
                            fallback_reason: reason,
                            error_code,
                            warnings: vec![warning],
                        })
                    }
                    // Default: silent text fallback (05 §1.7), warnings[] stays empty.
                    SearchFailBehavior::Fallback => Ok(ResolvedMode {
                        requested,
                        resolved: SearchMode::Text,
                        fallback: true,
                        fallback_reason: reason,
                        error_code,
                        warnings: Vec::new(),
                    }),
                }
            }
        }
    }
}

/// One scope's live search state for a single page of a query.
pub(crate) struct ExecScope {
    pub(crate) target: ScopeTarget,
    /// Commit whose tree fixes the live chunk set (05 §1.5). HEAD on page 1,
    /// the frozen sub-cursor commit on later pages.
    pub(crate) snapshot_commit: Option<String>,
    /// `rowid <= max_rowid` freezes the chunk set across pages (CT3-CURSOR-002).
    pub(crate) max_rowid: Option<u64>,
    /// Association append boundary frozen by a v2 cursor.
    pub(crate) max_association_rowid: Option<u64>,
    /// Effective per-scope chunking config frozen by a v2 cursor.
    pub(crate) chunking_config_hash: Option<String>,
    /// PC19/PC21: the scope's `index_generation` ULID frozen by a v2 cursor.
    /// `None` on a fresh page 1 (nothing to compare against yet).
    pub(crate) index_generation: Option<String>,
    pub(crate) from_cursor: bool,
}

/// What a result row POINTS AT — 05 §1.7's `result_type` (2026-07-26).
///
/// The distinction is not cosmetic. An image row and the chunk row that cites
/// it carry the SAME `chunk_hash`, because 05 §1.7 anchors an image's
/// `evidence_pointer` to its referencing chunk (an object URI has no commit,
/// tree or path, so it satisfies neither time-travel nor `evidence verify`).
/// So `chunk_hash` stopped being a row identity the moment images could rank:
/// it is what the row CITES, and this is what the row IS.
///
/// Everything downstream keeps working off `chunk_hash` on purpose — the
/// evidence pointer, `max_per_raw_hash`'s counting key (05 §1.4: "1 result 行 =
/// 1 evidence_pointer = 1 raw_hash"), the alias expansion, and the MMR
/// tie-break all want the citing chunk. Only two things want the row's own
/// identity: the vector rank (an image is ranked by ITS vector, not the
/// chunk's) and `payload_uri`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResultPayload {
    /// The row is the chunk its pointer names; `evidence_uri` already points
    /// at the payload, so 05 §1.7 omits `payload_uri` here.
    Chunk,
    /// The row is an image object cited by that chunk.
    Image {
        /// `objects/image/` content hash — the `image_vec` key.
        image_hash: String,
        /// The URI **exactly as the chunk body writes it** (05 §1.7): never
        /// re-derived from the live `scope_id`, so a fork copy carrying the
        /// original scope's id round-trips unchanged and stays resolvable by
        /// hash.
        image_uri: String,
    },
}

impl ResultPayload {
    /// 05 §1.7's `result_type`, which is required on every row.
    pub(crate) fn result_type(&self) -> &'static str {
        match self {
            Self::Chunk => "chunk",
            Self::Image { .. } => kio_search::object_uri::IMAGE_OBJECT_TYPE,
        }
    }

    /// The row's own identity in the ranking pipeline, distinct from the
    /// `chunk_hash` it cites. Used as the vector lane's key and as the final
    /// sort's last tie-break, so an image and its citing chunk can never
    /// collapse into one another.
    pub(crate) fn row_id<'a>(&'a self, chunk_hash: &'a str) -> &'a str {
        match self {
            Self::Chunk => chunk_hash,
            Self::Image { image_hash, .. } => image_hash,
        }
    }
}

/// A candidate materialized by the device-level replica.
pub(crate) struct ScoredCandidate {
    pub(crate) scope_index: usize,
    pub(crate) scope_id: String,
    pub(crate) scope_path: PathBuf,
    /// The chunk this row CITES — for a `ResultPayload::Image` row that is the
    /// referencing chunk V6 chose, not the row's own identity. See
    /// [`ResultPayload`].
    pub(crate) chunk_hash: String,
    /// 05 §1.7: what this row points at. `Chunk` for every row until image
    /// embedding existed, which is why so much of the pipeline keys on
    /// `chunk_hash` alone.
    pub(crate) payload: ResultPayload,
    pub(crate) rrf_score: f64,
    pub(crate) meta: ChunkMeta,
    /// Deterministic path/commit aliases expanded only after global diversify.
    pub(crate) bindings: Vec<SearchHistoryBinding>,
    /// The chunk's embedding (hybrid/vector mode only), fed into MMR (05 §1.4)
    /// and the global cosine re-rank (05 §1.8). `None` in text mode, which makes
    /// MMR skip and only the raw_hash dedup run.
    pub(crate) embedding: Option<Vec<f32>>,
}

#[derive(Clone)]
pub(crate) struct ChunkMeta {
    pub(crate) raw_hash: String,
    pub(crate) tool_profile_hash: String,
    pub(crate) r#gen: u64,
    pub(crate) heading_path: Option<Vec<String>>,
    pub(crate) section_id: Option<String>,
    /// Unit-local UTF-8 byte offset (03 §8.1). NOT NULL in `chunks` — always
    /// present, unlike the Evidence Pointer wire field it feeds (optional there,
    /// 08 §2.2).
    pub(crate) byte_start: u64,
    pub(crate) byte_end: u64,
    pub(crate) text: String,
    /// Which normalized unit this chunk was cut from (03 §8.1). Carried so the
    /// result assembler can reach that unit's `metadata["images"]`, where the
    /// figures' boxes live — the chunk text names images but says nothing about
    /// how big they are.
    pub(crate) unit_key: String,
}

/// One final result hit after historical/deleted alias expansion.
pub(crate) struct ExpandedCandidate<'a> {
    pub(crate) candidate: &'a ScoredCandidate,
    pub(crate) binding: &'a SearchHistoryBinding,
}

pub(crate) struct SearchedScopeInfo {
    pub(crate) scope_id: String,
    /// R23-20 (03 §4 L296): the canonical `.kio` directory — the value
    /// relayed into `searched_scopes[].scope_path` (display/hint only).
    pub(crate) scope_path: PathBuf,
    /// The scope's filesystem root (parent of `scope_path`), kept separately
    /// for functional use (`Repository::open`, `compute_index_status`) now
    /// that `scope_path` itself is the `.kio` directory, not its parent.
    pub(crate) repo_root: PathBuf,
    pub(crate) snapshot_at: String,
    pub(crate) max_rowid: u64,
    pub(crate) max_association_rowid: u64,
    pub(crate) chunking_config_hash: String,
    /// PC19/PC21: this scope's `index_metadata.index_generation` ULID at the
    /// moment it was searched — frozen into the next page's cursor.
    pub(crate) index_generation: String,
    /// An active purge journal was observed only while admitting this scope's
    /// last coherent Ready header across a HEAD/header mismatch.  Candidate
    /// selection may use that older projection solely to reach the narrow
    /// candidate barrier; it must reject the scope even if the journal finishes
    /// before that barrier opens.
    pub(crate) journal_active_at_prepare: bool,
    /// PC45/PC46 (§R note-3 ruling): shallow ancestors this scope's history
    /// walk skipped rather than hard-failing on, for `--all-history` /
    /// `--since` / `--include-deleted`. Zero for every other selector.
    pub(crate) shallow_skipped: u64,
    /// CAS-preflighted aliases for a historical selector (or any cursor
    /// replay). `None` preserves fresh current-search shallow-cache behavior;
    /// `Some`, including `Some(vec![])`, is the complete relation the replica
    /// must apply before ranking candidates.
    pub(crate) runtime_binding_filter: Option<Vec<kio_index::aggregator::AggBindingFilter>>,
}

/// `$XDG_CACHE_HOME/kio/aggregator.sqlite` — the device-level read replica both
/// lanes are scored against (05 §1.8). Cache root, not data root: it is derived
/// entirely from the scopes and costs only a re-projection to lose.
pub(crate) fn aggregator_path(control: &ControlHandle) -> PathBuf {
    cache_home(control).join("kio/aggregator.sqlite")
}

/// The replica collection frozen into a cursor page.
pub(crate) struct GlobalRanks {
    /// The collection that selected candidates, frozen into the next page's
    /// cursor so replay can reject a changed global corpus (05 §1.8).
    pub(crate) collection_generation: String,
}

/// Borrowed inputs to replica-only candidate selection for one search request.
///
/// The caller has already prepared every scope and opened the sole device
/// replica. This keeps that coherent request boundary explicit without copying
/// search state or reopening a source index.
pub(crate) struct ReplicaCandidateRequest<'a> {
    pub(crate) replica: &'a mut kio_index::aggregator::Aggregator,
    pub(crate) searched: &'a [SearchedScopeInfo],
    pub(crate) current_policies: &'a BTreeMap<String, CurrentPolicyEvaluator>,
    /// Recomputed from tree-pinned normalized units for this query. The device
    /// replica is intentionally stale between index runs, so it cannot itself
    /// authorize an image candidate after an Ignore/policy change.
    pub(crate) image_authorities: &'a BTreeMap<String, image_authority::ImageAuthorityMap>,
    pub(crate) eligible_images: &'a BTreeSet<(String, String)>,
    pub(crate) exec_scopes: &'a [ExecScope],
    pub(crate) retainable_all_scopes: &'a BTreeSet<String>,
    pub(crate) authoritative_all_scope_registry: bool,
    pub(crate) scope_mode: ScopeSelectionMode,
    pub(crate) is_cursor_replay: bool,
    pub(crate) time_selector: &'a TimeSelector,
    pub(crate) since_cutoff: Option<&'a str>,
    pub(crate) match_expr: Option<&'a str>,
    pub(crate) short_tokens: &'a [String],
    pub(crate) mode: SearchMode,
    pub(crate) query_vec: Option<&'a [f32]>,
    pub(crate) config: RrfConfig,
}

pub(crate) fn aggregator_selector(
    time_selector: &TimeSelector,
) -> kio_index::aggregator::AggSelector {
    match time_selector {
        TimeSelector::Current => kio_index::aggregator::AggSelector::Current,
        TimeSelector::At(_) => kio_index::aggregator::AggSelector::At,
        TimeSelector::AllHistory | TimeSelector::Since(_) => {
            kio_index::aggregator::AggSelector::AllHistory
        }
        TimeSelector::IncludeDeleted => kio_index::aggregator::AggSelector::IncludeDeleted,
    }
}

/// Load the descriptor-bound, current policy authority for a registered search
/// scope.  Search never trusts the registry path alone: reopening the managed
/// root gives `CurrentPolicyEvaluator` the retained handles it needs to read
/// every live ancestor safely.
pub(crate) fn current_search_policy(
    target: &ScopeTarget,
    control: &kio_core::control::ControlHandle,
) -> Result<(Repository, CurrentPolicyEvaluator)> {
    let repo = open_existing_managed_root(&target.repo_root, control)?;
    if repo.scope_identity()?.scope_id != target.scope_id {
        return Err(KioError::new(
            "KIO-E-MANAGEMENT-AUTHORITY-001",
            "registered search scope identity changed",
            json!({ "scope_id": target.scope_id }),
            ExitCode::PermanentFailure,
        ));
    }
    let binding = binding_for_repo(&repo)?;
    let record = kio_core::management::read_record(&binding)?;
    let policy =
        CurrentPolicyEvaluator::load(&binding, record.case_insensitive).map_err(pipeline_to_kio)?;
    Ok((repo, policy))
}

/// Scope-local authority failures exclude that scope before any query egress
/// or ranking. Version mismatches and unexpected errors remain command-wide
/// failures. A cursor must reject any such change instead of changing its set.
pub(crate) fn search_policy_exclusion_reason(error: &KioError) -> Option<&'static str> {
    match error.error_code() {
        "KIO-E-STORE-IO-001"
        | "KIO-E-STORE-NOT-FOUND-001"
        | "KIO-E-STORE-UNSAFE-001"
        | "KIO-E-CONFIG-SCHEMA-001" => Some("unreachable"),
        "KIO-E-STORE-CORRUPT-001" => Some("store_corrupt"),
        "KIO-E-MANAGEMENT-AUTHORITY-001" => Some("policy_unavailable"),
        _ => None,
    }
}

/// Load the retained, live policy authority for a writer operation.  Historical
/// names are policy inputs only, so this does not require a source file to
/// exist at any of those names.
pub(crate) fn current_embedding_policy(repo: &Repository) -> Result<CurrentPolicyEvaluator> {
    let binding = binding_for_repo(repo)?;
    let record = kio_core::management::read_record(&binding)?;
    CurrentPolicyEvaluator::load(&binding, record.case_insensitive).map_err(pipeline_to_kio)
}

pub(crate) fn policy_cursor_mismatch(scope_id: &str, reason: &str) -> KioError {
    KioError::new(
        "KIO-E-SEARCH-CURSOR-001",
        "search cursor current policy changed or is no longer eligible; re-run without a cursor",
        json!({ "scope_id": scope_id, "reason": reason }),
        ExitCode::InvalidUsage,
    )
}

const MAX_POLICY_BINDINGS_PER_SCOPE: usize = 100_000;

/// Intersect the selector's complete durable binding relation with current
/// policy before the replica computes ranks or applies candidate depth.  This
/// is deliberately an allow-list, including an empty one: filtering ranked
/// rows afterwards would let a denied top hit consume `limit`/depth.
pub(crate) fn apply_current_policy_binding_filters(
    replica: &kio_index::aggregator::Aggregator,
    searched: &mut [SearchedScopeInfo],
    policies: &BTreeMap<String, CurrentPolicyEvaluator>,
    time_selector: &TimeSelector,
) -> std::result::Result<(), String> {
    let selector = aggregator_selector(time_selector);
    for scope in searched {
        let policy = policies
            .get(&scope.scope_id)
            .expect("runtime searched scope has a current policy");
        policy
            .revalidate()
            .map_err(|error| format!("current policy revalidation failed: {error}"))?;
        let policy_bindings = replica
            .eligible_bindings_for_policy(
                &scope.scope_id,
                selector,
                &scope.snapshot_at,
                MAX_POLICY_BINDINGS_PER_SCOPE,
            )
            .map_err(|error| format!("policy binding enumeration failed: {error}"))?;
        let mut allowed = Vec::with_capacity(policy_bindings.len());
        for binding in policy_bindings {
            let path_allowed = policy
                .allows_path(&binding.path_at_commit)
                .map_err(|error| format!("current policy path evaluation failed: {error}"))?;
            // The binding relation stays byte-for-byte intact for the replica
            // equality join.  `current_paths` are presentation aliases and
            // are redacted after selection; requiring every alias here would
            // let one ignored rename hide an otherwise allowed result.
            if path_allowed {
                allowed.push(binding);
            }
        }
        scope.runtime_binding_filter = Some(match scope.runtime_binding_filter.take() {
            Some(history) => allowed
                .into_iter()
                .filter(|binding| history.iter().any(|historical| historical == binding))
                .collect(),
            None => allowed,
        });
    }
    Ok(())
}

pub(crate) fn revalidate_current_search_policies(
    searched: &[SearchedScopeInfo],
    policies: &BTreeMap<String, CurrentPolicyEvaluator>,
    is_cursor_replay: bool,
) -> Result<()> {
    for scope in searched {
        let policy = policies
            .get(&scope.scope_id)
            .expect("runtime searched scope has a current policy");
        if let Err(error) = policy.revalidate() {
            if is_cursor_replay {
                return Err(policy_cursor_mismatch(&scope.scope_id, "policy_changed"));
            }
            return Err(KioError::new(
                "KIO-E-SEARCH-POLICY-001",
                "current search policy changed while the response was being assembled; retry",
                json!({ "scope_id": scope.scope_id, "reason": error.to_string() }),
                ExitCode::PartialFailure,
            ));
        }
    }
    Ok(())
}

pub(crate) fn revalidate_current_policy_map(
    policies: &BTreeMap<String, CurrentPolicyEvaluator>,
    is_cursor_replay: bool,
) -> Result<()> {
    for (scope_id, policy) in policies {
        if let Err(error) = policy.revalidate() {
            if is_cursor_replay {
                return Err(policy_cursor_mismatch(scope_id, "policy_changed"));
            }
            return Err(KioError::new(
                "KIO-E-SEARCH-POLICY-001",
                "current search policy changed before query embedding admission; retry",
                json!({ "scope_id": scope_id, "reason": error.to_string() }),
                ExitCode::PartialFailure,
            ));
        }
    }
    Ok(())
}

/// Recompute image-object authority at the query boundary. Unlike chunk
/// bindings, image associations in the device replica have no source-side
/// policy predicate to intersect; returning an old `AggImage` after Ignore
/// changes would otherwise disclose a stale vector/payload until reindex.
pub(crate) fn current_search_image_authority(
    repo: &Repository,
    policy: &CurrentPolicyEvaluator,
) -> Result<image_authority::ImageAuthorityMap> {
    let Some(head) = repo.head_commit_hash()? else {
        return Ok(image_authority::ImageAuthorityMap::default());
    };
    match retained_history_instances(repo, &head) {
        Ok(retained) => image_authority::retained_image_authority(repo, &retained, policy),
        // A fresh current search is permitted to serve a Ready replica's
        // authenticated cached text when its source ancestry has become shallow.
        // It cannot authenticate image ownership from that incomplete graph, so
        // retain the text-only degradation while fail-closing every image.
        Err(error) if error.error_code() == "KIO-E-COMMIT-SHALLOW-001" => {
            Ok(image_authority::ImageAuthorityMap::default())
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn eligible_image_pairs(
    authorities: &BTreeMap<String, image_authority::ImageAuthorityMap>,
) -> BTreeSet<(String, String)> {
    authorities
        .iter()
        .flat_map(|(scope_id, authority)| {
            authority
                .allowed_hashes()
                .map(move |hash| (scope_id.clone(), hash.to_owned()))
        })
        .collect()
}

pub(crate) fn redact_current_policy_paths(
    policy: &CurrentPolicyEvaluator,
    paths: Vec<String>,
) -> kio_pipeline::Result<Vec<String>> {
    paths.into_iter().try_fold(Vec::new(), |mut allowed, path| {
        if policy.allows_path(&path)? {
            allowed.push(path);
        }
        Ok(allowed)
    })
}

/// Per-scope search failure disposition.
pub(crate) enum ScopeSearchError {
    /// The selected history start (`--at`, fresh history HEAD, or cursor
    /// snapshot) is gone (shallow), so the whole search must hard-fail.
    Shallow,
    /// Recorded in `excluded_scopes`; the overall search may still succeed.
    Excluded(String),
    /// An unexpected error that must abort the command.
    Fatal(KioError),
}

/// Exclusion reason for a scope observed mid-reindex (P10): HEAD has advanced to a
/// new generation but the rebuilt sqlite is not yet swapped in, so not one chunk is
/// live. Surfaced as `KIO-E-INDEX-REBUILDING-001` when it is the sole failure mode.
pub(crate) const INDEX_REBUILDING_REASON: &str = "index_rebuilding";

/// §I (LC52-56) exclusion reason: this scope's `ReadBarrierCheckpoint` found
/// an active purge journal, or the journal/purge-epoch/lifecycle-epoch triple
/// changed between this scope's checkpoint 1 and checkpoint 2. Surfaced as
/// `KIO-E-PURGE-JOURNAL-ACTIVE-001` when it is the sole failure mode (mirrors
/// `INDEX_REBUILDING_REASON`'s all-scopes promotion below).
pub(crate) const PURGE_JOURNAL_ACTIVE_REASON: &str = "purge_journal_active";

/// PC52/PC55(c) (05 §1.8 L390-391): this scope's chunk-embedding profile
/// does not match the currently adopted embedding profile — checked only
/// for explicit `--vector` (auto/`--hybrid` fold this into the device-wide
/// text-fallback aggregate upstream, `resolve_vector_availability`).
/// Surfaced as `KIO-E-SEARCH-VEC-INCOMPAT-001` / exit 8 when it is the sole
/// exclusion reason across every searched scope (`all_vec_incompatible` in
/// `run_search_inner`).
pub(crate) const VEC_PROFILE_INCOMPATIBLE_REASON: &str = "embedding_profile_incompatible";

/// PC52: this scope has no chunk-embedding data at all — checked only for
/// explicit `--vector`, same reasoning as `VEC_PROFILE_INCOMPATIBLE_REASON`.
/// Reuses `resolve_vector_availability`'s own `"embedding_index_missing"`
/// reason string for the device-wide equivalent of this per-scope cause.
pub(crate) const VEC_PROFILE_ABSENT_REASON: &str = "embedding_index_missing";

pub(crate) struct ScopeOutcome {
    pub(crate) snapshot_commit: String,
    pub(crate) max_rowid: u64,
    pub(crate) max_association_rowid: u64,
    pub(crate) chunking_config_hash: String,
    /// PC19/PC21: this scope's `index_metadata.index_generation` ULID as of
    /// this search (05 §1.5).
    pub(crate) index_generation: String,
    /// PC45/PC46: shallow ancestors skipped during this scope's history walk.
    pub(crate) shallow_skipped: u64,
    pub(crate) runtime_binding_filter: Option<Vec<kio_index::aggregator::AggBindingFilter>>,
    pub(crate) journal_active_at_prepare: bool,
}

/// Source-independent preparation result for the direct replica route.  It
/// deliberately contains only facts captured in `aggregator.sqlite` plus the
/// resolved `--at` commit; it never holds a source SQLite connection.
pub(crate) struct ReplicaScopeResolution {
    pub(crate) snapshot_commit: String,
    pub(crate) header: kio_index::aggregator::AggScopeHeader,
    pub(crate) runtime_binding_filter: Option<Vec<kio_index::aggregator::AggBindingFilter>>,
    pub(crate) shallow_skipped: u64,
    pub(crate) journal_active_at_prepare: bool,
}

fn scope_deadline_check(
    deadline: multi_scope::ScopeDeadline,
) -> std::result::Result<(), ScopeSearchError> {
    if deadline.is_expired() {
        Err(ScopeSearchError::Excluded("timeout".to_owned()))
    } else {
        Ok(())
    }
}

/// Resolve a scope for direct search without opening `.kio/index/sqlite.db`.
///
/// The registry/scope store is still used for scope-format validation and
/// `--at` operand resolution, but index facts (HEAD snapshot, configuration,
/// append bounds, vector profiles, and readiness) are immutable facts from
/// the device replica header.  A missing or incomplete header fails closed;
/// search must never repair it by querying the source index.
pub(crate) fn prepare_scope_from_replica_header(
    exec: &ExecScope,
    header: Option<&kio_index::aggregator::AggScopeHeader>,
    time_selector: &TimeSelector,
    since_cutoff: Option<&str>,
    deadline: multi_scope::ScopeDeadline,
    scope_delay_control: &multi_scope::ScopeDelayTestControl,
    control: &ControlHandle,
) -> std::result::Result<ReplicaScopeResolution, ScopeSearchError> {
    multi_scope::maybe_delay_scope_for_test(&exec.target.scope_id, deadline, scope_delay_control);
    scope_deadline_check(deadline)?;
    // `Repository::open_for_search` validates only the scope store; it does
    // not open the per-scope search index. A format-version incompatibility is
    // a command-wide hard stop: continuing the healthy scopes would conceal a
    // repository that this binary must not interpret.
    let repo = Repository::open_for_search_with_control(&exec.target.repo_root, control.clone())
        .map_err(|error| {
            if error.error_code() == "KIO-E-STORE-VERSION-001" {
                ScopeSearchError::Fatal(error)
            } else {
                ScopeSearchError::Excluded("unreachable".to_owned())
            }
        })?;
    let header = header
        .cloned()
        .ok_or_else(|| ScopeSearchError::Excluded("index_missing".to_owned()))?;
    // A fresh current query is served by the replica, but it must still inspect
    // the selected CAS snapshot before trusting that projection.  This catches
    // a hash-mismatched commit/tree as per-scope `store_corrupt` without opening
    // `.kio/index/sqlite.db`.  A Ready header is itself the durable replacement
    // for the old source-db "cached tree_entries" proof, so a deliberately
    // shallow current snapshot remains serviceable from that projection.  If a
    // manual snapshot advanced HEAD and the writer marked its replica Rebuilding,
    // however, a shallow selected tree has no certified projection and must be
    // reported as `snapshot_shallow`, not the less-specific rebuilding state.
    if matches!(time_selector, TimeSelector::Current)
        && !exec.from_cursor
        && let Some(snapshot_commit) = header.current_snapshot_commit.as_deref()
    {
        match HistoryReader::new_with_store(repo.kio_dir(), repo.object_store())
            .snapshot(snapshot_commit)
        {
            Ok(_) => {}
            Err(error) if error.error_code() == "KIO-E-COMMIT-SHALLOW-001" => {
                if !matches!(
                    header.index_status,
                    kio_index::aggregator::AggIndexStatus::Ready
                ) {
                    return Err(ScopeSearchError::Excluded("snapshot_shallow".to_owned()));
                }
            }
            Err(error) => return Err(ScopeSearchError::Fatal(error)),
        }
    }
    // HEAD and the device replica are separate durable stores. A writer marks
    // the prior Ready projection Rebuilding immediately after it advances
    // HEAD, but a reader can observe the tiny cross-store interval in between.
    // For every selector anchored in the current HEAD (including a replayed
    // current/history cursor), reject a Ready header whose certified snapshot
    // is not the actual CAS HEAD. An explicit `--at` is intentionally
    // independent: its own completed marker and CAS binding plan certify its
    // selected historical target instead.
    let mut journal_active_at_prepare = false;
    if !matches!(time_selector, TimeSelector::At(_))
        && matches!(
            header.index_status,
            kio_index::aggregator::AggIndexStatus::Ready
        )
    {
        let head = repo.head_commit_hash().map_err(ScopeSearchError::Fatal)?;
        if head.is_none() || header.current_snapshot_commit.as_deref() != head.as_deref() {
            // Do not install a command-wide purge preflight: only a replica
            // header which would otherwise be rejected for this exact
            // HEAD/header mismatch gets this narrow look.  A visible journal
            // means the old Ready header is the last coherent binding snapshot;
            // pass it to replica candidate selection so its existing
            // candidate-scope barrier owns the purge error and body discard.
            // Record the observation as well: the journal can finish between
            // here and the candidate barrier, but that must not make stale rows
            // eligible to cross the response boundary.
            journal_active_at_prepare =
                PurgeState::open_with_control(&exec.target.kio_dir, control.clone())
                    .map_err(ScopeSearchError::Fatal)?
                    .read_barrier_active()
                    .map_err(ScopeSearchError::Fatal)?;
            if !journal_active_at_prepare {
                return Err(ScopeSearchError::Excluded(
                    INDEX_REBUILDING_REASON.to_owned(),
                ));
            }
        }
    }
    match header.index_status {
        kio_index::aggregator::AggIndexStatus::Ready => {}
        kio_index::aggregator::AggIndexStatus::Missing => {
            return Err(ScopeSearchError::Excluded("index_missing".to_owned()));
        }
        kio_index::aggregator::AggIndexStatus::Corrupt => {
            return Err(ScopeSearchError::Excluded("index_corrupt".to_owned()));
        }
        kio_index::aggregator::AggIndexStatus::Rebuilding => {
            return Err(ScopeSearchError::Excluded(
                INDEX_REBUILDING_REASON.to_owned(),
            ));
        }
    }
    let snapshot_commit = match &exec.snapshot_commit {
        Some(commit) => commit.clone(),
        None => match time_selector.at() {
            Some(operand) => match repo.resolve_commit(operand) {
                Ok(commit) => commit,
                Err(error) if error.error_code() == "KIO-E-COMMIT-SHALLOW-001" => {
                    return Err(ScopeSearchError::Shallow);
                }
                Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {
                    return Err(ScopeSearchError::Excluded("commit_not_found".to_owned()));
                }
                Err(error) => return Err(ScopeSearchError::Fatal(error)),
            },
            None => header
                .current_snapshot_commit
                .clone()
                .ok_or_else(|| ScopeSearchError::Excluded(INDEX_REBUILDING_REASON.to_owned()))?,
        },
    };
    // Fresh current search deliberately retains its established shallow-cache
    // behavior. Every historical selector, plus any cursor replay, instead
    // derives the exact binding relation from source CAS at request time. This
    // validates a selected shallow tree and removes aliases from a tree that
    // became shallow after the writer published the replica projection,
    // without reading `.kio/index/sqlite.db`.
    let (runtime_binding_filter, shallow_skipped) =
        if matches!(time_selector, TimeSelector::Current) && !exec.from_cursor {
            (None, 0)
        } else {
            let plan = plan_search_history(&repo, &snapshot_commit, time_selector, since_cutoff)
                .map_err(|error| match error.error_code() {
                    "KIO-E-COMMIT-SHALLOW-001" => ScopeSearchError::Shallow,
                    "KIO-E-COMMIT-HISTORY-LIMIT-001" if !exec.from_cursor => {
                        ScopeSearchError::Excluded("history_limit_exceeded".to_owned())
                    }
                    _ => ScopeSearchError::Fatal(error),
                })?;
            let shallow_skipped = plan.shallow_skipped.len() as u64;
            let filters = plan
                .bindings
                .into_iter()
                .map(|binding| kio_index::aggregator::AggBindingFilter {
                    scope_id: exec.target.scope_id.clone(),
                    raw_hash: binding.raw_hash,
                    tool_profile_hash: binding.tool_profile_hash,
                    r#gen: binding.r#gen,
                    manifest_hash: binding.manifest_hash,
                    path_at_commit: binding.path_at_commit,
                    pointer_commit: binding.pointer_commit,
                    current_paths: binding.current_paths,
                    is_live: binding.is_live,
                })
                .collect();
            (Some(filters), shallow_skipped)
        };
    if exec
        .index_generation
        .as_deref()
        .is_some_and(|frozen| frozen != header.index_generation)
    {
        return Err(ScopeSearchError::Fatal(KioError::new(
            "KIO-E-SEARCH-CURSOR-001",
            "search cursor index generation changed",
            json!({
                "scope_id": exec.target.scope_id,
                "reason": "index_generation_mismatch",
            }),
            ExitCode::InvalidUsage,
        )));
    }
    scope_deadline_check(deadline)?;
    Ok(ReplicaScopeResolution {
        snapshot_commit,
        header,
        runtime_binding_filter,
        shallow_skipped,
        journal_active_at_prepare,
    })
}

#[cfg(test)]
pub(crate) fn is_vector_capacity_message(message: &str) -> bool {
    message.contains("knn query too large") || message.contains("too many SQL variables")
}

pub(crate) fn scope_all_failed_error(
    message: &str,
    excluded: Vec<ExcludedScopeResponse>,
) -> KioError {
    KioError::new(
        "KIO-E-SEARCH-SCOPE-ALL-FAILED-001",
        message,
        json!({ "excluded_scopes": excluded }),
        ExitCode::PermanentFailure,
    )
}

/// PC57 (05 §1.8 L392 / 06 §7 L362-363): whether a per-scope exclusion reason
/// belongs to the "retryable" set for the mixed-reason all-scopes-failed
/// split — exactly the reasons whose OWN homogeneous promotion is exit 3:
/// `index_rebuilding` (P10), `index_missing`/`index_corrupt` (R23-14(b) — a
/// missing/unusable sqlite.db is folded into the SAME `all_rebuilding`
/// promotion as `index_rebuilding`, 06 §7 L345-346), `purge_journal_active`
/// (§I), `timeout` (05 §1.8's per-scope timeout), and `registry_duplicate`
/// (R23-27 — `KIO-E-REGISTRY-DUP-001`, now produced by `registry_all_targets`'s
/// dedup and the cursor-replay remap, R23-25). Every other reason (store
/// corruption, `history_limit_exceeded` (R23-15), incompatible profile/format
/// version, unreachable, not-yet-indexed) is permanent here — re-running the
/// identical command will not, by itself, change the outcome.
pub(crate) fn is_retryable_scope_reason(reason: &str) -> bool {
    matches!(
        reason,
        "index_rebuilding"
            | "index_missing"
            | "index_corrupt"
            | "purge_journal_active"
            | "timeout"
            | "registry_duplicate"
    )
}

/// R17-4/R18-4: the class-specific recovery hint for a store-corruption search
/// exclusion reason (`store_corrupt` / `snapshot_shallow`), or `None` for any other
/// reason (transient reasons need no guidance). `index_missing`/`index_corrupt`
/// also answer here (their hint is folded into `all_rebuilding`'s exit-3
/// KIO-E-INDEX-REBUILDING-001 response — R23-14(b) — not a separate exit-1 path
/// as before that fix). Attached both to each individual
/// `excluded_scopes` entry (R18-4 — so a PARTIAL multi-scope exclusion, where healthy
/// scopes keep `searched` non-empty and skip the all-failed aggregate, is still
/// agent-recoverable) and aggregated into the all-scopes-failed SCOPE-ALL-FAILED
/// response (R17-4). The error CODE stays SCOPE-ALL-FAILED-001 (docs frozen — no new
/// code); only the `recovery` context is added.
pub(crate) fn store_corruption_recovery_hint(reason: &str) -> Option<&'static str> {
    match reason {
        "store_corrupt" => Some(
            "store_corrupt: try `kio repair all`; if it still fails, restore \
             the corrupt commit/tree object from objects/refs",
        ),
        "snapshot_shallow" => Some(
            "snapshot_shallow: restore the discarded HEAD commit/tree object, or \
             re-run `kio index` to rebuild the snapshot from the working tree",
        ),
        // R19-6: index_missing/index_corrupt (sqlite.db absent/damaged) are the most
        // common store-corruption class, yet R18-4 only wired the recovery hint for
        // store_corrupt/snapshot_shallow — so a partial exclusion (a healthy scope
        // survives) surfaced these reasons bare, and a heterogeneous all-failed mix
        // fell through both homogeneous aggregations to a hintless message. Same
        // repair path as the others.
        "index_missing" => Some(
            "index_missing: run `kio repair replica`; if the scope SQLite source is \
             also missing, run `kio repair all`",
        ),
        "index_corrupt" => Some(
            "index_corrupt: run `kio repair replica`; if the scope SQLite source is \
             also corrupt, run `kio repair all`",
        ),
        _ => None,
    }
}

/// R20-8: the deduplicated recovery hints for a set of excluded scopes, in a stable reason
/// order. Aggregates the per-entry `store_corruption_recovery_hint`s into the
/// all-scopes-failed response's top-level `context.recovery` — so the `index_unusable`
/// exit and a HETEROGENEOUS all-failed mix (which R18-4 left hintless) both surface
/// guidance, matching the homogeneous store-corruption branch.
pub(crate) fn aggregate_store_recovery_hints(
    excluded: &[ExcludedScopeResponse],
) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for reason in [
        "store_corrupt",
        "snapshot_shallow",
        "index_missing",
        "index_corrupt",
    ] {
        let present = excluded.iter().any(|e| e.reason == reason);
        if present
            && let Some(hint) = store_corruption_recovery_hint(reason)
            && !out.contains(&hint)
        {
            out.push(hint);
        }
    }
    out
}
