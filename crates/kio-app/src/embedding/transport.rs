use crate::approvals;
use crate::egress::persistent_network_allowed_for;
use crate::embedding::admission::EMBEDDING_ADAPTER_KIND;
use crate::embedding::admission::declared_embedding_profile;
use crate::embedding::admission::embedding_billed_from_usage;
use crate::embedding::admission::embedding_execution;
use crate::embedding::admission::embedding_is_deterministic;
use crate::embedding::admission::embedding_text_reservation;
use crate::embedding::admission::embedding_transport_allowed;
use crate::embedding::admission::query_embedding_send_lane;
use crate::invocation::Invocation;
use crate::ledger_access::TASK_SYNC_EFFECTIVE_TIMEOUT_SECONDS;
use crate::ledger_access::budget_cap_config;
use crate::ledger_access::open_optional_ledger_db;
use crate::ledger_access::settle_task_charge_success;
use crate::ledger_access::settle_task_charge_unknown;
use crate::markdownize::execution::TaskExecutionFailure;
use crate::markdownize::execution::task_failure_from_adapter;
use crate::object_persistence::pipeline_to_kio;
use crate::scope_access::maybe_wait_at_test_search_final_consent_barrier;
use crate::search::scope::ExecScope;
use crate::search::scope::revalidate_current_policy_map;
use crate::settings::user_config_toml_path;
use kio_adapter::AdapterError;
use kio_adapter::catalog::EmbeddingExecution;
use kio_adapter::catalog::embedding_descriptor_with_control;
use kio_adapter::catalog::{EmbeddingRunError, run_embedding_attempt_with_control};
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::types::EmbeddingCompleted;
use kio_adapter::types::EmbeddingInputType;
use kio_adapter::types::EmbeddingItem;
use kio_adapter::types::EmbeddingLogicalJobId;
use kio_adapter::types::EmbeddingMemberResult;
use kio_adapter::types::EmbeddingUsage;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::scope::Repository;
use kio_pipeline::budget::read_budget_policy;
use kio_pipeline::ledger::TaskKey as LedgerTaskKey;
use kio_pipeline::ledger::embedding_job::EmbeddingDeviceClaimOutcome;
use kio_pipeline::ledger::embedding_job::EmbeddingDeviceClaimRequest;
use kio_pipeline::ledger::embedding_job::EmbeddingJobBinding;
use kio_pipeline::ledger::embedding_job::EmbeddingJobMember;
use kio_pipeline::ledger::ops::claim_device_embedding_job;
use kio_pipeline::ledger::ops::device_input_hash;
use kio_pipeline::ledger::ops::finish_embedding_publication;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::task::EmbeddingTaskTarget;
use kio_pipeline::task::RetryErrorKind;
use std::collections::BTreeMap;
#[cfg(debug_assertions)]
use std::fs::OpenOptions;
#[cfg(debug_assertions)]
use std::io::Write;
use std::sync::Arc;

/// Run the active embedding adapter over a batch of items, returning one vector
/// per item.
pub(crate) enum EmbeddingRunFailure {
    BeforeDispatch(kio_core::KioError),
    Failure(TaskExecutionFailure),
}
impl From<TaskExecutionFailure> for EmbeddingRunFailure {
    fn from(failure: TaskExecutionFailure) -> Self {
        Self::Failure(failure)
    }
}

pub(crate) fn run_embedding_adapter(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    execution: EmbeddingExecution,
    items: Vec<EmbeddingItem>,
    input_type: EmbeddingInputType,
    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): the caller's
    // ledger `intent_token` for this send — see
    // `kio_adapter::types::EmbeddingRequest::idempotency_token`'s doc.
    idempotency_token: Option<String>,
    control: &ControlHandle,
) -> std::result::Result<EmbeddingCompleted, EmbeddingRunFailure> {
    run_embedding_adapter_unclassified(
        adapter_settings,
        execution,
        items,
        input_type,
        idempotency_token,
        control,
    )
    .map_err(|error| match error {
        EmbeddingRunError::BeforeDispatch(error) => {
            EmbeddingRunFailure::BeforeDispatch(crate::object_persistence::adapter_to_kio(error))
        }
        EmbeddingRunError::DispatchPossible(error) => {
            EmbeddingRunFailure::Failure(task_failure_from_adapter(error))
        }
    })
}

