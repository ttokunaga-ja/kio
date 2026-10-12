use crate::QueryEmbeddingOutcome;
use crate::active_embedding_adapter_identity;
use crate::capture_search_budget_observation;
use crate::commands::SearchArgs;
use crate::compute_index_status;
use crate::compute_query_embedding_page1;
use crate::embedding_execution;
use crate::embedding_is_deterministic;
use crate::embedding_opt_in_for_scopes;
use crate::invocation::Invocation;
use crate::ledger_db_path;
use crate::multi_scope;
use crate::pipeline_to_kio;
use crate::scope_access::ReadBarrierCheckpoint;
use crate::scope_access::enumerate_scope_targets;
use crate::scope_access::gc_marker_active_for_scopes;
use crate::scope_access::is_store_corrupt_class;
use crate::scope_access::maybe_wait_at_test_search_response_barrier;
use crate::scope_access::recheck_search_response_barriers;
use crate::scope_access::registry_duplicate_groups;
use crate::search::projection::replica_candidates_for;
use crate::search::query::UnitFigureCache;
use crate::search::query::append_failed_search_metrics;
use crate::search::query::append_search_logs;
use crate::search::query::build_query_plan;
use crate::search::query::cursor_mode_from_selection;
use crate::search::query::cursor_query_vector_error;
use crate::search::query::diversify_merged;
use crate::search::query::effective_search_config;
use crate::search::query::effective_search_tuning;
use crate::search::query::parsed_search;
use crate::search::query::persist_query_vector_cache;
use crate::search::query::query_tokens;
use crate::search::query::query_vector_digest_hex;
use crate::search::query::read_instance_figures;
use crate::search::query::replay_query_vector;
use crate::search::query::resolve_cursor_exec_scopes;
use crate::search::query::scope_selection_from_cursor;
use crate::search::query::search_mode_json;
use crate::search::query::selector_for_search;
use crate::search::query::selector_from_cursor;
use crate::search::query::significant_related_images;
use crate::search::scope::ExecScope;
use crate::search::scope::ExpandedCandidate;
use crate::search::scope::INDEX_REBUILDING_REASON;
use crate::search::scope::PURGE_JOURNAL_ACTIVE_REASON;
use crate::search::scope::ReplicaCandidateRequest;
use crate::search::scope::ResultPayload;
use crate::search::scope::ScopeEmbedState;
use crate::search::scope::ScopeOutcome;
use crate::search::scope::ScopeSearchError;
use crate::search::scope::SearchFailBehavior;
use crate::search::scope::SearchedScopeInfo;
use crate::search::scope::VEC_PROFILE_ABSENT_REASON;
use crate::search::scope::VEC_PROFILE_INCOMPATIBLE_REASON;
use crate::search::scope::VectorAvailability;
use crate::search::scope::aggregate_store_recovery_hints;
use crate::search::scope::aggregator_path;
use crate::search::scope::aggregator_selector;
use crate::search::scope::apply_current_policy_binding_filters;
use crate::search::scope::current_search_image_authority;
use crate::search::scope::current_search_policy;
use crate::search::scope::eligible_image_pairs;
use crate::search::scope::is_retryable_scope_reason;
use crate::search::scope::policy_cursor_mismatch;
use crate::search::scope::prepare_scope_from_replica_header;
use crate::search::scope::replica_embedding_state;
use crate::search::scope::resolve_search_mode;
use crate::search::scope::resolve_vector_availability;
use crate::search::scope::revalidate_current_search_policies;
use crate::search::scope::scope_all_failed_error;
use crate::search::scope::search_policy_exclusion_reason;
use crate::search::scope::store_corruption_recovery_hint;
use crate::search_time::reconcile_cursor_selector;
use crate::search_time::since_cutoff_utc;
use crate::search_time::validate_cursor_cutoff;
use crate::search_to_kio;
use crate::settings::cursor_signing_key;
use crate::settings::related_images_min_area_ratio;
use crate::settings::user_config_toml_path;
use crate::settings::validate_repo_tool_lock;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::scope::Repository;
use kio_pipeline::budget::utc_month;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_search::cursor::CursorExcludedScope;
use kio_search::cursor::CursorToken;
use kio_search::cursor::ScopeCursor;
use kio_search::cursor::decode_cursor_token;
use kio_search::cursor::encode_cursor_token;
use kio_search::evidence::EvidencePointerIssueRequest;
use kio_search::evidence::evidence_pointer_to_uri;
use kio_search::evidence::issue_evidence_pointer;
use kio_search::object_uri::extract_related_images;
use kio_search::query::ChunkingConfigBinding;
use kio_search::query::QueryHashInput;
use kio_search::query::ScopeSelectionMode;
use kio_search::query::SearchMode;
use kio_search::query::query_hash;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Instant;
use unicode_normalization::UnicodeNormalization;

pub(crate) fn run_search(ctx: &Invocation<'_>, args: SearchArgs) -> Result<Value> {
    // R12-4: a FAILED search (cursor mismatch, all-scope-failed, …) returns before
    // `append_search_logs`, so it never wrote a per-search metrics line and dropped
    // out of the p50/p95/p99 latency population (docs/05:578). Emit that line here on
    // the error path (result_count 0 + error_code). The errors.jsonl line for a hard
    // Err is still written by main()'s Err arm.
    let started = Instant::now();
    let result = run_search_inner(ctx, args, started);
    if let Err(error) = &result {
        append_failed_search_metrics(started, error, ctx.control, ctx.locks);
    }
    result
}