fn run_embedding_adapter_unclassified(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    execution: EmbeddingExecution,
    items: Vec<EmbeddingItem>,
    input_type: EmbeddingInputType,
    idempotency_token: Option<String>,
    control: &ControlHandle,
) -> std::result::Result<EmbeddingCompleted, EmbeddingRunError> {
    #[cfg(debug_assertions)]
    if let Some(path) = &control.debug().cli.task_embed_trace {
        let input_type_name = match input_type {
            EmbeddingInputType::Text | EmbeddingInputType::MarkdownChunk => "text",
            EmbeddingInputType::Image | EmbeddingInputType::ImageObject => "image",
            EmbeddingInputType::Query => "query",
        };
        let mut line = serde_json::json!({
            "input_type": input_type_name,
            "member_ids": items.iter().map(|item| &item.id).collect::<Vec<_>>(),
            "intent_token": &idempotency_token,
        })
        .to_string();
        line.push('\n');
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(line.as_bytes()));
    }
    let profile = declared_embedding_profile(execution);
    let expected = EmbeddingAcceptance {
        dimensions: u32::try_from(profile.dimensions).map_err(|_| {
            EmbeddingRunError::BeforeDispatch(AdapterError::ContractViolation(
                "embedding dimensions exceed u32".to_owned(),
            ))
        })?,
        distance: &profile.distance,
        modality: &profile.modality,
        profile_hash: &profile.profile_hash,
        member_ids: items.iter().map(|item| item.id.clone()).collect(),
    };
    // The selected factory is the last unsent boundary after paid reservation
    // and the caller's final policy admission. Release builds are inert.
    if execution.is_online() {
        control
            .checkpoint(kio_core::durability::DurabilityPoint::EmbeddingBeforeFactory)
            .map_err(|error| {
                EmbeddingRunError::BeforeDispatch(AdapterError::RuntimeTrust(error))
            })?;
    }
    let outcome = run_embedding_attempt_with_control(
        std::sync::Arc::clone(adapter_settings),
        execution,
        items,
        input_type,
        idempotency_token,
        control,
    )?;
    outcome
        .into_completed()
        .and_then(|mut completed| {
            validate_completed_mapping(&completed, &expected)?;
            for member in &mut completed.members {
                if let EmbeddingMemberResult::Vector { vector } = &mut member.result {
                    normalize_inline_vector(vector, expected.dimensions)?;
                }
            }
            validate_completed_embedding(&completed, &expected)?;
            if completed
                .members
                .iter()
                .any(|member| matches!(member.result, EmbeddingMemberResult::Failed { .. }))
            {
                return Err(AdapterError::ContractViolation(
                    "synchronous embedding member failed".to_owned(),
                ));
            }
            Ok(completed)
        })
        .map_err(EmbeddingRunError::DispatchPossible)
}

/// Frozen completion contract, shared by inline and collected Batch results.
/// Acceptance checks inspect f32 values without rewriting their bytes.
pub(crate) struct EmbeddingAcceptance<'a> {
    pub(crate) dimensions: u32,
    pub(crate) distance: &'a str,
    pub(crate) modality: &'a str,
    pub(crate) profile_hash: &'a str,
    pub(crate) member_ids: Vec<String>,
}

pub(crate) fn validate_completed_embedding(
    completed: &EmbeddingCompleted,
    expected: &EmbeddingAcceptance<'_>,
) -> kio_adapter::Result<()> {
    validate_completed_mapping(completed, expected)?;
    for member in &completed.members {
        match &member.result {
            EmbeddingMemberResult::Vector { vector } => {
                kio_adapter::types::validate_cosine_vector(vector, expected.dimensions)?;
                let norm = vector
                    .iter()
                    .map(|value| f64::from(*value).powi(2))
                    .sum::<f64>()
                    .sqrt();
                if (norm - 1.0).abs() > 0.0001 {
                    return Err(AdapterError::ContractViolation(
                        "embedding vector is not normalized".to_owned(),
                    ));
                }
            }
            EmbeddingMemberResult::Failed { reason }
                if reason.is_empty()
                    || reason.len() > 256
                    || reason.chars().any(char::is_control) =>
            {
                return Err(AdapterError::ContractViolation(
                    "invalid embedding member failure".to_owned(),
                ));
            }
            EmbeddingMemberResult::Failed { .. } => {}
        }
    }
    Ok(())
}