fn run_search_inner(ctx: &Invocation<'_>, args: SearchArgs, started: Instant) -> Result<Value> {
    let adapter_settings = ctx.settings;

    let parsed = parsed_search(args)?;
    // PC10 (05 §1.3 L115): a query that tokenizes to zero tokens (empty, or
    // whitespace-only under PC9's Unicode-whitespace split) is a usage error
    // — rejected here, before any repo/registry/index access ("起動時に...
    // 拒否する (index/registry へのアクセス前)").
    if query_tokens(&parsed.query.nfc().collect::<String>()).is_empty() {
        return Err(KioError::invalid_usage(
            "search query has no indexable token",
        ));
    }
    let repo = ctx.open_search_repository()?;
    validate_repo_tool_lock(&repo)?;

    let multi_scope_settings = multi_scope::effective_settings(
        &repo.kio_dir().join("config.toml"),
        &user_config_toml_path(ctx.control),
    )?;

    // Page 1 enumerates scopes (registry-based, K3); later pages replay the frozen
    // scope set stored in the cursor (05 §1.8 — the cursor scope set is truth).
    // The scope set must be known before mode resolution (K4), because vector
    // availability depends on the actual per-scope embedding indexes (03 §7).
    // PC49/PC50: it must ALSO be known before `[search]` config resolution
    // (moved below this block, was above it) — whether folder config may
    // apply at all depends on whether this is a single-scope or multi-scope
    // search (05 §1.8 L384-387).
    // O1(b): cursors are HMAC-signed with a device-local key; decode verifies the
    // signature, so a forged / tampered token is rejected (KIO-E-SEARCH-CURSOR-001)
    // before its frozen scope set is ever trusted.
    let cursor_key = cursor_signing_key(ctx.control)?;
    let decoded_cursor = match &parsed.cursor {
        Some(token) => Some(decode_cursor_token(token, &cursor_key).map_err(search_to_kio)?),
        None => None,
    };
    let explicit_selector = parsed
        .time_selector_flags
        .is_explicit()
        .then(|| parsed.time_selector_flags.canonicalize())
        .transpose()?;
    let (time_selector, since_cutoff) = match &decoded_cursor {
        Some(cursor) => {
            let frozen = selector_from_cursor(&cursor.time_travel)?;
            validate_cursor_cutoff(&frozen, cursor.since_cutoff.as_deref())?;
            (
                reconcile_cursor_selector(explicit_selector.as_ref(), &frozen)?,
                cursor.since_cutoff.clone(),
            )
        }
        None => {
            let selector = explicit_selector.unwrap_or_default();
            let cutoff = since_cutoff_utc(&selector, &ctx.control.now_utc_seconds())?;
            (selector, cutoff)
        }
    };
    let (scope_mode, mut exec_scopes, mut cursor_excluded, authoritative_all_scope_registry) =
        match &decoded_cursor {
            Some(cursor) => {
                // R23-25 (05 §1.8 L425 / 06 §7 L361 "DUP → exit 3"): a live
                // registry scope_id duplicate discovered while re-resolving a
                // frozen cursor scope (`resolve_cursor_exec_scopes` ->
                // `resolve_scope_id_in_registry`) is retryable (dedupe, then
                // retry) -- the same search-domain classification §1.8's
                // homogeneous-reason promotion and the multi-scope aggregation
                // below give it. The SHARED `registry_duplicate_error`
                // constructor keeps its own `PermanentFailure` (4) default for
                // its other callers (open/view/restore's Evidence resolution,
                // the write-path `registry_duplicate_guard` preflight -- both
                // already exit 4 by design, per their own tests), so the
                // remapping happens only here, at this search-specific call
                // site, not in the constructor itself.
                let (exec, excluded) = match resolve_cursor_exec_scopes(ctx, cursor) {
                    Ok(pair) => pair,
                    Err(error) if error.error_code() == "KIO-E-REGISTRY-DUP-001" => {
                        return Err(KioError::new(
                            error.error_code().to_owned(),
                            error.message().to_owned(),
                            error.context().clone(),
                            ExitCode::PartialFailure,
                        ));
                    }
                    Err(error) => return Err(error),
                };
                // O1(a): a cursor must not bypass the caller's --scope/--descendants
                // restriction. Compute the scopes this invocation is allowed to reach
                // and intersect the cursor's frozen scope set with it; a cursor scope
                // outside the allowed set is excluded (scope_restriction_mismatch),
                // never searched. For a plain page-2 replay (no --scope) the allowed
                // set is every registered scope, so this is a no-op.
                let (_allowed_mode, allowed_targets, _) =
                    enumerate_scope_targets(ctx, &repo, &parsed)?;
                let allowed_ids: BTreeSet<String> = allowed_targets
                    .iter()
                    .map(|target| target.scope_id.clone())
                    .collect();
                if let Some(restricted) = exec
                    .iter()
                    .find(|exec| !allowed_ids.contains(&exec.target.scope_id))
                {
                    return Err(KioError::new(
                        "KIO-E-SEARCH-CURSOR-001",
                        "search cursor active scope is outside the requested scope restriction",
                        json!({
                            "reason": "active_scope_unavailable",
                            "scope_id": restricted.target.scope_id,
                        }),
                        ExitCode::InvalidUsage,
                    ));
                }
                (
                    scope_selection_from_cursor(cursor.scope_mode),
                    exec,
                    excluded,
                    false,
                )
            }
            None => {
                let (scope_mode, targets, authoritative_all_scope_registry) =
                    enumerate_scope_targets(ctx, &repo, &parsed)?;
                let exec = targets
                    .into_iter()
                    .map(|target| ExecScope {
                        target,
                        snapshot_commit: None,
                        max_rowid: None,
                        max_association_rowid: None,
                        chunking_config_hash: None,
                        index_generation: None,
                        from_cursor: false,
                    })
                    .collect::<Vec<_>>();
                // R23-27 (10 §3 L284-299): report the scope_id groups
                // `registry_all_targets` fail-closed out of the enumeration
                // above, so a live clone pair is surfaced (excluded_scopes,
                // reason=registry_duplicate, KIO-E-REGISTRY-DUP-001) instead of
                // silently vanishing from the response. `--scope`/`--descendants`
                // restrict enumeration by path, which this device-wide duplicate
                // listing does not filter by — attaching it there would report
                // an unrelated duplicate as if this search had tried to reach
                // it, so it is scoped to the unrestricted default enumeration
                // (registry open failure best-effort: `Ok(None)` is already
                // reflected in `enumerate_scope_targets`'s own P6 fallback
                // above, so this simply adds nothing in that case).
                let dup_excluded = if parsed.scope.is_none() && !parsed.descendants {
                    registry_duplicate_groups(ctx.control)?
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(scope_id, candidates)| {
                            json!({
                                "scope_id": scope_id,
                                "scope_path": Value::Null,
                                "reason": "registry_duplicate",
                                "candidates": candidates
                                    .iter()
                                    .map(|path| path.display().to_string())
                                    .collect::<Vec<_>>(),
                            })
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                (
                    scope_mode,
                    exec,
                    dup_excluded,
                    authoritative_all_scope_registry,
                )
            }
        };

    // M2: current management membership and Ignore authority decide the
    // searchable scope set before mode resolution or query embedding consent.
    // A denied scope must not influence the OR consent gate, even though a
    // later binding filter will also remove its individual paths.
    let mut current_policies = BTreeMap::<String, CurrentPolicyEvaluator>::new();
    let mut search_repositories = BTreeMap::<String, Repository>::new();
    let mut policy_denied_scope_ids = BTreeSet::<String>::new();
    // Retain reachable projections even when current policy denies a scope.
    // Only proven store-open failures permit removal during fresh enumeration.
    let mut retainable_all_scopes = exec_scopes
        .iter()
        .map(|exec| exec.target.scope_id.clone())
        .collect::<BTreeSet<_>>();
    for exec in &exec_scopes {
        let eligible_policy =
            current_search_policy(&exec.target, ctx.control).and_then(|(repo, policy)| {
                policy
                    .allows_scope()
                    .map(|allowed| (repo, policy, allowed))
                    .map_err(pipeline_to_kio)
            });
        let (repo, policy, allowed) = match eligible_policy {
            Ok(eligible) => eligible,
            Err(error) => {
                let Some(reason) = search_policy_exclusion_reason(&error) else {
                    return Err(error);
                };
                if exec.from_cursor {
                    return Err(policy_cursor_mismatch(&exec.target.scope_id, reason));
                }
                policy_denied_scope_ids.insert(exec.target.scope_id.clone());
                if reason == "unreachable" {
                    retainable_all_scopes.remove(&exec.target.scope_id);
                }
                let mut entry = json!({
                    "scope_id": exec.target.scope_id,
                    "scope_path": exec.target.kio_dir,
                    "reason": reason,
                    "error_code": error.error_code(),
                });
                if let Some(hint) = store_corruption_recovery_hint(reason) {
                    entry["recovery"] = json!(hint);
                }
                cursor_excluded.push(entry);
                continue;
            }
        };
        if !allowed {
            if exec.from_cursor {
                return Err(policy_cursor_mismatch(
                    &exec.target.scope_id,
                    "scope_denied",
                ));
            }
            policy_denied_scope_ids.insert(exec.target.scope_id.clone());
            cursor_excluded.push(json!({
                "scope_id": exec.target.scope_id,
                "scope_path": exec.target.kio_dir,
                "reason": "policy_denied",
            }));
            continue;
        }
        search_repositories.insert(exec.target.scope_id.clone(), repo);
        current_policies.insert(exec.target.scope_id.clone(), policy);
    }
    exec_scopes.retain(|exec| !policy_denied_scope_ids.contains(&exec.target.scope_id));

    // Scope eligibility errors precede adapter availability and budget access.
    // In particular, a duplicate-only registry must not become a misleading
    // "embedding endpoint unavailable" failure for an explicit vector query.
    if exec_scopes.is_empty() {
        if !cursor_excluded.is_empty()
            && cursor_excluded.iter().all(|entry| {
                entry.get("reason").and_then(Value::as_str) == Some("registry_duplicate")
            })
        {
            return Err(KioError::new(
                "KIO-E-REGISTRY-DUP-001",
                "every searched scope's scope_id is a live registry duplicate; dedupe before retrying",
                json!({ "excluded_scopes": cursor_excluded }),
                ExitCode::PartialFailure,
            ));
        }
        return Err(scope_all_failed_error(
            if cursor_excluded.is_empty() {
                "no indexed scopes are registered for search; run `kio index` in a scope first"
            } else {
                "no eligible scope remains for this search; inspect the excluded scopes"
            },
            cursor_excluded,
        ));
    }

    // PC49/PC50 (05 §1.8 L384-387): folder config.toml applies only for a
    // single, non-`--descendants` `--scope <path>` — every other scope_mode
    // (All / Descendants) uses the user (device) layer exclusively, even when
    // the CWD itself happens to be one of the searched scopes and carries its
    // own folder override.
    let single_scope_kio_dir = (scope_mode == ScopeSelectionMode::Scope)
        .then(|| exec_scopes.first().map(|exec| exec.target.kio_dir.clone()))
        .flatten();
    // R11-7: apply the `[search]` config (config.schema.json §search). `default_mode`
    // seeds the requested mode ONLY when no CLI `--text`/`--vector`/`--hybrid` was
    // given (the flag always wins); `fail_behavior` governs what auto/--hybrid does
    // when no vector backend is available. Both were schema-valid + documented but
    // entirely unwired before (the [search] version of the R10-2 config drift).
    let (config_default_mode, config_fail_behavior) =
        effective_search_config(single_scope_kio_dir.as_deref(), ctx.control)?;
    // R12-1: effective `[search.rrf]` / `[search.diversify]` (05 §1.3/§1.4). These
    // were documented + schema-valid but hardcoded at every call site (RRF fuse,
    // diversify, query_hash) — the tuning keys were dead. Read them once and thread
    // them through so config actually changes ranking/dedup AND invalidates a stale
    // cursor via query_hash.
    let (rrf_config, diversify_request) =
        effective_search_tuning(single_scope_kio_dir.as_deref(), ctx.control)?;
    let requested_mode = if parsed.explicit_mode {
        parsed.requested_mode
    } else {
        config_default_mode.unwrap_or(parsed.requested_mode)
    };
    let fail_behavior = config_fail_behavior.unwrap_or(SearchFailBehavior::Fallback);

    // Direct search opens the device replica once and keeps that connection for
    // both scope preparation and candidate selection.  In particular, do not
    // probe every `.kio/index/sqlite.db` merely to learn vector profiles or a
    // current snapshot: those facts are committed with the replica corpus.
    let mut replica = if exec_scopes.is_empty() {
        None
    } else {
        Some(
            kio_index::aggregator::Aggregator::open_with_control(&aggregator_path(ctx.control), ctx.control).map_err(|error| {
                KioError::new(
                    "KIO-E-AGGREGATOR-001",
                    "the device search replica could not be opened; run `kio repair replica` (or `kio repair all`) to recreate it, then retry",
                    json!({ "reason": error.to_string() }),
                    ExitCode::PartialFailure,
                )
            })?,
        )
    };
    let replica_headers = if let Some(replica) = replica.as_ref() {
        exec_scopes
            .iter()
            .map(|exec| {
                replica
                    .scope_header(&exec.target.scope_id)
                    .map(|header| (exec.target.scope_id.clone(), header))
                    .map_err(|error| {
                        KioError::new(
                            "KIO-E-AGGREGATOR-001",
                            "the device search replica could not read a scope header; retry after indexing",
                            json!({
                                "scope_id": exec.target.scope_id,
                                "reason": error.to_string(),
                            }),
                            ExitCode::PartialFailure,
                        )
                    })
            })
            .collect::<Result<BTreeMap<_, _>>>()?
    } else {
        BTreeMap::new()
    };
    let replica_embedding_states = exec_scopes
        .iter()
        .map(|exec| {
            replica_embedding_state(
                adapter_settings,
                replica_headers
                    .get(&exec.target.scope_id)
                    .and_then(Option::as_ref),
                ctx.control,
            )
        })
        .collect::<Vec<_>>();

    // Mode resolution (05 §1.1). O2: the query embedding is SENT to the online
    // embedding endpoint, so it must not be computed until the resolved mode
    // actually uses vectors AND the scope's embedding opt-in (07 §3) is granted.
    // Judge vector availability from cheap predicates (endpoint + opt-in + query
    // length + per-scope compat) — never by eagerly calling the adapter.
    // QA21/23: the AND-gate's profile match needs the adapter's CURRENT
    // `tool_profile_hash` alongside its `tool_id`.
    let adapter_identity = active_embedding_adapter_identity(adapter_settings, ctx.control)?;
    let adapter_id = adapter_identity.as_ref().map(|(id, _)| id.clone());
    // An embedding adapter can be an authenticated local HTTPS peer. Only the
    // deterministic evaluator is in-process; every other execution needs an
    // exact device+scope grant.
    let active_embedding = embedding_execution(adapter_settings, ctx.control);
    let embedding_is_deterministic = active_embedding.is_some_and(embedding_is_deterministic);
    if parsed.online && (active_embedding.is_none() || embedding_is_deterministic) {
        return Err(KioError::invalid_usage(
            "search --online requires an active network embedding adapter",
        ));
    }
    let embedding_opt_in = if embedding_is_deterministic {
        true
    } else {
        match &adapter_identity {
            Some((id, tool_profile_hash)) => embedding_opt_in_for_scopes(
                adapter_settings,
                &exec_scopes,
                id,
                tool_profile_hash,
                parsed.offline,
                ctx.control,
            )?,
            None => false,
        }
    };
    let query_embeddable = parsed.query.chars().count() >= 2;
    let vector_precheck = resolve_vector_availability(
        requested_mode,
        &replica_embedding_states,
        // `--offline` forbids every adapter transport, including authenticated
        // local peers, but never the in-process deterministic evaluator.
        parsed.offline && !embedding_is_deterministic,
        active_embedding.is_some(),
        embedding_opt_in,
        query_embeddable,
    );
    let precheck_mode = resolve_search_mode(requested_mode, &vector_precheck, fail_behavior)?;
    // The deterministic evaluator has no billable ledger operation. Do not
    // initialize or inspect the device ledger for that in-process path; budget
    // status is zero there. HTTP sends retain the stable snapshot used by both
    // admission and output.
    let ledger_path = ledger_db_path(ctx.control);
    let (budget_month, budget_ledger) = if embedding_is_deterministic {
        (utc_month(&ctx.control.now_utc_seconds()), None)
    } else {
        capture_search_budget_observation(&ledger_path, ctx.control)?
    };

    if parsed.online {
        // `--online` selects the remote execution path; it neither manufactures
        // a grant nor permits the normal auto/hybrid text fallback when the
        // remote adapter, index, or approval cannot support this request.
        resolve_search_mode(
            SearchMode::Vector,
            &vector_precheck,
            SearchFailBehavior::Error,
        )?;
    }

    // Only now, and only when the pre-resolved mode uses vectors, compute (send)
    // the query embedding. In --text this branch is never taken, so the query is
    // never sent.
    let uses_vectors = matches!(
        precheck_mode.resolved,
        SearchMode::Hybrid | SearchMode::Vector
    );
    // F2: the FTS index projection is NFC-normalized (kio-index `index_chunk`), so
    // the FTS query and the embedded query must use the same normal form or NFD
    // content is a silent false negative (and vice versa). `query_hash` already
    // normalizes internally, so the cursor hash is unaffected; only the MATCH /
    // vector inputs need this. Display fields keep the caller's original `query`.
    let query_nfc = parsed.query.nfc().collect::<String>();
    // 04 §5.4 §H (CL48-55): only a FRESH page 1 (no cursor) writes the device
    // query-embedding row. R23-01 (05 §1.5 L234): a cursor replay (page 2+)
    // no longer computes a query embedding at all — see the `else` arm below.
    let mut vector_unavailable_reason = "query_embedding_unavailable";
    let mut final_consent_unauthorized = false;
    let query_embedding = if uses_vectors {
        if decoded_cursor.is_none() {
            // Consent was computed from policy-filtered scopes above. Recheck
            // that same live authority immediately before the embedding helper
            // can claim budget or send the query body.
            match compute_query_embedding_page1(
                ctx,
                &query_nfc,
                &exec_scopes,
                &search_repositories,
                adapter_id.as_deref().unwrap_or_default(),
                parsed.offline,
                &current_policies,
            )? {
                Some(QueryEmbeddingOutcome::Vector(vector)) => Some(vector),
                // CL54: a live in-flight claim from another process already
                // holds this exact query — text fallback, distinct reason from
                // a plain adapter failure.
                Some(QueryEmbeddingOutcome::InFlight) => {
                    vector_unavailable_reason = "embedding_in_flight";
                    None
                }
                // PC7: the adapter's response failed its 07 §5.3 acceptance
                // check — a distinct technical failure from a generic
                // "unavailable" (both are fail_behavior-governed, PC2).
                Some(QueryEmbeddingOutcome::ContractViolation) => {
                    vector_unavailable_reason = "embedding_contract_violation";
                    None
                }
                // PC6: the final pre-attempt re-read found the send no longer
                // authorized (a revoke completed between the precheck above
                // and the re-read) — this is the same user-intent category as
                // the precheck's own Unauthorized, not a technical failure.
                Some(QueryEmbeddingOutcome::NotAuthorized) => {
                    final_consent_unauthorized = true;
                    None
                }
                None => None,
            }
        } else {
            // R23-01 (05 §1.5 L234 "vector / hybrid の replay は page 1 の
            // query vector を再利用する — query の再 embedding は行わない"):
            // reuse page 1's vector via its device-local cache, verified by
            // the cursor's own signed `query_vector_digest`. This branch is
            // reachable only when `decoded_cursor.is_some()` (the
            // `if decoded_cursor.is_none()` arm above took the page-1 path
            // otherwise). No code path from here can reach
            // `run_embedding_adapter`/`compute_query_embedding` — replay
            // NEVER re-sends the query, regardless of provider
            // non-determinism or cache staleness (both fail closed to
            // `KIO-E-SEARCH-CURSOR-001`, never a silent resend).
            let digest = decoded_cursor
                .as_ref()
                .and_then(|cursor| cursor.query_vector_digest.as_deref())
                .ok_or_else(cursor_query_vector_error)?;
            Some(replay_query_vector(digest, ctx.control)?)
        }
    } else {
        None
    };
    // A live adapter failure (auth/rate) after the send still degrades vector→text
    // so `--vector` errors and auto/hybrid falls back, exactly as before O2.
    let vector = if uses_vectors && query_embedding.is_none() {
        if final_consent_unauthorized {
            VectorAvailability::Unauthorized
        } else {
            VectorAvailability::Unavailable {
                reason: vector_unavailable_reason,
            }
        }
    } else {
        vector_precheck
    };
    if parsed.online {
        resolve_search_mode(SearchMode::Vector, &vector, SearchFailBehavior::Error)?;
    }
    let mode = resolve_search_mode(requested_mode, &vector, fail_behavior)?;
    // PC24 (05 §1.5/§1.8): the page-1 query vector's digest — present only
    // when a vector was actually obtained (vector|hybrid), matching
    // `query_embedding`'s own condition exactly. `None` in text mode omits
    // the cursor/query_hash field entirely (PC27).
    let query_vector_digest = query_embedding.as_deref().map(query_vector_digest_hex);
    // R23-01: seed the replay cache on a FRESH page 1 that actually obtained
    // a vector (a replay's own vector is already cached from ITS page 1 —
    // re-writing the identical bytes under the identical digest-derived path
    // is harmless but pointless). Best-effort (see `persist_query_vector_cache`).
    if decoded_cursor.is_none()
        && let (Some(vector), Some(digest)) = (&query_embedding, &query_vector_digest)
    {
        persist_query_vector_cache(digest, vector, ctx.control);
    }

    // N8: the short-query short-circuit is NOT taken here — it must run after scope
    // resolution, the all-failed check, and index_status aggregation below, or a
    // 1-char query would mask a scope failure and pin index_status to a fixed 1.0.

    // A replay can validate its hash entirely from the signed active scopes and
    // their frozen config mappings. Page 1 defers hash construction until after
    // per-scope execution, so an unreadable sibling remains a normal partial
    // exclusion instead of aborting before the isolation boundary.
    let mut qhash = String::new();
    if let Some(cursor) = &decoded_cursor {
        let mut scope_ids = cursor
            .scopes
            .iter()
            .map(|scope| scope.scope_id.clone())
            .collect::<Vec<_>>();
        scope_ids.sort();
        let mut chunking_configs = cursor
            .scopes
            .iter()
            .map(|scope| ChunkingConfigBinding {
                scope_id: scope.scope_id.clone(),
                chunking_config_hash: scope.chunking_config_hash.clone(),
            })
            .collect::<Vec<_>>();
        chunking_configs.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
        qhash = query_hash(&QueryHashInput {
            query: parsed.query.clone(),
            mode: mode.resolved,
            scope_mode,
            scopes: scope_ids,
            diversify: diversify_request.clone(),
            rrf_k: rrf_config.k,
            rrf_candidate_depth: rrf_config.candidate_depth,
            rrf_w_text: rrf_config.w_text,
            rrf_w_vector: rrf_config.w_vector,
            chunking_configs,
            time_travel: selector_for_search(&time_selector),
            // PC24: replay recomputes using the token's OWN recorded digest —
            // this hash check verifies the rest of the preimage against the
            // signed cursor's self-reported value, not a freshly-derived one.
            query_vector_digest: cursor.query_vector_digest.clone(),
        })
        .map_err(search_to_kio)?;
        if cursor.query_hash != qhash {
            return Err(KioError::new(
                "KIO-E-SEARCH-CURSOR-001",
                "search cursor query_hash mismatch",
                json!({ "expected": qhash, "actual": cursor.query_hash }),
                ExitCode::InvalidUsage,
            ));
        }
    }

    // Resolve and validate each scope before querying the replica. This is
    // deliberately NOT a per-scope candidate search: the single device
    // replica below is the only candidate source.
    let query_plan = build_query_plan(&query_nfc);
    let mut searched = Vec::<SearchedScopeInfo>::new();
    // A fresh all-scopes query may reconcile registry departures, but a
    // request-local readiness/mode exclusion must not make a still-reachable
    // scope disappear from the device replica.  Start with the complete
    // enumerated registry set and remove only scopes whose store cannot be
    // opened at all below; no source search index is consulted for this.
    // Scopes the cursor froze but the registry can no longer resolve are already
    // excluded (reason "unreachable") before per-scope execution starts.
    let mut excluded = cursor_excluded;
    // Candidate-scope barriers are opened only after the replica has selected
    // rows; unselected scopes do not participate in the purge read barrier.
    let mut late_barriers = Vec::<(usize, usize, ReadBarrierCheckpoint)>::new();
    let scope_delay_control = ctx.scope_delay_test_control();

    let replica_selector = aggregator_selector(&time_selector);
    let scope_executions =
        multi_scope::run_ordered(exec_scopes.len(), multi_scope_settings, |idx, deadline| {
            prepare_scope_from_replica_header(
                &exec_scopes[idx],
                replica_headers
                    .get(&exec_scopes[idx].target.scope_id)
                    .and_then(Option::as_ref),
                &time_selector,
                since_cutoff.as_deref(),
                deadline,
                &scope_delay_control,
                ctx.control,
            )
        });
    for (idx, execution) in scope_executions.into_iter().enumerate() {
        let exec = &exec_scopes[idx];
        let result = match execution {
            multi_scope::ScopeExecution::Completed(result) => result,
            multi_scope::ScopeExecution::TimedOut => {
                Err(ScopeSearchError::Excluded("timeout".to_owned()))
            }
        }
        .and_then(|resolution| {
            let replica = replica
                .as_ref()
                .expect("a nonempty direct search opens its replica once");
            let marker = replica
                .projection_marker(
                    &exec.target.scope_id,
                    replica_selector,
                    &resolution.snapshot_commit,
                )
                .map_err(|error| {
                    ScopeSearchError::Fatal(KioError::new(
                        "KIO-E-AGGREGATOR-001",
                        "the device search replica could not read a scope projection; retry after indexing",
                        json!({
                            "scope_id": exec.target.scope_id,
                            "reason": error.to_string(),
                        }),
                        ExitCode::PartialFailure,
                    ))
                })?
                .ok_or_else(|| ScopeSearchError::Excluded(INDEX_REBUILDING_REASON.to_owned()))?;
            if mode.resolved == SearchMode::Vector {
                match replica_embedding_state(adapter_settings, Some(&resolution.header), ctx.control) {
                    ScopeEmbedState::Compatible => {}
                    ScopeEmbedState::Incompatible => {
                        return Err(ScopeSearchError::Excluded(
                            VEC_PROFILE_INCOMPATIBLE_REASON.to_owned(),
                        ));
                    }
                    ScopeEmbedState::Absent => {
                        return Err(ScopeSearchError::Excluded(
                            VEC_PROFILE_ABSENT_REASON.to_owned(),
                        ));
                    }
                }
            }
            let chunking_config_hash = marker
                .chunking_config_hash
                .clone()
                .or_else(|| resolution.header.current_chunking_config_hash.clone())
                .ok_or_else(|| ScopeSearchError::Excluded(INDEX_REBUILDING_REASON.to_owned()))?;
            if exec
                .chunking_config_hash
                .as_deref()
                .is_some_and(|frozen| frozen != chunking_config_hash)
            {
                return Err(ScopeSearchError::Fatal(KioError::new(
                    "KIO-E-SEARCH-CURSOR-001",
                    "search cursor chunking config changed",
                    json!({ "scope_id": exec.target.scope_id }),
                    ExitCode::InvalidUsage,
                )));
            }
            Ok(ScopeOutcome {
                snapshot_commit: resolution.snapshot_commit,
                max_rowid: exec.max_rowid.unwrap_or(resolution.header.max_rowid),
                max_association_rowid: exec
                    .max_association_rowid
                    .unwrap_or(resolution.header.max_association_rowid),
                chunking_config_hash,
                index_generation: resolution.header.index_generation,
                shallow_skipped: resolution.shallow_skipped,
                runtime_binding_filter: resolution.runtime_binding_filter,
                journal_active_at_prepare: resolution.journal_active_at_prepare,
            })
        });
        match result {
            Ok(outcome) => {
                let current_policy = current_policies
                    .get(&exec.target.scope_id)
                    .expect("policy-filtered execution scope has a current policy")
                    .clone();
                if let Some(cursor) = &decoded_cursor
                    && let Some(frozen) = cursor
                        .scopes
                        .iter()
                        .find(|scope| scope.scope_id == exec.target.scope_id)
                        .map(|scope| &scope.policy_digest)
                    && frozen != current_policy.digest()
                {
                    return Err(policy_cursor_mismatch(
                        &exec.target.scope_id,
                        "policy_digest_mismatch",
                    ));
                }
                searched.push(SearchedScopeInfo {
                    scope_id: exec.target.scope_id.clone(),
                    // R23-20 (03 §4 L296): canonical `.kio` path, matching
                    // `searched_scopes[].scope_path`'s documented example
                    // value (05 §1.7/§1.8, "/Users/foo/Research/.kio").
                    scope_path: exec.target.kio_dir.clone(),
                    repo_root: exec.target.repo_root.clone(),
                    snapshot_at: outcome.snapshot_commit.clone(),
                    max_rowid: outcome.max_rowid,
                    max_association_rowid: outcome.max_association_rowid,
                    chunking_config_hash: outcome.chunking_config_hash.clone(),
                    index_generation: outcome.index_generation.clone(),
                    journal_active_at_prepare: outcome.journal_active_at_prepare,
                    shallow_skipped: outcome.shallow_skipped,
                    runtime_binding_filter: outcome.runtime_binding_filter,
                });
            }
            Err(ScopeSearchError::Shallow) => {
                // A selected `--at` target, history start, or cursor snapshot
                // is never partially served from a durable projection: its
                // whole tree is required by 05 §2.2.
                return Err(KioError::new(
                    "KIO-E-COMMIT-SHALLOW-001",
                    "selected search snapshot is shallow (tree discarded); re-run with an available snapshot",
                    json!({ "scope_id": exec.target.scope_id }),
                    ExitCode::Failure,
                ));
            }
            Err(ScopeSearchError::Excluded(reason)) => {
                // A reachable scope can be transiently excluded for its replica
                // status, embedding profile, timeout, or a selector condition.
                // Preserve its projection for the next eligible request.  An
                // actual store-open failure proves the registered scope has
                // departed, so fresh all-scopes reconciliation may evict it.
                if reason == "unreachable" {
                    retainable_all_scopes.remove(&exec.target.scope_id);
                }
                if exec.from_cursor {
                    return Err(KioError::new(
                        "KIO-E-SEARCH-CURSOR-001",
                        "search cursor active scope is no longer available; re-run without a cursor",
                        json!({
                            "reason": "active_scope_unavailable",
                            "cause": reason,
                            "scope_id": exec.target.scope_id,
                        }),
                        ExitCode::InvalidUsage,
                    ));
                }
                // R18-4: attach the store-corruption recovery hint to THIS entry (not
                // only to the all-scopes-failed aggregate below), so a PARTIAL exclusion
                // (some scopes healthy → `searched` non-empty → the aggregate block is
                // skipped) is still agent-detectable.
                let mut entry = json!({
                    "scope_id": exec.target.scope_id,
                    // R23-20 (03 §4 L296): canonical `.kio` path, matching
                    // `excluded_scopes[].scope_path`'s documented example
                    // value (05 §1.8, "/Volumes/ext/Research/.kio").
                    "scope_path": exec.target.kio_dir,
                    "reason": reason,
                });
                if let Some(hint) = store_corruption_recovery_hint(&reason) {
                    entry["recovery"] = json!(hint);
                }
                excluded.push(entry);
            }
            Err(ScopeSearchError::Fatal(error)) => {
                if exec.from_cursor {
                    return Err(error);
                }
                // R16-2: a store-corruption Fatal from ONE scope must not discard the
                // healthy scopes' already-collected results — that violated the 05 §1.8
                // per-scope isolation contract that `index_corrupt` / `index_missing` /
                // vector-capacity already honor. Downgrade the store-corruption class to
                // an `Excluded("store_corrupt")` so healthy scopes still return and the
                // "every scope failed → SCOPE-ALL-FAILED exit 4" aggregation still fires
                // only when EVERY scope failed. This is deliberately NOT a blanket
                // Fatal→Excluded generalization: a genuine programming error / unexpected
                // Fatal must stay fail-fast, so only the explicit store class is caught.
                if is_store_corrupt_class(&error) {
                    // R18-4: same per-entry recovery hint as the Excluded arm above.
                    let mut entry = json!({
                        "scope_id": exec.target.scope_id,
                        // R23-20 (03 §4 L296): canonical `.kio` path.
                        "scope_path": exec.target.kio_dir,
                        "reason": "store_corrupt",
                    });
                    if let Some(hint) = store_corruption_recovery_hint("store_corrupt") {
                        entry["recovery"] = json!(hint);
                    }
                    excluded.push(entry);
                } else {
                    return Err(error);
                }
            }
        }
    }

    // Image authority requires a strict immutable-history walk. Calculate it
    // only after the regular per-scope preflight has admitted a scope: a
    // corrupt sibling must be recorded as an exclusion instead of aborting
    // healthy scopes before the replica can return their text results. This
    // runs before the all-failed aggregation and fresh cursor hash so their
    // scope sets include the same exclusions.
    let mut image_authorities = BTreeMap::new();
    let mut image_authority_exclusions = BTreeSet::new();
    for scope in &searched {
        let repo = search_repositories
            .get(&scope.scope_id)
            .expect("eligible search scope retains its repository");
        let policy = current_policies
            .get(&scope.scope_id)
            .expect("eligible search scope retains its current policy");
        match current_search_image_authority(repo, policy) {
            Ok(authority) => {
                image_authorities.insert(scope.scope_id.clone(), authority);
            }
            Err(error) if is_store_corrupt_class(&error) => {
                if decoded_cursor.is_some() {
                    return Err(policy_cursor_mismatch(&scope.scope_id, "store_corrupt"));
                }
                let mut entry = json!({
                    "scope_id": scope.scope_id.clone(),
                    "scope_path": scope.scope_path.clone(),
                    "reason": "store_corrupt",
                });
                if let Some(hint) = store_corruption_recovery_hint("store_corrupt") {
                    entry["recovery"] = json!(hint);
                }
                excluded.push(entry);
                image_authority_exclusions.insert(scope.scope_id.clone());
            }
            Err(error) => return Err(error),
        }
    }
    searched.retain(|scope| !image_authority_exclusions.contains(&scope.scope_id));

    if searched.is_empty() {
        // PC52/PC55(c) (05 §1.8 L390-391 / §R note-4 ruling: INCOMPAT →
        // journal → DUP → REBUILDING): every enumerated scope was
        // excluded by explicit `--vector`'s own per-scope profile-compat gate
        // (replica-header preparation's `VEC_PROFILE_INCOMPATIBLE_REASON` /
        // `VEC_PROFILE_ABSENT_REASON`, checked only when `resolved_mode ==
        // Vector`). Checked second, right after STORE-VERSION and ahead of
        // journal/DUP/REBUILDING — same exit-8 family as STORE-VERSION
        // (`KIO-E-SEARCH-VEC-INCOMPAT-001` mirrors PC30's already-confirmed
        // single-scope `--vector` + incompatible hard error, generalized here
        // to "every scope", not just one).
        let all_vec_incompatible = !excluded.is_empty()
            && excluded.iter().all(|entry| {
                matches!(
                    entry.get("reason").and_then(Value::as_str),
                    Some(VEC_PROFILE_INCOMPATIBLE_REASON) | Some(VEC_PROFILE_ABSENT_REASON)
                )
            });
        if all_vec_incompatible {
            return Err(KioError::new(
                "KIO-E-SEARCH-VEC-INCOMPAT-001",
                "every searched scope's embedding profile is incompatible with (or absent from) \
                 the adopted embedding profile; vector search cannot run",
                json!({ "excluded_scopes": excluded }),
                ExitCode::IncompatibleProfile,
            ));
        }
        // §I (LC52-56): every enumerated scope hit an active purge journal, or
        // its journal/purge-epoch/lifecycle-epoch triple changed between its
        // checkpoint 1 and checkpoint 2. Surface the command-level
        // KIO-E-PURGE-JOURNAL-ACTIVE-001 (retryable exit 3) instead of a false
        // permanent all-failed — mirrors `all_rebuilding` below, and is
        // checked first per 10-operations.md §3's priority order ((1) purge
        // journal/epoch precedes (3) index availability).
        let all_purge_journal_active = !excluded.is_empty()
            && excluded.iter().all(|entry| {
                entry.get("reason").and_then(Value::as_str) == Some(PURGE_JOURNAL_ACTIVE_REASON)
            });
        if all_purge_journal_active {
            return Err(KioError::new(
                "KIO-E-PURGE-JOURNAL-ACTIVE-001",
                "an incomplete purge transaction is active, or completed while this read was in \
                 flight, in every searched scope; retry",
                json!({ "excluded_scopes": excluded }),
                ExitCode::PartialFailure,
            ));
        }
        // R23-27 (10 §3 L284-299 priority (2) / 06 §7 L361 "DUP → exit 3"):
        // every enumerated scope was excluded because its scope_id is a live
        // registry duplicate (`registry_all_targets`'s own dedup below, or a
        // cursor-replay scope that became ambiguous — R23-25 above). Checked
        // ahead of (3) index availability, matching 10 §3's exclusion
        // priority (journal → DUP → REBUILDING). The command-wide exact-format
        // gate has already stopped any version mismatch before this aggregate.
        let all_registry_duplicate = !excluded.is_empty()
            && excluded.iter().all(|entry| {
                entry.get("reason").and_then(Value::as_str) == Some("registry_duplicate")
            });
        if all_registry_duplicate {
            return Err(KioError::new(
                "KIO-E-REGISTRY-DUP-001",
                "every searched scope's scope_id is a live registry duplicate; dedupe before \
                 retrying",
                json!({ "excluded_scopes": excluded }),
                ExitCode::PartialFailure,
            ));
        }
        // P10: every enumerated scope is mid-reindex (HEAD advanced, rebuilt sqlite
        // not yet swapped in), OR its sqlite.db is missing/unusable (R23-14(b), 06
        // §7 L345-346: "sqlite.db 不在・利用不能 ... 全経路(verify/open/view/restore/
        // search) ... KIO-E-INDEX-REBUILDING-001・exit 3" — both are "the index
        // needs (re)building, retry" and share ONE promotion. A missing/corrupt
        // sqlite.db used to earn its own permanent-looking
        // KIO-E-SEARCH-VEC-UNAVAIL-001/exit 1 below (removed — text and vector
        // both live in that one file, so losing it is never actually a
        // "vector-only" unavailability, and it is exactly as recoverable as a
        // mid-rebuild window). This is transient — the complete result set
        // returns on retry once the atomic rename lands, or once `kio repair all`
        // repairs the source and replica (or `kio repair replica` repairs only
        // the device replica) — so surface the honest KIO-E-INDEX-REBUILDING-001
        // (docs/05:564) with the retryable exit 3 (05 §6, as
        // KIO-E-STORE-LOCKED-001 does), never a false permanent all-failed or a
        // silent empty page.
        let all_rebuilding = !excluded.is_empty()
            && excluded.iter().all(|entry| {
                matches!(
                    entry.get("reason").and_then(Value::as_str),
                    Some(INDEX_REBUILDING_REASON) | Some("index_missing") | Some("index_corrupt")
                )
            });
        if all_rebuilding {
            return Err(KioError::new(
                "KIO-E-INDEX-REBUILDING-001",
                "the searched scopes' projections in the device search replica are missing, \
                 unusable, or being rebuilt; retry, or run `kio repair replica` (or `kio repair all`) if the problem \
                 persists",
                json!({
                    "excluded_scopes": excluded,
                    "recovery": aggregate_store_recovery_hints(&excluded),
                }),
                ExitCode::PartialFailure,
            ));
        }
        // R23-15 (06 §8 L403 "単独操作 exit 4" / 05 §1.6 L307-310): every
        // enumerated scope hit the bounded history-walk aggregate cap
        // (`--all-history`/`--since`, the history planner's non-cursor
        // `Excluded("history_limit_exceeded")` arm) — promote to the
        // canonical KIO-E-COMMIT-HISTORY-LIMIT-001 at its own single-
        // operation exit (4, PermanentFailure) instead of losing the code
        // identity to the generic SCOPE-ALL-FAILED-001 the final PC57
        // fallback below would otherwise assign it (same exit either way,
        // since this reason is not retryable — re-running the identical
        // query cannot shrink the walk — but the CODE must survive).
        let all_history_limit_exceeded = !excluded.is_empty()
            && excluded.iter().all(|entry| {
                entry.get("reason").and_then(Value::as_str) == Some("history_limit_exceeded")
            });
        if all_history_limit_exceeded {
            return Err(KioError::new(
                "KIO-E-COMMIT-HISTORY-LIMIT-001",
                "the bounded history walk aggregate cap was exceeded in every searched scope",
                json!({ "excluded_scopes": excluded }),
                ExitCode::PermanentFailure,
            ));
        }
        // R17-4: when every scope was excluded for a store-corruption class (R16-2's
        // `store_corrupt` — a tampered/corrupt commit-or-tree object — or R16-3's
        // `snapshot_shallow` — a discarded HEAD tree with no cached rows), the generic
        // SCOPE-ALL-FAILED (exit 4, no guidance) left the operator/agent with no
        // recovery path (the R16-2/R16-3 exclusion reasons were never taught to this
        // aggregation, unlike `index_missing`/`index_corrupt` above). Attach
        // class-specific recovery guidance to the SCOPE-ALL-FAILED response (message +
        // `context.recovery`); the error CODE stays the docs-registered
        // SCOPE-ALL-FAILED-001 (the code registry in docs/06 §8 / docs/10 §7.5 is
        // normative and docs are frozen — no new code). Deliberately NOT unified with
        // `index_missing`'s retryable device-repair guidance: a shallow snapshot
        // still needs object restore/re-index, while `repair all` can attempt the
        // verifier's bounded working-tree recovery for store_corrupt. Exit stays 4
        // when that source state cannot be read (manual intervention required); the
        // `recovery` context is what makes the path agent-detectable.
        let store_corruption = !excluded.is_empty()
            && excluded.iter().all(|entry| {
                matches!(
                    entry.get("reason").and_then(Value::as_str),
                    Some("store_corrupt") | Some("snapshot_shallow")
                )
            });
        if store_corruption {
            let reason_is = |want: &str| {
                excluded
                    .iter()
                    .any(|entry| entry.get("reason").and_then(Value::as_str) == Some(want))
            };
            // R18-4: the same hints the per-entry attachment uses, aggregated here for
            // the all-scopes-failed response (identical strings via the shared helper).
            let mut recovery = Vec::<&str>::new();
            if reason_is("store_corrupt") {
                recovery.push(store_corruption_recovery_hint("store_corrupt").unwrap());
            }
            if reason_is("snapshot_shallow") {
                recovery.push(store_corruption_recovery_hint("snapshot_shallow").unwrap());
            }
            return Err(KioError::new(
                "KIO-E-SEARCH-SCOPE-ALL-FAILED-001",
                format!(
                    "every searched scope's store is corrupt or shallow, so search cannot \
                     run — {}",
                    recovery.join("; ")
                ),
                json!({ "excluded_scopes": excluded, "recovery": recovery }),
                ExitCode::PermanentFailure,
            ));
        }
        // R20-8: a HETEROGENEOUS all-failed mix (e.g. one index_corrupt + one store_corrupt)
        // matches neither homogeneous aggregation above, yet every scope has a known
        // recovery. R18-4 left it falling through to the bare "all searched scopes failed"
        // with no guidance. Attach aggregate recovery (message + context.recovery); the code
        // stays SCOPE-ALL-FAILED-001 (docs frozen — no new code) and exit 4 (a store_corrupt
        // member needs manual object restore, so not the exit-1 VEC-UNAVAIL rebuild path).
        let all_recoverable = !excluded.is_empty()
            && excluded.iter().all(|e| {
                matches!(
                    e.get("reason").and_then(Value::as_str),
                    Some(reason) if store_corruption_recovery_hint(reason).is_some()
                )
            })
            // R23-14(b): `store_corruption_recovery_hint` also answers for
            // `index_missing`/`index_corrupt`, which (unlike `store_corrupt`/
            // `snapshot_shallow`) are now retryable (folded into
            // `all_rebuilding` above). A HETEROGENEOUS mix of one of those
            // with a genuinely permanent reason must not be swept into this
            // block's flat exit 4 — defer to the PC57 split below, which
            // promotes such a mix to the retryable exit 3 per 05 §1.8's
            // "retryable 理由を含めば exit 3" rule.
            && !excluded.iter().any(|e| {
                e.get("reason")
                    .and_then(Value::as_str)
                    .is_some_and(is_retryable_scope_reason)
            });
        if all_recoverable {
            let recovery = aggregate_store_recovery_hints(&excluded);
            return Err(KioError::new(
                "KIO-E-SEARCH-SCOPE-ALL-FAILED-001",
                format!(
                    "every searched scope failed with a recoverable store/index error, so \
                     search cannot run — {}",
                    recovery.join("; ")
                ),
                json!({ "excluded_scopes": excluded, "recovery": recovery }),
                ExitCode::PermanentFailure,
            ));
        }
        // PC57 (05 §1.8 L392 / 06 §7 L362-363): a mixed-reason all-scopes
        // failure (none of the homogeneous promotions above matched) splits
        // by retryability instead of always landing on the generic exit 4 —
        // one retryable reason anywhere in the set is enough to promise a
        // retry MIGHT make progress (exit 3); an all-permanent mix cannot
        // (exit 4).
        let any_retryable = excluded.iter().any(|entry| {
            entry
                .get("reason")
                .and_then(Value::as_str)
                .is_some_and(is_retryable_scope_reason)
        });
        let exit = if any_retryable {
            ExitCode::PartialFailure
        } else {
            ExitCode::PermanentFailure
        };
        return Err(KioError::new(
            "KIO-E-SEARCH-SCOPE-ALL-FAILED-001",
            "all searched scopes failed",
            json!({ "excluded_scopes": excluded }),
            exit,
        ));
    }

    // A fresh page-1 cursor binds only scopes that actually participated. Scopes
    // excluded during execution are signed separately and never enter the stream.
    if decoded_cursor.is_none() {
        let mut active_scope_ids = searched
            .iter()
            .map(|scope| scope.scope_id.clone())
            .collect::<Vec<_>>();
        active_scope_ids.sort();
        let mut active_configs = searched
            .iter()
            .map(|scope| ChunkingConfigBinding {
                scope_id: scope.scope_id.clone(),
                chunking_config_hash: scope.chunking_config_hash.clone(),
            })
            .collect::<Vec<_>>();
        active_configs.sort_by(|a, b| a.scope_id.cmp(&b.scope_id));
        qhash = query_hash(&QueryHashInput {
            query: parsed.query.clone(),
            mode: mode.resolved,
            scope_mode,
            scopes: active_scope_ids,
            diversify: diversify_request.clone(),
            rrf_k: rrf_config.k,
            rrf_candidate_depth: rrf_config.candidate_depth,
            rrf_w_text: rrf_config.w_text,
            rrf_w_vector: rrf_config.w_vector,
            chunking_configs: active_configs,
            time_travel: selector_for_search(&time_selector),
            query_vector_digest: query_vector_digest.clone(),
        })
        .map_err(search_to_kio)?;
    }

    // N8/PC10/PC11 (05 §1.3 L95-97, L115): the former short-query
    // (< 2 char) short-circuit to an empty page is gone — PC10 already
    // rejects the genuine zero-token case at the very top of this function
    // (before any repo/registry/index access), and PC11 gives every
    // remaining short (1-2 Unicode scalar) query real candidates via the
    // bounded replica short-query path instead of a forced
    // empty result. `searched`/`candidates` above already reflect that.

    // The replica is the sole candidate selector and has already assigned
    // global text/vector ranks before this merge and diversify stage.
    let query_vec_for_replica = matches!(mode.resolved, SearchMode::Vector | SearchMode::Hybrid)
        .then(|| query_embedding.as_deref())
        .flatten();
    // This is the candidate-read admission boundary.  A replay turns a
    // changed policy into the cursor contract error rather than recomputing
    // the page against a different visibility set.
    revalidate_current_search_policies(&searched, &current_policies, decoded_cursor.is_some())?;
    apply_current_policy_binding_filters(
        replica
            .as_ref()
            .expect("a nonempty direct search opens its replica once"),
        &mut searched,
        &current_policies,
        &time_selector,
    )
    .map_err(|reason| {
        KioError::new(
            "KIO-E-SEARCH-POLICY-001",
            "current search policy could not be evaluated; retry after repairing policy authority",
            json!({ "reason": reason }),
            ExitCode::PartialFailure,
        )
    })?;
    let eligible_images = eligible_image_pairs(&image_authorities);
    let (global_ranks, replica_candidates) = replica_candidates_for(ReplicaCandidateRequest {
        replica: replica
            .as_mut()
            .expect("a nonempty direct search opens its replica once"),
        searched: &searched,
        current_policies: &current_policies,
        image_authorities: &image_authorities,
        eligible_images: &eligible_images,
        exec_scopes: &exec_scopes,
        retainable_all_scopes: &retainable_all_scopes,
        authoritative_all_scope_registry,
        scope_mode,
        is_cursor_replay: decoded_cursor.is_some(),
        time_selector: &time_selector,
        since_cutoff: since_cutoff.as_deref(),
        match_expr: query_plan.match_expr.as_deref(),
        short_tokens: &query_plan.short_tokens,
        mode: mode.resolved,
        query_vec: query_vec_for_replica,
        config: rrf_config,
    })
    .map_err(|reason| {
        KioError::new(
            "KIO-E-AGGREGATOR-001",
            "the device search replica could not select candidates; retry after indexing",
            json!({ "reason": reason }),
            ExitCode::PartialFailure,
        )
    })?;
    let mut candidates = replica_candidates;
    // The purge barrier is deliberately opened only for scopes the replica
    // actually selected.  It is the direct-route visibility gate: disabling
    // this block makes the active-journal regression leak a replica hit.
    let searched_positions = searched
        .iter()
        .enumerate()
        .map(|(position, scope)| (scope.scope_id.as_str(), position))
        .collect::<BTreeMap<_, _>>();
    let candidate_scope_indexes = candidates
        .iter()
        .map(|candidate| candidate.scope_index)
        .collect::<BTreeSet<_>>();
    let mut blocked_candidate_scopes = BTreeSet::new();
    for scope_index in candidate_scope_indexes {
        let exec = &exec_scopes[scope_index];
        // A stale Ready header is normally rejected before candidate selection.
        // The sole exception is an already-active purge journal: its older
        // header is the last coherent projection we can inspect without reading
        // source SQLite, and the candidate barrier below must own the rejection.
        // Keep the observation durable for this request, because a purge can
        // complete after preparation but before `ReadBarrierCheckpoint::open`;
        // accepting that stale row after the journal disappears would leak it.
        let journal_active_at_prepare = searched_positions
            .get(exec.target.scope_id.as_str())
            .and_then(|position| searched.get(*position))
            .is_some_and(|scope| scope.journal_active_at_prepare);
        if journal_active_at_prepare {
            if exec.from_cursor {
                return Err(KioError::new(
                    "KIO-E-SEARCH-CURSOR-001",
                    "search cursor active scope is no longer available; re-run without a cursor",
                    json!({
                        "reason": "active_scope_unavailable",
                        "cause": "purge_journal_active",
                        "scope_id": exec.target.scope_id,
                    }),
                    ExitCode::InvalidUsage,
                ));
            }
            blocked_candidate_scopes.insert(scope_index);
            excluded.push(json!({
                "scope_id": exec.target.scope_id,
                "scope_path": exec.target.kio_dir,
                "reason": "purge_journal_active",
            }));
            continue;
        }
        match ReadBarrierCheckpoint::open(&exec.target.kio_dir, ctx.control) {
            Ok(checkpoint) => {
                if let Some(&searched_pos) = searched_positions.get(exec.target.scope_id.as_str()) {
                    late_barriers.push((scope_index, searched_pos, checkpoint));
                }
            }
            Err(error) if error.error_code() == "KIO-E-PURGE-JOURNAL-ACTIVE-001" => {
                if exec.from_cursor {
                    return Err(KioError::new(
                        "KIO-E-SEARCH-CURSOR-001",
                        "search cursor active scope is no longer available; re-run without a cursor",
                        json!({
                            "reason": "active_scope_unavailable",
                            "cause": "purge_journal_active",
                            "scope_id": exec.target.scope_id,
                        }),
                        ExitCode::InvalidUsage,
                    ));
                }
                blocked_candidate_scopes.insert(scope_index);
                excluded.push(json!({
                    "scope_id": exec.target.scope_id,
                    "scope_path": exec.target.kio_dir,
                    "reason": "purge_journal_active",
                }));
            }
            Err(error) => return Err(error),
        }
    }
    if !blocked_candidate_scopes.is_empty() {
        candidates.retain(|candidate| !blocked_candidate_scopes.contains(&candidate.scope_index));
        searched.retain(|scope| {
            exec_scopes
                .iter()
                .position(|exec| exec.target.scope_id == scope.scope_id)
                .is_none_or(|index| !blocked_candidate_scopes.contains(&index))
        });
        if searched.is_empty() {
            return Err(KioError::new(
                "KIO-E-PURGE-JOURNAL-ACTIVE-001",
                "an incomplete purge transaction is active in every scope selected by the replica; retry",
                json!({ "excluded_scopes": excluded }),
                ExitCode::PartialFailure,
            ));
        }
    }
    // A cursor replay must reproduce page 1's ordering, so page 1's DECISION is
    // part of the frozen state, not just its inputs: a page 1 that ranked
    // without the replica (a transient open failure, say) must not have page 2
    // rank with it.
    let cursor_collection_generation = decoded_cursor
        .as_ref()
        .map(|cursor| cursor.collection_generation.clone());
    // PC19/PC21's per-scope `index_generation` check has the same shape and the
    // same remedy: the state this page is replaying against moved, so the page
    // cannot be served and the caller re-runs without a cursor.
    if let Some(Some(frozen)) = &cursor_collection_generation
        && global_ranks.collection_generation != *frozen
    {
        return Err(KioError::new(
            "KIO-E-SEARCH-CURSOR-001",
            "search cursor aggregator collection generation changed",
            json!({
                "reason": "collection_generation_mismatch",
                "expected": frozen,
                "actual": global_ranks.collection_generation,
            }),
            ExitCode::InvalidUsage,
        ));
    }
    candidates.sort_by(|a, b| {
        b.rrf_score
            .total_cmp(&a.rrf_score)
            .then_with(|| a.scope_id.as_bytes().cmp(b.scope_id.as_bytes()))
            .then_with(|| a.chunk_hash.cmp(&b.chunk_hash))
            // 05 §1.7 puts the CITING chunk's pointer on an image row, so an
            // image and its chunk reach here with the same `chunk_hash` and the
            // documented `(scope_id, chunk_hash)` key no longer separates them.
            // Falling through to the row's own identity keeps the order a
            // function of content rather than of pool arrival, which is what
            // pagination (§1.5) needs. Chunk-only pools never reach this arm.
            .then_with(|| {
                a.payload
                    .row_id(&a.chunk_hash)
                    .cmp(b.payload.row_id(&b.chunk_hash))
            })
    });

    // Diversify the merged pool once (05 §1.8 step 4). Text-only -> MMR is skipped;
    // only the raw_hash dedup runs (spanning scopes and pages, CT3-MULTI-003).
    // R12-1: the effective `[search.diversify]` request drives it (was a fixed literal).
    let (ordered, diversify_summary) =
        diversify_merged(&candidates, mode.resolved, &diversify_request)?;

    // Historical/deleted path aliases inherit their semantic parent's score and
    // are expanded only after global MMR/dedup. Group order is stable by
    // (scope_id, chunk_hash, path, pointer commit), then pagination sees the final
    // hit stream rather than semantic chunks.
    let mut expanded = Vec::<ExpandedCandidate<'_>>::new();
    for candidate in ordered {
        let mut bindings = candidate.bindings.iter().collect::<Vec<_>>();
        bindings.sort_by(|left, right| {
            left.path_at_commit
                .as_bytes()
                .cmp(right.path_at_commit.as_bytes())
                .then_with(|| {
                    left.pointer_commit
                        .as_bytes()
                        .cmp(right.pointer_commit.as_bytes())
                })
        });
        expanded.extend(
            bindings
                .into_iter()
                .map(|binding| ExpandedCandidate { candidate, binding }),
        );
    }

    // R23-12 (05 §3.5 L841-855): the third, response-time barrier recheck —
    // 「本文・存在情報を返す直前」. The per-scope checkpoint-2 recheck runs at
    // scope completion, but global MMR / alias expansion / response assembly
    // still execute after it; a purge completing in that window must not let
    // this response emit the purged content. A scope failing here has its
    // already-obtained results discarded (LC55) and is excluded as
    // purge_journal_active (retryable) — same fixed 3-tuple comparison
    // (journal, purge/epoch, lifecycle counter) as checkpoints 1/2.
    let mut late_failed = std::collections::BTreeSet::<usize>::new();
    let mut searched_drop = Vec::<usize>::new();
    for (scope_index, searched_pos, checkpoint) in &late_barriers {
        if checkpoint.recheck().is_ok() {
            continue;
        }
        let exec = &exec_scopes[*scope_index];
        if exec.from_cursor {
            // CT3-CURSOR-005 posture: a replayed scope that just became
            // unavailable is a hard cursor failure, same as the Excluded arm.
            return Err(KioError::new(
                "KIO-E-SEARCH-CURSOR-001",
                "search cursor active scope is no longer available; re-run without a cursor",
                json!({
                    "reason": "active_scope_unavailable",
                    "cause": "purge_journal_active",
                    "scope_id": exec.target.scope_id,
                }),
                ExitCode::InvalidUsage,
            ));
        }
        late_failed.insert(*scope_index);
        searched_drop.push(*searched_pos);
        excluded.push(json!({
            "scope_id": exec.target.scope_id,
            "scope_path": exec.target.kio_dir,
            "reason": "purge_journal_active",
        }));
    }
    if !late_failed.is_empty() {
        expanded.retain(|hit| !late_failed.contains(&hit.candidate.scope_index));
        // These checkpoints have already failed and their rows were discarded
        // above.  Do not let the response-boundary check below turn the
        // remaining healthy scopes into a whole-command failure merely because
        // this intentionally excluded scope is still mid-purge.
        late_barriers.retain(|(scope_index, _, _)| !late_failed.contains(scope_index));
        searched_drop.sort_unstable_by(|left, right| right.cmp(left));
        for position in searched_drop {
            searched.remove(position);
        }
        if searched.is_empty() {
            return Err(KioError::new(
                "KIO-E-PURGE-JOURNAL-ACTIVE-001",
                "an incomplete purge transaction is active, or completed while this read was \
                 in flight, in every searched scope; retry",
                json!({ "excluded_scopes": excluded }),
                ExitCode::PartialFailure,
            ));
        }
    }

    // Global skip: cursor consumed (summed across scopes) or --offset (05 §1.5).
    let total_skip = match &decoded_cursor {
        Some(cursor) => cursor
            .scopes
            .iter()
            .map(|scope| scope.consumed)
            .sum::<u64>() as usize,
        None => parsed.offset.unwrap_or(0) as usize,
    };
    let limit = parsed.limit as usize;
    let slice_start = total_skip.min(expanded.len());
    let slice_end = slice_start.saturating_add(limit).min(expanded.len());
    let page = &expanded[slice_start..slice_end];

    // Per-scope consumed = candidates from that scope within the first `slice_end`
    // positions of the deterministic stream (uniform for cursor and --offset,
    // CT3-CURSOR-006). Preserves the frozen scope set even at 0 consumed.
    let mut signed_exclusions = excluded
        .iter()
        .filter_map(|entry| {
            Some(CursorExcludedScope {
                scope_id: entry.get("scope_id")?.as_str()?.to_owned(),
                reason: entry.get("reason")?.as_str()?.to_owned(),
            })
        })
        .collect::<Vec<_>>();
    signed_exclusions.sort_by(|a, b| a.scope_id.cmp(&b.scope_id));
    signed_exclusions.dedup_by(|a, b| a.scope_id == b.scope_id);
    // A GC marker freezes a tree-only sweep and forces search pagination to
    // linearize at page 1: never mint a token that could be replayed across
    // the pre/post sweep index-generation rotations.  The marker is read by
    // core through its descriptor-bound nofollow validator, not as an
    // ambient pathname probe.
    let gc_recovery_pending = gc_marker_active_for_scopes(&searched, ctx.control)?;
    if gc_recovery_pending && decoded_cursor.is_some() {
        return Err(KioError::new(
            "KIO-E-SEARCH-CURSOR-001",
            "search cursor cannot be replayed while GC shallow-sweep recovery is pending; re-run without a cursor",
            json!({ "reason": "gc_sweep_recovery_pending" }),
            ExitCode::InvalidUsage,
        ));
    }
    let next_cursor = if slice_end < expanded.len() && !gc_recovery_pending {
        let mut consumed = vec![0u64; exec_scopes.len()];
        for hit in &expanded[..slice_end] {
            consumed[hit.candidate.scope_index] += 1;
        }
        let mut sub_cursors = exec_scopes
            .iter()
            .enumerate()
            .filter_map(|(idx, exec)| {
                let searched_scope = searched
                    .iter()
                    .find(|scope| scope.scope_id == exec.target.scope_id)?;
                Some(ScopeCursor {
                    scope_id: exec.target.scope_id.clone(),
                    snapshot_commit: searched_scope.snapshot_at.clone(),
                    policy_digest: current_policies
                        .get(&searched_scope.scope_id)
                        .expect("runtime searched scope has a current policy")
                        .digest()
                        .to_owned(),
                    index_generation: searched_scope.index_generation.clone(),
                    max_rowid: searched_scope.max_rowid,
                    max_association_rowid: searched_scope.max_association_rowid,
                    chunking_config_hash: searched_scope.chunking_config_hash.clone(),
                    consumed: consumed[idx],
                })
            })
            .collect::<Vec<_>>();
        sub_cursors.sort_by(|a, b| a.scope_id.cmp(&b.scope_id));
        // PC24: the cursor being replayed already carries its own digest
        // (validated above); a fresh page 1 signs the digest just computed
        // from this run's actual query embedding (`None` in text mode).
        let cursor_query_vector_digest = match &decoded_cursor {
            Some(cursor) => cursor.query_vector_digest.clone(),
            None => query_vector_digest.clone(),
        };
        Some(
            encode_cursor_token(
                &CursorToken {
                    version: CursorToken::VERSION,
                    scope_mode: cursor_mode_from_selection(scope_mode),
                    query_hash: qhash,
                    query_vector_digest: cursor_query_vector_digest,
                    // Freshly computed rather than carried over from the
                    // replayed cursor: the two are equal by the mismatch check
                    // above, and re-deriving keeps the field describing the
                    // collection that actually ranked THIS page.
                    collection_generation: Some(global_ranks.collection_generation.clone()),
                    time_travel: selector_for_search(&time_selector),
                    since_cutoff: since_cutoff.clone(),
                    excluded_scopes: signed_exclusions,
                    scopes: sub_cursors,
                },
                &cursor_key,
            )
            .map_err(search_to_kio)?,
        )
    } else {
        None
    };

    let mut results = Vec::new();
    // Resolved once for the whole response, and the per-unit figure sizes it
    // needs are memoized across rows — a page of results is usually a handful
    // of units, so this is a few reads, not one per row.
    let min_area_ratio =
        related_images_min_area_ratio(single_scope_kio_dir.as_deref(), ctx.control)?;
    let mut unit_figures: UnitFigureCache = BTreeMap::new();
    for hit in page {
        let candidate = hit.candidate;
        let binding = hit.binding;
        let pointer = issue_evidence_pointer(EvidencePointerIssueRequest {
            scope_id: candidate.scope_id.clone(),
            scope_path: Some(candidate.scope_path.display().to_string()),
            commit: binding.pointer_commit.clone(),
            tree: None,
            raw_hash: candidate.meta.raw_hash.clone(),
            tool_profile_hash: candidate.meta.tool_profile_hash.clone(),
            chunk_hash: candidate.chunk_hash.clone(),
            path_at_commit: Some(binding.path_at_commit.clone()),
            heading_path: candidate.meta.heading_path.clone(),
            section_id: candidate.meta.section_id.clone(),
            byte_start: Some(candidate.meta.byte_start),
            byte_end: Some(candidate.meta.byte_end),
        })
        .map_err(search_to_kio)?;
        let uri = evidence_pointer_to_uri(&pointer).map_err(search_to_kio)?;
        // 05 §1.7: every row declares what it points at, and `evidence_pointer`
        // is a CHUNK's either way — an object URI carries no commit, tree or
        // path, so it satisfies neither time-travel nor `evidence verify`.
        let mut result = json!({
            "result_type": candidate.payload.result_type(),
            "chunk_hash": candidate.chunk_hash,
            "evidence_pointer": pointer,
            "evidence_uri": uri,
            "score": candidate.rrf_score,
            "scope_path": candidate.scope_path,
            "title": binding.path_at_commit,
        });
        match &candidate.payload {
            ResultPayload::Chunk => {
                // The payload IS this chunk, so `evidence_uri` already points at
                // it and 05 §1.7 omits `payload_uri` here.
                result["snippet"] =
                    json!(candidate.meta.text.chars().take(200).collect::<String>());
            }
            ResultPayload::Image { image_uri, .. } => {
                // What the Agent opens to get the bytes. Never base64 — 05 §1.7
                // is explicit that the response passes URIs, since inlining
                // images would hit the Agent's context and cost directly and
                // contradict a contract that otherwise returns pointers.
                result["payload_uri"] = json!(image_uri);
                // No `snippet`: its documented meaning is the start of THIS
                // row's chunk body, and this row is not a chunk. The citing
                // text is reachable through the chunk row that carries the same
                // pointer, where it means what it says.
            }
        }
        if time_selector.all_history() && !binding.current_paths.is_empty() {
            result["current_paths"] = json!(binding.current_paths);
            if let Some(current_path) = binding.current_path() {
                result["current_path"] = json!(current_path);
            }
        }
        // 05 §1.7: the images this chunk body references, so a multimodal Agent
        // can `kio open` them. Derived from the already-loaded chunk text — no
        // index lookup and no existence check (a purged image may still be
        // named here; `kio open`'s purge barrier is what terminates that).
        // Omitted entirely when empty, like `current_paths` above.
        //
        // Chunk rows only. 05 §1.7 defines the field as "この chunk 本文が参照
        // している画像の列挙", and on an image row the enumeration would be of
        // the CITING chunk's figures — including this row's own payload, which
        // is already `payload_uri`.
        if candidate.payload == ResultPayload::Chunk {
            let mut related_images = extract_related_images(&candidate.meta.text);
            // Then keep only what is worth opening. The extractor above must
            // stay exhaustive (it doubles as the liveness scanner), so the
            // narrowing happens here and only for what this response shows.
            if min_area_ratio > 0.0 && !related_images.is_empty() {
                let key = (
                    candidate.meta.raw_hash.clone(),
                    candidate.meta.tool_profile_hash.clone(),
                    candidate.meta.r#gen,
                    binding.manifest_hash.clone(),
                );
                let instance = unit_figures.entry(key).or_insert_with(|| {
                    read_instance_figures(
                        search_repositories
                            .get(&candidate.scope_id)
                            .expect("eligible search scope retains its repository"),
                        &candidate.meta.raw_hash,
                        &candidate.meta.tool_profile_hash,
                        candidate.meta.r#gen,
                        &binding.manifest_hash,
                    )
                });
                if let Some(figures) = instance.get(&candidate.meta.unit_key) {
                    related_images = significant_related_images(
                        related_images,
                        figures,
                        min_area_ratio,
                        &candidate.scope_id,
                    );
                }
            }
            if !related_images.is_empty() {
                result["related_images"] = json!(related_images);
            }
        }
        results.push(result);
    }

    let searched_scopes = searched
        .iter()
        .map(|scope| {
            let mut entry = json!({
                "scope_id": scope.scope_id,
                "scope_path": scope.scope_path,
                "snapshot_at": scope.snapshot_at,
            });
            // PC46 / §R note-3 ruling (2026-07-22): a per-scope field, count
            // omitted when zero — not a top-level aggregate, not folded into
            // excluded_scopes (the scope is not excluded, only partially
            // degraded).
            if scope.shallow_skipped > 0 {
                entry["shallow_skipped"] = json!(scope.shallow_skipped);
            }
            entry
        })
        .collect::<Vec<_>>();
    let index_status = compute_index_status(
        &searched,
        budget_ledger.as_ref(),
        &ledger_path,
        &budget_month,
        ctx.control,
        ctx.locks,
    )?;
    // PC45/PC46: a scope that skipped shallow ancestors mid-walk is not fully
    // complete even though it was not excluded — same partial-failure
    // treatment (exit 3) as an excluded_scopes entry (05 §1.6 "黙って欠落
    // させない").
    let any_shallow_skipped = searched.iter().any(|scope| scope.shallow_skipped > 0);
    let partial_failure = !excluded.is_empty() || any_shallow_skipped;

    let mut response = json!({
        "query": parsed.query,
        "requested_mode": search_mode_json(mode.requested),
        "resolved_mode": search_mode_json(mode.resolved),
        "fallback": mode.fallback,
        "fallback_reason": mode.fallback_reason.clone().map(Value::from).unwrap_or(Value::Null),
        "error_code": mode.error_code.clone().map(Value::from).unwrap_or(Value::Null),
        // PC3 / §R note-1 ruling: `warnings[]` array (replaces the retired
        // singular `warning` field) — non-empty only under
        // [search].fail_behavior = "warn" text fallback.
        "warnings": mode.warnings.clone(),
        "diversify": diversify_summary,
        "aggregator": {
            "collection_generation": global_ranks.collection_generation,
        },
        "paging": { "limit": parsed.limit, "next_cursor": next_cursor },
        "searched_scopes": searched_scopes,
        "excluded_scopes": excluded,
        "index_status": index_status,
        "results": results,
    });
    if gc_recovery_pending {
        response["gc_recovery_pending"] = json!({
            "reason": "gc_sweep_active",
            "next_cursor_suppressed": true,
        });
    }
    append_search_logs(ctx, &repo, &response, started);
    if partial_failure {
        // Some scopes were excluded, or a searched scope skipped shallow
        // ancestors, but others succeeded: emit results on stdout and exit 3
        // (05 §1.8 partial-failure row, CT3-MULTI-005; PC45/PC46).
        if let Some(object) = response.as_object_mut() {
            object.insert("__exit_code".to_owned(), json!(3));
        }
    }

    // The earlier candidate-time recheck preserves per-scope isolation before
    // pagination.  Response assembly can still take long enough for a purge
    // to start after that check, however (pointer signing, related-image
    // materialization, cursor construction, and log preparation all happen
    // above).  Recheck the same frozen checkpoints at the actual response
    // boundary so no selected scope's snippet, title, or pointer can escape
    // through that final TOCTOU window.  On a failure the fully assembled
    // response is deliberately dropped rather than trying to mutate a signed
    // cursor or partially built JSON body after the fact.
    maybe_wait_at_test_search_response_barrier(ctx.control);
    revalidate_current_search_policies(&searched, &current_policies, decoded_cursor.is_some())?;
    recheck_search_response_barriers(&late_barriers, &exec_scopes)?;
    // A marker may have appeared while the result body was assembled.  Check
    // again at the actual response boundary and redact a freshly minted token
    // rather than leaking a cursor across the sweep transition.
    if gc_marker_active_for_scopes(&searched, ctx.control)? {
        if decoded_cursor.is_some() {
            return Err(KioError::new(
                "KIO-E-SEARCH-CURSOR-001",
                "search cursor cannot be replayed while GC shallow-sweep recovery is pending; re-run without a cursor",
                json!({ "reason": "gc_sweep_recovery_pending" }),
                ExitCode::InvalidUsage,
            ));
        }
        response["paging"]["next_cursor"] = Value::Null;
        response["gc_recovery_pending"] = json!({
            "reason": "gc_sweep_active",
            "next_cursor_suppressed": true,
        });
    }
    Ok(response)
}