fn validate_completed_mapping(
    completed: &EmbeddingCompleted,
    expected: &EmbeddingAcceptance<'_>,
) -> kio_adapter::Result<()> {
    if completed.dimensions != expected.dimensions
        || completed.distance != expected.distance
        || completed.modality != expected.modality
        || completed.embedding_profile_hash != expected.profile_hash
        || completed.members.len() != expected.member_ids.len()
    {
        return Err(AdapterError::ContractViolation(
            "embedding completion metadata mismatch".to_owned(),
        ));
    }
    let ids = expected
        .member_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    if ids.len() != expected.member_ids.len() {
        return Err(AdapterError::ContractViolation(
            "duplicate embedding request member".to_owned(),
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for member in &completed.members {
        if !ids.contains(member.id.as_str()) || !seen.insert(member.id.as_str()) {
            return Err(AdapterError::ContractViolation(
                "embedding completion member mismatch".to_owned(),
            ));
        }
    }
    Ok(())
}

fn normalize_inline_vector(vector: &mut [f32], dimensions: u32) -> kio_adapter::Result<()> {
    kio_adapter::types::validate_cosine_vector(vector, dimensions)?;
    let norm = vector
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    if (norm - 1.0).abs() <= 0.0001 {
        return Ok(());
    }
    for value in vector {
        *value = (f64::from(*value) / norm) as f32;
    }
    Ok(())
}

#[cfg(test)]
mod completion_numeric_tests {
    use super::*;
    use kio_adapter::types::{EmbeddingMemberOutcome, EmbeddingUsage};

    #[test]
    fn completion_norm_matches_cas_boundary_without_rewriting_valid_f32() {
        let expected = EmbeddingAcceptance {
            dimensions: 3,
            distance: "cosine",
            modality: "multimodal",
            profile_hash: "profile",
            member_ids: vec!["one".to_owned()],
        };
        let mut result = EmbeddingCompleted {
            members: vec![EmbeddingMemberOutcome {
                id: "one".to_owned(),
                result: EmbeddingMemberResult::Vector {
                    vector: vec![1.0, 0.0, 0.0],
                },
            }],
            dimensions: 3,
            distance: "cosine".to_owned(),
            modality: "multimodal".to_owned(),
            embedding_profile_hash: "profile".to_owned(),
            usage: EmbeddingUsage::Unmetered,
        };
        let before = result.clone();
        validate_completed_embedding(&result, &expected).unwrap();
        assert_eq!(result, before);
        result.members[0].result = EmbeddingMemberResult::Vector {
            vector: vec![1.0002, 0.0, 0.0],
        };
        assert!(validate_completed_embedding(&result, &expected).is_err());
        if let EmbeddingMemberResult::Vector { vector } = &mut result.members[0].result {
            normalize_inline_vector(vector, expected.dimensions).unwrap();
        }
        validate_completed_embedding(&result, &expected).unwrap();
    }
}

/// The `fallback_reason`s [`compute_query_embedding_page1`] can report through
/// `VectorAvailability::Unavailable` beyond the pre-existing
/// `query_embedding_unavailable` (05 §1.7).
pub(crate) enum QueryEmbeddingOutcome {
    Vector(Vec<f32>),
    /// CL54: a live (non-stale) in-flight claim from another process already
    /// holds this exact query's device row — never send, never overwrite its
    /// token.
    InFlight,
    /// PC7 (05 §1.1 L32 / 07-adapter-spec.md §5.3): the adapter's response
    /// failed its acceptance check (e.g. a dimension mismatch) — a distinct
    /// technical failure from a plain adapter error.
    ContractViolation,
    /// PC6 (05 §1.1 L50-53 / 07 §3 L144-146): the final pre-attempt re-read
    /// found the send consent gate no longer satisfied (a revoke completed
    /// between the caller's precheck and this re-read) — refuse the send.
    /// Maps to
    /// `VectorAvailability::Unauthorized`, not `Unavailable`, at the caller.
    NotAuthorized,
}

/// Fresh vector/hybrid page-one query admission. The device claim and send
/// are shared by local and remote executions below; text and cursor replay
/// never reach this function. A remote execution must retain an exact grant
/// under the chosen scope's writer lease before any ledger write.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_query_embedding_page1(
    invocation: &Invocation<'_>,
    query: &str,
    exec_scopes: &[ExecScope],
    search_repositories: &BTreeMap<String, Repository>,
    tool_id: &str,
    offline: bool,
    current_policies: &BTreeMap<String, CurrentPolicyEvaluator>,
    budget_error: Option<kio_core::KioError>,
) -> Result<Option<QueryEmbeddingOutcome>> {
    let adapter_settings = invocation.settings;

    if query.chars().count() < 2 {
        return Ok(None);
    }
    let Some(execution) = embedding_execution(adapter_settings, invocation.control) else {
        return Ok(None);
    };
    if embedding_is_deterministic(execution) {
        revalidate_current_policy_map(current_policies, false)?;
        return run_query_embedding_job(invocation, execution, query);
    }
    if !embedding_transport_allowed(execution, offline) {
        return Ok(None);
    }
    let profile = declared_embedding_profile(execution);

    // PC6: the final authorization check is deliberately before *any* writable
    // source ledger action. A revoked auto/hybrid candidate therefore resolves to its
    // normal text fallback without opening the ledger, refreshing write-seq,
    // sweeping stale rows, or creating a claim. Once the check succeeds, later
    // claim/reservation/send/settlement outcomes remain the normal fresh-page-1
    // metered path.
    // Select one currently-authorized scope deterministically, then acquire only
    // that scope's retained writer lease. Rechecking the exact device+scope grant
    // while the lease is held makes a concurrent revoke either win before this
    // request or wait until this in-flight request completes.
    let mut candidates = exec_scopes
        .iter()
        .filter_map(|exec| {
            search_repositories
                .get(&exec.target.scope_id)
                .map(|repo| (exec.target.scope_id.as_str(), repo))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(scope_id, _)| *scope_id);
    let mut authorizing_repo = None;
    for (_, repo) in candidates {
        if persistent_network_allowed_for(
            adapter_settings,
            repo,
            approvals::AdapterRole::Embedding,
            tool_id,
            &profile.profile_hash,
        )? {
            authorizing_repo = Some(repo);
            break;
        }
    }
    let Some(authorizing_repo) = authorizing_repo else {
        return Ok(Some(QueryEmbeddingOutcome::NotAuthorized));
    };
    maybe_wait_at_test_search_final_consent_barrier(invocation.control);
    let _authorizing_scope_lock =
        authorizing_repo.lock_store_in_session(authorizing_repo.lock_session())?;
    revalidate_current_policy_map(current_policies, false)?;
    if !persistent_network_allowed_for(
        adapter_settings,
        authorizing_repo,
        approvals::AdapterRole::Embedding,
        tool_id,
        &profile.profile_hash,
    )? {
        return Ok(Some(QueryEmbeddingOutcome::NotAuthorized));
    }

    if !embedding_is_free(adapter_settings, execution, invocation.control)?
        && let Some(error) = budget_error
    {
        return Err(error);
    }
    run_query_embedding_job(invocation, execution, query)
}

/// One device claim, adapter completion, validation and terminal outcome for
/// every fresh query posture. The caller owns consent and policy admission.
fn run_query_embedding_job(
    invocation: &Invocation<'_>,
    execution: EmbeddingExecution,
    query: &str,
) -> Result<Option<QueryEmbeddingOutcome>> {
    let adapter_settings = invocation.settings;
    let profile = declared_embedding_profile(execution);
    let descriptor =
        embedding_descriptor_with_control(adapter_settings, execution, invocation.control)
            .map_err(crate::object_persistence::adapter_to_kio)?;
    if !descriptor.capability.text || !descriptor.completion.sync {
        return Ok(None);
    }
    if embedding_is_free(adapter_settings, execution, invocation.control)? {
        return run_free_query_embedding_job(invocation, execution, query);
    }
    let Some(ledger) = open_optional_ledger_db(invocation.control)? else {
        return Ok(None);
    };
    let input_hash = device_input_hash(query);
    let key = LedgerTaskKey::new(
        LedgerTaskKey::DEVICE_SCOPE_ID,
        EMBEDDING_ADAPTER_KIND,
        input_hash.clone(),
        profile.profile_hash.clone(),
    );
    let candidate_usd = embedding_text_reservation(adapter_settings, query_embedding_send_lane());
    let device_config_path = user_config_toml_path(invocation.control);
    let no_folder_cap_sentinel =
        device_config_path.with_file_name("__kio_device_claim_no_folder_cap__.toml");
    let budget_caps = read_budget_policy(&device_config_path, &no_folder_cap_sentinel)
        .map_err(pipeline_to_kio)?;
    let device_cap_config = budget_cap_config(
        &budget_caps,
        LedgerTaskKey::DEVICE_SCOPE_ID,
        EMBEDDING_ADAPTER_KIND,
    );
    let binding = EmbeddingJobBinding {
        logical_job_id: EmbeddingLogicalJobId(input_hash.clone()),
        dimensions: u32::try_from(profile.dimensions)
            .map_err(|_| kio_core::KioError::schema("embedding profile dimensions exceed u32"))?,
        distance: profile.distance.clone(),
        modality: profile.modality.clone(),
        members: vec![EmbeddingJobMember {
            id: "query".to_owned(),
            target: EmbeddingTaskTarget::Query {
                query_hash: input_hash.clone(),
                profile_hash: profile.profile_hash.clone(),
                input_hash,
            },
        }],
    };
    let claim = claim_device_embedding_job(
        &ledger,
        &EmbeddingDeviceClaimRequest {
            key: &key,
            now_ms: kio_pipeline::ledger::time::now_millis(),
            candidate_usd,
            timeout_seconds: TASK_SYNC_EFFECTIVE_TIMEOUT_SECONDS,
            device_cap: device_cap_config.device_cap,
            per_adapter_cap: device_cap_config.device_per_adapter_cap,
            binding: &binding,
        },
    )
    .map_err(pipeline_to_kio)?;
    let job = match claim {
        EmbeddingDeviceClaimOutcome::Claimed(job) => job,
        EmbeddingDeviceClaimOutcome::InFlight => return Ok(Some(QueryEmbeddingOutcome::InFlight)),
        EmbeddingDeviceClaimOutcome::Denied(_) => return Ok(None),
    };
    record_query_embed_trace(query, invocation.control);
    match run_embedding_adapter(
        adapter_settings,
        execution,
        vec![EmbeddingItem::text("query", query)],
        EmbeddingInputType::Query,
        Some(job.intent_token.clone()),
        invocation.control,
    ) {
        Ok(outcome) => {
            let billed = embedding_billed_from_usage(
                adapter_settings,
                match &outcome.usage {
                    EmbeddingUsage::Metered { usage } => Some(usage),
                    EmbeddingUsage::Estimated | EmbeddingUsage::Unmetered => None,
                },
                query_embedding_send_lane(),
                candidate_usd,
                invocation.control,
                invocation.locks,
                EmbeddingInputType::Query,
            );
            settle_task_charge_success(&ledger, &key, &job.intent_token, billed)?;
            finish_embedding_publication(&ledger, &job).map_err(pipeline_to_kio)?;
            Ok(outcome.members.into_iter().find_map(|member| {
                if member.id != "query" {
                    return None;
                }
                match member.result {
                    EmbeddingMemberResult::Vector { vector } => {
                        Some(QueryEmbeddingOutcome::Vector(vector))
                    }
                    EmbeddingMemberResult::Failed { .. } => None,
                }
            }))
        }
        Err(EmbeddingRunFailure::BeforeDispatch(error)) => {
            kio_pipeline::ledger::embedding_job::reject_unsent_embedding_job(&ledger, &job)
                .map_err(pipeline_to_kio)?;
            Err(error)
        }
        Err(EmbeddingRunFailure::Failure(failure)) => {
            settle_task_charge_unknown(&ledger, &key, &job.intent_token, candidate_usd)?;
            finish_embedding_publication(&ledger, &job).map_err(pipeline_to_kio)?;
            if failure.retry_kind == RetryErrorKind::ContractViolation {
                Ok(Some(QueryEmbeddingOutcome::ContractViolation))
            } else {
                Ok(None)
            }
        }
    }
}

/// Append the query to the file named by `KIO_TEST_QUERY_EMBED_TRACE`, if set, at
/// the point the query embedding is sent (O2 test seam only; no-op in production).
#[cfg(debug_assertions)]
fn record_query_embed_trace(query: &str, control: &ControlHandle) {
    let trace_path = control.debug().cli.query_embed_trace.clone();
    if let Some(path) = trace_path {
        let mut line = query.to_owned();
        line.push('\n');
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(line.as_bytes()));
    }
}

#[cfg(not(debug_assertions))]
fn record_query_embed_trace(_query: &str, _control: &ControlHandle) {}

#[cfg(test)]
#[path = "query_acceptance_tests.rs"]
mod query_acceptance_tests;

/// Free authority is an explicit adapter effect and a local execution type.
/// Numeric prices, missing credentials, and missing usage never grant it.
pub(crate) fn embedding_is_free(
    settings: &Arc<AdapterRuntimeSettings>,
    execution: EmbeddingExecution,
    control: &ControlHandle,
) -> Result<bool> {
    Ok(!execution.is_online()
        && !embedding_descriptor_with_control(settings, execution, control)
            .map_err(crate::object_persistence::adapter_to_kio)?
            .effects
            .paid_authorization)
}

pub(crate) fn local_embedding_identity(
    target: crate::local_execution::LocalTarget,
    input_hash: String,
    profile: &kio_adapter::catalog::DeclaredEmbeddingProfile,
    execution: EmbeddingExecution,
    members: Vec<kio_pipeline::ledger::embedding_job::EmbeddingJobMember>,
) -> Result<crate::local_execution::LocalIdentity> {
    Ok(crate::local_execution::LocalIdentity {
        execution: if embedding_is_deterministic(execution) {
            crate::local_execution::LocalExecutionKind::InProcess
        } else {
            crate::local_execution::LocalExecutionKind::Loopback
        },
        target,
        input_sha256: input_hash
            .strip_prefix("sha256:")
            .unwrap_or(&input_hash)
            .to_owned(),
        profile: profile.profile_hash.clone(),
        contract: crate::local_execution::LocalContract::Embedding {
            dimensions: u32::try_from(profile.dimensions)
                .map_err(|_| kio_core::KioError::schema("embedding dimensions exceed u32"))?,
            distance: profile.distance.clone(),
            modality: profile.modality.clone(),
            members: members
                .into_iter()
                .map(|member| crate::local_execution::LocalMember {
                    id: member.id,
                    target: member.target,
                })
                .collect(),
        },
    })
}

pub(crate) fn decode_local_embedding_result(
    bytes: &[u8],
    profile: &kio_adapter::catalog::DeclaredEmbeddingProfile,
    member_ids: Vec<String>,
) -> Result<EmbeddingCompleted> {
    let completed: EmbeddingCompleted = serde_json::from_slice(bytes)
        .map_err(|_| kio_core::KioError::schema("invalid local embedding completion"))?;
    validate_completed_embedding(
        &completed,
        &EmbeddingAcceptance {
            dimensions: u32::try_from(profile.dimensions)
                .map_err(|_| kio_core::KioError::schema("embedding dimensions exceed u32"))?,
            distance: &profile.distance,
            modality: &profile.modality,
            profile_hash: &profile.profile_hash,
            member_ids,
        },
    )
    .map_err(crate::object_persistence::adapter_to_kio)?;
    Ok(completed)
}

fn run_free_query_embedding_job(
    invocation: &Invocation<'_>,
    execution: EmbeddingExecution,
    query: &str,
) -> Result<Option<QueryEmbeddingOutcome>> {
    use crate::local_execution::{Claim, LocalExecution, LocalTarget};
    let profile = declared_embedding_profile(execution);
    let input_hash = device_input_hash(query);
    let identity = local_embedding_identity(
        LocalTarget::Query {
            query_sha256: input_hash
                .strip_prefix("sha256:")
                .unwrap_or(&input_hash)
                .to_owned(),
            query_kind: "query".to_owned(),
        },
        input_hash.clone(),
        &profile,
        execution,
        vec![EmbeddingJobMember {
            id: "query".to_owned(),
            target: EmbeddingTaskTarget::Query {
                query_hash: input_hash.clone(),
                profile_hash: profile.profile_hash.clone(),
                input_hash,
            },
        }],
    )?;
    let store = LocalExecution::new(
        LocalExecution::default_root(invocation.control)?,
        invocation.control.clone(),
    )?;
    let mut claim = match store.claim(
        &identity,
        0,
        None,
        local_max_attempts(execution),
        invocation.control.now_millis(),
    )? {
        Claim::Claimed(claim) => claim,
        Claim::Busy => return Ok(Some(QueryEmbeddingOutcome::InFlight)),
        Claim::Backoff { .. } | Claim::Exhausted { .. } => return Ok(None),
    };
    let recovered_bytes = claim.result()?;
    let completed = if let Some(bytes) = recovered_bytes {
        decode_local_embedding_result(&bytes, &profile, vec!["query".to_owned()])?
    } else {
        record_query_embed_trace(query, invocation.control);
        match run_local_embedding_adapter(
            invocation.settings,
            execution,
            vec![EmbeddingItem::text("query", query)],
            EmbeddingInputType::Query,
            Some(claim.token().to_owned()),
            invocation.control,
        ) {
            Ok(completed) => {
                let bytes = serde_json::to_vec(&completed).map_err(|_| {
                    kio_core::KioError::schema("cannot serialize local embedding result")
                })?;
                claim.stage_result(&bytes)?;
                completed
            }
            Err(LocalEmbeddingRunFailure::Classified(failure)) => {
                let policy = kio_pipeline::task::retry_policy(failure.retry_kind);
                let now = invocation.control.now_utc_seconds();
                let next = if failure.retry_kind == RetryErrorKind::RateLimit {
                    Some(crate::task_execution::rate_limit_retry_at(
                        &now,
                        failure.retry_after_ms,
                    ))
                } else if policy.retryable
                    && policy.max_attempts.is_none_or(|max| claim.attempts() < max)
                {
                    Some(crate::task_execution::scheduled_retry_at(
                        &now,
                        &policy.backoff,
                        claim.attempts(),
                    ))
                } else {
                    None
                };
                claim.fail(
                    failure.retry_kind,
                    next.as_deref()
                        .and_then(kio_core::scope::parse_utc_seconds)
                        .map(|seconds| seconds * 1000),
                    !matches!(
                        failure.retry_kind,
                        RetryErrorKind::RateLimit | RetryErrorKind::AuthError
                    ),
                    failure.retry_kind != RetryErrorKind::AuthError
                        && (!policy.retryable || next.is_none()),
                )?;
                return if failure.retry_kind == RetryErrorKind::ContractViolation {
                    Ok(Some(QueryEmbeddingOutcome::ContractViolation))
                } else {
                    Ok(None)
                };
            }
            Err(LocalEmbeddingRunFailure::Admission(error)) => {
                claim.cancel_unsent()?;
                return Err(error);
            }
            Err(LocalEmbeddingRunFailure::InProcessIo(error)) => {
                claim.interrupt()?;
                return Err(local_runtime_error(error));
            }
        }
    };
    let vector = completed
        .members
        .into_iter()
        .find_map(|member| match member.result {
            EmbeddingMemberResult::Vector { vector } if member.id == "query" => Some(vector),
            _ => None,
        });
    claim.publish()?;
    claim.retire()?;
    Ok(vector.map(QueryEmbeddingOutcome::Vector))
}

pub(crate) enum LocalEmbeddingRunFailure {
    Classified(TaskExecutionFailure),
    Admission(kio_core::KioError),
    InProcessIo(AdapterError),
}

pub(crate) fn local_max_attempts(execution: EmbeddingExecution) -> u32 {
    if embedding_is_deterministic(execution) {
        3
    } else {
        5
    }
}

pub(crate) fn run_local_embedding_adapter(
    settings: &Arc<AdapterRuntimeSettings>,
    execution: EmbeddingExecution,
    items: Vec<EmbeddingItem>,
    input_type: EmbeddingInputType,
    token: Option<String>,
    control: &ControlHandle,
) -> std::result::Result<EmbeddingCompleted, LocalEmbeddingRunFailure> {
    run_embedding_adapter_unclassified(settings, execution, items, input_type, token, control)
        .map_err(|error| match error {
            EmbeddingRunError::BeforeDispatch(error) => LocalEmbeddingRunFailure::Admission(
                crate::object_persistence::adapter_to_kio(error),
            ),
            EmbeddingRunError::DispatchPossible(error) => {
                classify_local_embedding_failure(error, embedding_is_deterministic(execution))
            }
        })
}

fn classify_local_embedding_failure(
    error: AdapterError,
    in_process: bool,
) -> LocalEmbeddingRunFailure {
    if matches!(
        &error,
        AdapterError::RuntimeTrust(_)
            | AdapterError::RuntimeConfiguration { .. }
            | AdapterError::LocalPeerConfig { .. }
    ) {
        return LocalEmbeddingRunFailure::Admission(crate::object_persistence::adapter_to_kio(
            error,
        ));
    }
    if in_process && matches!(&error, AdapterError::Io { .. }) {
        LocalEmbeddingRunFailure::InProcessIo(error)
    } else {
        LocalEmbeddingRunFailure::Classified(task_failure_from_adapter(error))
    }
}

pub(crate) fn local_runtime_error(error: AdapterError) -> kio_core::KioError {
    kio_core::KioError::new(
        "KIO-E-EMBED-RUNTIME-001",
        "local embedding execution failed",
        serde_json::json!({"detail":error.to_string()}),
        kio_core::ExitCode::PermanentFailure,
    )
}

#[cfg(test)]
mod local_failure_tests {
    use super::*;
    #[test]
    fn selected_runtime_configuration_is_unsent_admission_with_its_original_code() {
        let source = kio_core::KioError::new(
            kio_adapter::tool_lock::TOOLS_PRIVATE_CREDENTIAL_ERROR,
            "fixture credential refused",
            serde_json::json!({"path":"private-credential-fixture"}),
            kio_core::ExitCode::InvalidUsage,
        );
        let failure = classify_local_embedding_failure(
            AdapterError::RuntimeConfiguration {
                role: "embedding".to_owned(),
                error: std::sync::Arc::new(source),
            },
            false,
        );
        let LocalEmbeddingRunFailure::Admission(error) = failure else {
            panic!("selected configuration failure must not spend a dispatched attempt");
        };
        assert_eq!(
            error.error_code(),
            kio_adapter::tool_lock::TOOLS_PRIVATE_CREDENTIAL_ERROR
        );
        assert_eq!(error.context()["role"], "embedding");
    }

    #[test]
    fn in_process_io_retains_runtime_failure_while_loopback_uses_network_policy() {
        let io = || AdapterError::Io {
            path: "owned-local-model".into(),
            message: "fixture IO failure".into(),
        };
        assert!(matches!(
            classify_local_embedding_failure(io(), true),
            LocalEmbeddingRunFailure::InProcessIo(_)
        ));
        let LocalEmbeddingRunFailure::InProcessIo(error) =
            classify_local_embedding_failure(io(), true)
        else {
            unreachable!()
        };
        assert_eq!(
            local_runtime_error(error).error_code(),
            "KIO-E-EMBED-RUNTIME-001"
        );
        assert!(matches!(
            classify_local_embedding_failure(io(), false),
            LocalEmbeddingRunFailure::Classified(TaskExecutionFailure {
                retry_kind: RetryErrorKind::NetworkError,
                ..
            })
        ));
    }
}
