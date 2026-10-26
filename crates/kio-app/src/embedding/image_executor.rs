//! Image embedding scheduling, durable ownership and source publication.

use crate::egress::admit_embedding_transport;
use crate::egress::embedding_secrets_allowed;
use crate::embedding::admission::EMBEDDING_ADAPTER_KIND;
use crate::embedding::admission::effective_invocation_lane;
use crate::embedding::admission::embedding_billed_from_usage;
use crate::embedding::admission::embedding_is_online;
use crate::embedding::executor::{
    EmbeddingExecutionContext, apply_embedding_transitions, count_embedding_failure,
    embedding_failure_transition, fail_local_embedding_claim, mirror_local_embedding_failure,
    mirror_local_embedding_start, mirror_local_embedding_wait, mirror_local_interruption,
    mirror_local_unsent, publish_image_embedding_object,
};
use crate::embedding::input::referenced_authorized_image_hashes_for_chunks;
use crate::embedding::plan::EmbeddableChunk;
use crate::embedding::transport::{EmbeddingRunFailure, run_embedding_adapter};
use crate::historical_reindex::RetainedNormalizedInstance;
use crate::image_authority;
use crate::index::chunk_ledger::rotate_index_generation_unconditionally;
use crate::invocation::Invocation;
use crate::ledger_access::budget_cap_config;
use crate::ledger_access::settle_task_charge_success;
use crate::ledger_access::settle_task_charge_unknown;
use crate::ledger_access::sync_result_unknown_fenced;
use crate::ledger_access::task_ledger_key;
use crate::markdownize::execution::TaskExecutionFailure;
use crate::object_persistence::adapter_to_kio;
use crate::object_persistence::index_to_kio;
use crate::object_persistence::pipeline_to_kio;
use crate::search::projection::mark_replica_unavailable_or_log;
use crate::search::projection::write_through_projection_or_log;
use crate::settings::user_config_toml_path;
use crate::task_execution::ExecOutcome;
use crate::task_execution::retry_reason;
use crate::task_execution::task_retry_allowed;
use crate::task_execution::task_retry_due;
use kio_adapter::catalog::embedding_descriptor_with_control;
use kio_adapter::traits::PreferredRequestKind;
use kio_adapter::types::EmbeddingContent;
use kio_adapter::types::EmbeddingInputType;
use kio_adapter::types::EmbeddingItem;
use kio_adapter::types::EmbeddingLogicalJobId;
use kio_adapter::types::EmbeddingMemberResult;
use kio_adapter::types::EmbeddingUsage;
use kio_core::KioError;
use kio_core::Result;
use kio_core::scope::new_ulid;
#[cfg(debug_assertions)]
use kio_core::test_control::EmbeddingPublicationFault;
use kio_index::EmbeddingDistance;
use kio_index::EmbeddingModality;
use kio_index::EmbeddingTargetType;
use kio_index::embedding_store;
use kio_pipeline::budget::read_budget_policy;
use kio_pipeline::ledger::BatchState;
use kio_pipeline::ledger::LedgerDb;
use kio_pipeline::ledger::Outcome;
use kio_pipeline::ledger::RequestKind;
use kio_pipeline::ledger::embedding_job::EmbeddingJob;
use kio_pipeline::ledger::embedding_job::EmbeddingJobBinding;
use kio_pipeline::ledger::embedding_job::EmbeddingJobMember;
use kio_pipeline::ledger::embedding_job::EmbeddingReservationOutcome;
use kio_pipeline::ledger::embedding_job::EmbeddingReserveRequest;
use kio_pipeline::ledger::ops::BilledAmount;
use kio_pipeline::ledger::ops::TerminalWrite;
use kio_pipeline::ledger::ops::cost_ledger_rows_for_key;
use kio_pipeline::ledger::ops::embedding_job;
use kio_pipeline::ledger::ops::finish_embedding_publication;
use kio_pipeline::ledger::ops::get_batch_request;
use kio_pipeline::ledger::ops::reserve_embedding_job;
use kio_pipeline::ledger::ops::stage_embedding_result;
use kio_pipeline::ledger::ops::terminal_transaction;
use kio_pipeline::task::EmbeddingTaskTarget;
use kio_pipeline::task::HoldReason;
use kio_pipeline::task::RetryErrorKind;
use kio_pipeline::task::TaskDescriptor;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use kio_pipeline::task::task_can_complete_from_materialized_output;
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Embed typed-owned image objects cited by live chunks into `image_vec`.
pub(crate) struct ImageEnrichmentRequest<'a> {
    pub(crate) retained_instances: &'a [RetainedNormalizedInstance],
    pub(crate) eligible_chunks: &'a [EmbeddableChunk],
    pub(crate) task_store: &'a TaskStore,
    pub(crate) ledger: Option<&'a LedgerDb>,
    pub(crate) secrets_approved: bool,
    pub(crate) execution_allowed: bool,
    pub(crate) override_budget: bool,
}

pub(crate) fn run_image_embedding_enrichment(
    invocation: Invocation<'_>,
    embedding: &EmbeddingExecutionContext<'_>,
    conn: &Connection,
    request: ImageEnrichmentRequest<'_>,
) -> Result<ExecOutcome> {
    let ImageEnrichmentRequest {
        retained_instances,
        eligible_chunks,
        task_store,
        ledger,
        secrets_approved,
        execution_allowed,
        override_budget,
    } = request;
    let ctx = &invocation;
    let adapter_settings = invocation.settings;

    let descriptor = embedding_descriptor_with_control(
        adapter_settings,
        embedding.execution,
        invocation.control,
    )
    .map_err(adapter_to_kio)?;
    let free_execution = !embedding.execution.is_online() && !descriptor.effects.paid_authorization;
    let external_images_allowed = embedding.repo.image_external_send_allowed()?;
    let exclusion = if descriptor.effects.egress && !external_images_allowed {
        Some("images_excluded_from_external_send")
    } else if descriptor.capability.image_mime_types.is_empty() {
        Some("image_capability_unavailable")
    } else {
        None
    };
    let scope_id = embedding.repo.scope_identity()?.scope_id;
    let image_authority = image_authority::retained_image_authority(
        embedding.repo,
        retained_instances,
        embedding.current_policy,
    )?;
    let referenced = referenced_authorized_image_hashes_for_chunks(
        eligible_chunks,
        &scope_id,
        &image_authority,
        secrets_approved,
    );
    if referenced.is_empty() {
        return Ok(ExecOutcome::default());
    }
    let mut already = embedding_store::embedded_image_inputs(conn, &embedding.profile.profile_hash)
        .map_err(index_to_kio)?;
    // The embedding CAS is immutable truth; the source image row and device
    // replica are projections. Restore a missing projection from the exact
    // current image/profile object without reserving or sending again.
    let store = embedding.repo.object_store();
    // Retain only small typed identities; source bytes are prepared one at a time.
    let mut plans = BTreeMap::new();
    for image_hash in &referenced {
        match crate::embedding::input::prepare_owned_embedding_image(
            &store,
            image_hash,
            embedding.profile,
        ) {
            Ok((plan, _)) => {
                plans.insert(image_hash.clone(), plan);
            }
            Err(error) => return Err(error),
        }
    }
    // A codec change retires only unsent tasks for the replaced input. A
    // durable accepted/known job still owns its original binding and must be
    // collected and settled even when that input is no longer current.
    let mut retired_task_ids = BTreeSet::new();
    for task in task_store.all().map_err(pipeline_to_kio)? {
        if !matches!(
            task.status,
            TaskStatus::Pending | TaskStatus::Paused | TaskStatus::Failed | TaskStatus::Skipped
        ) || task.reservation_id.is_some()
            || task.reserved_usd.is_some()
            || task.reserved_month.is_some()
        {
            continue;
        }
        if task.status == TaskStatus::Skipped
            && task.fallback_reason.as_deref() == Some("image_input_changed")
        {
            continue;
        }
        let Some(EmbeddingTaskTarget::Image {
            image_hash,
            embedding_hash,
            profile_hash,
            ..
        }) = task.embedding_target.as_ref()
        else {
            continue;
        };
        let Some(plan) = plans.get(image_hash) else {
            continue;
        };
        if profile_hash != &embedding.profile.profile_hash
            || task.embedding_target.as_ref() == Some(&plan.target)
        {
            continue;
        }
        if let Some(ledger) = ledger {
            let key = task_ledger_key(
                &scope_id,
                EMBEDDING_ADAPTER_KIND,
                embedding_hash,
                profile_hash,
            );
            if let Some(row) = get_batch_request(ledger, &key).map_err(pipeline_to_kio)?
                && (row.state.is_inflight()
                    || embedding_job(ledger, &row)
                        .map_err(pipeline_to_kio)?
                        .is_some_and(|job| job.known_result.is_some()))
            {
                continue;
            }
        }
        retired_task_ids.insert(task.task_id);
    }
    if !retired_task_ids.is_empty() {
        task_store
            .update_matching(|task| {
                if !retired_task_ids.contains(&task.task_id) {
                    return false;
                }
                task.status = TaskStatus::Skipped;
                task.fallback_reason = Some("image_input_changed".to_owned());
                task.hold_reason = None;
                task.next_retry_at = None;
                task.deadline = None;
                task.heartbeat_at = None;
                true
            })
            .map_err(pipeline_to_kio)?;
    }
    let mut restored = 0_usize;
    let restoration = (|| -> Result<()> {
        for image_hash in &referenced {
            let Some(plan) = plans.get(image_hash) else {
                continue;
            };
            if already
                .get(image_hash)
                .is_some_and(|input| input.as_deref() == plan.context_key())
            {
                continue;
            }
            let embedding_hash = embedding_store::embedding_hash(
                EmbeddingTargetType::Image,
                image_hash,
                embedding.profile.dimensions,
                EmbeddingDistance::Cosine,
                EmbeddingModality::Multimodal,
                &embedding.profile.profile_hash,
                plan.context.as_ref(),
            )
            .map_err(index_to_kio)?;
            let object = match store.read_embedding(&embedding_hash) {
                Ok(object) => object,
                Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => continue,
                Err(error) => return Err(error),
            };
            if object.metadata.image_input_receipt.as_ref() != plan.receipt()
                || object.context != plan.context
            {
                return Err(KioError::schema("image embedding input identity mismatch"));
            }
            if restored == 0 {
                mark_replica_unavailable_or_log(ctx, embedding.repo.kio_dir());
            }
            embedding_store::write_image_embedding(
                conn,
                &embedding_hash,
                image_hash,
                &embedding_store::f32_to_le_bytes(&object.vector),
                object.dimensions,
                &object.distance,
                &object.modality,
                &object.profile_hash,
                plan.context_key(),
            )
            .map_err(index_to_kio)?;
            already.insert(image_hash.clone(), plan.context_key().map(str::to_owned));
            restored += 1;
        }
        Ok(())
    })();
    if restored > 0 {
        rotate_index_generation_unconditionally(
            embedding.repo.kio_dir(),
            embedding.repo.control(),
        )?;
        write_through_projection_or_log(ctx, embedding.repo.kio_dir());
    }
    restoration?;
    let now = invocation.control.now_utc_seconds();
    let existing_tasks = task_store.all().map_err(pipeline_to_kio)?;
    for image_hash in &referenced {
        let Some(plan) = plans.get(image_hash) else {
            continue;
        };
        let target = plan.target.clone();
        let materialized = already
            .get(image_hash)
            .is_some_and(|input| input.as_deref() == plan.context_key());
        let output_ref = target.output_ref();
        if let Some(existing) = existing_tasks
            .iter()
            .find(|task| task.output_ref == output_ref)
        {
            if exclusion.is_none()
                && materialized
                && materialized_image_task_can_complete(existing, &target, &scope_id, ledger)?
            {
                task_store
                    .update_matching(|task| {
                        if task.task_id != existing.task_id {
                            return false;
                        }
                        task.clear_reservation();
                        task.status = TaskStatus::Done;
                        task.fallback_reason = Some("embedding_adapter_done".to_owned());
                        task.next_retry_at = None;
                        task.heartbeat_at = None;
                        true
                    })
                    .map_err(pipeline_to_kio)?;
                continue;
            }
            let admitted_pending = if !execution_allowed {
                (TaskStatus::Pending, Some("network_opt_in_required"))
            } else if !free_execution && ledger.is_none() {
                (TaskStatus::Paused, Some("ledger_initialization_required"))
            } else {
                (TaskStatus::Pending, None)
            };
            let next = if (existing.status == TaskStatus::Skipped && exclusion.is_none())
                || (existing.status == TaskStatus::Paused
                    && existing.fallback_reason.as_deref()
                        == Some("ledger_initialization_required")
                    && (free_execution || ledger.is_some() || !execution_allowed))
                || (existing.status == TaskStatus::Pending
                    && existing.fallback_reason.as_deref() == Some("network_opt_in_required")
                    && execution_allowed)
            {
                Some(admitted_pending)
            } else if existing.status == TaskStatus::Paused
                && existing.hold_reason == Some(HoldReason::Budget)
                && execution_allowed
                && (free_execution || ledger.is_some())
                && exclusion.is_none()
                && override_budget
            {
                Some((TaskStatus::Pending, None))
            } else if (existing.status != TaskStatus::Done
                && existing.status != TaskStatus::Skipped
                && exclusion.is_some())
                || (existing.status == TaskStatus::Skipped
                    && existing.fallback_reason.as_deref() != exclusion)
            {
                Some((TaskStatus::Skipped, exclusion))
            } else {
                None
            };
            if let Some((status, reason)) = next {
                task_store
                    .update_matching(|task| {
                        if task.output_ref != output_ref {
                            return false;
                        }
                        task.status = status;
                        task.fallback_reason = reason.map(str::to_owned);
                        task.hold_reason = (status == TaskStatus::Paused)
                            .then_some(HoldReason::LedgerInitializationRequired);
                        task.next_retry_at = None;
                        task.heartbeat_at = None;
                        true
                    })
                    .map_err(pipeline_to_kio)?;
            }
            continue;
        }
        let input_path = eligible_chunks
            .iter()
            .find(|chunk| chunk.text.contains(image_hash))
            .map(|chunk| chunk.raw_path.clone())
            .ok_or_else(|| KioError::schema("owned image has no citing chunk path"))?;
        let status = if exclusion.is_some() {
            TaskStatus::Skipped
        } else if materialized {
            TaskStatus::Done
        } else if !execution_allowed {
            TaskStatus::Pending
        } else if !free_execution && ledger.is_none() {
            TaskStatus::Paused
        } else {
            TaskStatus::Pending
        };
        let fallback_reason = exclusion
            .or_else(|| (!execution_allowed).then_some("network_opt_in_required"))
            .or_else(|| {
                (!free_execution && ledger.is_none()).then_some("ledger_initialization_required")
            });
        task_store
            .append(&TaskDescriptor {
                task_id: format!("task_{}", new_ulid(embedding.repo.canonical_root())),
                task_type: TaskType::Embedding,
                embedding_target: Some(target.clone()),
                mode: None,
                input_path,
                input_hash: target.input_hash().to_owned(),
                previous_raw_hash: None,
                parent_run_id: None,
                changed_unit_keys: vec![image_hash.clone()],
                output_ref,
                unit_keys: None,
                status,
                attempts: 0,
                next_retry_at: None,
                deadline: None,
                heartbeat_at: None,
                fallback_reason: fallback_reason.map(str::to_owned),
                created_at: now.clone(),
                bbox_annotation_enabled: None,
                hold_reason: (status == TaskStatus::Paused)
                    .then_some(HoldReason::LedgerInitializationRequired),
                reserved_usd: None,
                reserved_month: None,
                reservation_id: None,
            })
            .map_err(pipeline_to_kio)?;
    }
    if exclusion.is_some() || !execution_allowed || (!free_execution && ledger.is_none()) {
        return Ok(ExecOutcome::default());
    }
    let pending = referenced
        .into_iter()
        .filter(|hash| {
            plans.get(hash).is_some_and(|plan| {
                !already
                    .get(hash)
                    .is_some_and(|input| input.as_deref() == plan.context_key())
            })
        })
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(ExecOutcome::default());
    }
    let budget_caps = read_budget_policy(
        user_config_toml_path(invocation.control),
        embedding.repo.kio_dir().join("config.toml"),
    )
    .map_err(pipeline_to_kio)?;
    let caps = budget_cap_config(&budget_caps, &scope_id, EMBEDDING_ADAPTER_KIND);
    let mut outcome = ExecOutcome::default();
    let mut replica_marked_unavailable = false;
    let mut published_jobs = Vec::new();
    let mut local_publications = Vec::new();
    let local_store = free_execution
        .then(|| {
            crate::local_execution::LocalExecution::new(
                crate::local_execution::LocalExecution::default_root(invocation.control)?,
                invocation.control.clone(),
            )
        })
        .transpose()?;
    let send_tasks = task_store.all().map_err(pipeline_to_kio)?;
    let result = (|| -> Result<()> {
        let store = embedding.repo.object_store();
        // The local adapter deliberately makes one HTTP request per image.
        // Keep the app-side authorization and bounded CAS read at that same
        // granularity: a policy change cannot admit later images, and a large
        // corpus never accumulates all source bytes in memory.
        for image_hash in &pending {
            let plan = plans.get(image_hash).expect("prepared image plan");
            let image_output_ref = plan.target.output_ref();
            let Some(task) = send_tasks
                .iter()
                .find(|task| task.output_ref == image_output_ref)
            else {
                return Err(KioError::schema("image send has no typed task"));
            };
            if !(free_execution && task.status == TaskStatus::Running)
                && !(task.status == TaskStatus::Pending && task_retry_due(task, invocation.control))
                && !(task.status == TaskStatus::Failed
                    && task_retry_allowed(task)
                    && task_retry_due(task, invocation.control))
            {
                continue;
            }
            // OCR-extracted figures and directly ingested images share the
            // same bounded source validation used for CAS reprojection.
            let (fresh_plan, prepared) = crate::embedding::input::prepare_owned_embedding_image(
                &store,
                image_hash,
                embedding.profile,
            )?;
            if fresh_plan.target != plan.target {
                return Err(KioError::schema("image input changed before send"));
            }

            // Re-read policy, exact network grant, and secret consent before
            // every authenticated local HTTP request. A secret alias is
            // conservative: it requires this embedding tool's paired grant
            // even if a public alias names the same immutable image object.
            admit_embedding_transport(
                adapter_settings,
                embedding.repo,
                embedding.execution,
                embedding.profile,
                embedding.current_policy,
                image_authority.requires_secret_approval(image_hash),
            )?;
            let prepared = if embedding_is_online(embedding.execution)
                && effective_invocation_lane() == PreferredRequestKind::Batch
            {
                match crate::embedding::batch::submit_image_embedding_job(
                    ctx,
                    embedding.repo,
                    ledger.expect("paid image batch requires ledger"),
                    embedding.execution,
                    embedding.profile,
                    plan,
                    prepared,
                    image_authority.requires_secret_approval(image_hash),
                    override_budget,
                )? {
                    crate::embedding::batch::ImageBatchDispatch::Sync(prepared) => prepared,
                    crate::embedding::batch::ImageBatchDispatch::Handled(batch_outcome) => {
                        outcome.add(batch_outcome);
                        continue;
                    }
                }
            } else {
                prepared
            };
            let items = vec![EmbeddingItem {
                id: image_hash.clone(),
                content: EmbeddingContent::Image {
                    bytes: prepared.bytes,
                    mime: prepared.mime.to_owned(),
                },
            }];
            if embedding_is_online(embedding.execution)
                && kio_adapter::catalog::validate_gemini_sync_inputs(
                    &items,
                    kio_adapter::catalog::ADOPTED_EMBEDDING_MODEL_PIN,
                    u32::try_from(embedding.profile.dimensions)
                        .map_err(|_| KioError::schema("embedding dimensions exceed u32"))?,
                )
                .is_err()
            {
                set_image_task_status(task_store, &image_output_ref, false, None)?;
                continue;
            }
            let embedding_hash = plan.embedding_hash().to_owned();
            let key = task_ledger_key(
                &scope_id,
                EMBEDDING_ADAPTER_KIND,
                &embedding_hash,
                &embedding.profile.profile_hash,
            );
            if free_execution {
                if task.reservation_claim().is_some() {
                    outcome.paused += 1;
                    continue;
                }
                let identity = crate::embedding::transport::local_embedding_identity(
                    crate::local_execution::LocalTarget::Image {
                        scope_id: scope_id.clone(),
                        target: embedding_hash.clone(),
                    },
                    embedding_hash.clone(),
                    embedding.profile,
                    embedding.execution,
                    vec![EmbeddingJobMember {
                        id: image_hash.clone(),
                        target: plan.target.clone(),
                    }],
                )?;
                let refs = BTreeSet::from([image_output_ref.clone()]);
                let floor_next = task
                    .next_retry_at
                    .as_deref()
                    .and_then(kio_core::scope::parse_utc_seconds)
                    .map(|seconds| seconds * 1000);
                let mut claim = match local_store.as_ref().expect("free image state").claim(
                    &identity,
                    task.attempts,
                    floor_next,
                    crate::embedding::transport::local_max_attempts(embedding.execution),
                    invocation.control.now_millis(),
                )? {
                    crate::local_execution::Claim::Claimed(claim) => claim,
                    crate::local_execution::Claim::Busy => {
                        outcome.inflight += 1;
                        continue;
                    }
                    crate::local_execution::Claim::Backoff {
                        attempts,
                        next_retry_at,
                        kind,
                    } => {
                        mirror_local_embedding_wait(
                            task_store,
                            &refs,
                            attempts,
                            Some(next_retry_at),
                            kind,
                            false,
                        )?;
                        continue;
                    }
                    crate::local_execution::Claim::Exhausted { attempts, kind } => {
                        mirror_local_embedding_wait(task_store, &refs, attempts, None, kind, true)?;
                        continue;
                    }
                };
                // A known result still requires current policy/grant admission.
                // Its accepted attempt is never refunded or dispatched again.
                let recovered_bytes = claim.result()?;
                if let Err(error) = admit_embedding_transport(
                    adapter_settings,
                    embedding.repo,
                    embedding.execution,
                    embedding.profile,
                    embedding.current_policy,
                    image_authority.requires_secret_approval(image_hash),
                ) {
                    if recovered_bytes.is_none() {
                        claim.cancel_unsent()?;
                        mirror_local_unsent(task_store, &refs, claim.attempts())?;
                    }
                    return Err(error);
                }
                mirror_local_embedding_start(task_store, &refs, claim.attempts(), &now)?;
                let completed = if let Some(bytes) = recovered_bytes {
                    crate::embedding::transport::decode_local_embedding_result(
                        &bytes,
                        embedding.profile,
                        vec![image_hash.clone()],
                    )?
                } else {
                    let result = admit_embedding_transport(
                        adapter_settings,
                        embedding.repo,
                        embedding.execution,
                        embedding.profile,
                        embedding.current_policy,
                        image_authority.requires_secret_approval(image_hash),
                    )
                    .map_err(crate::embedding::transport::LocalEmbeddingRunFailure::Admission)
                    .and_then(|_| {
                        crate::embedding::transport::run_local_embedding_adapter(
                            adapter_settings,
                            embedding.execution,
                            items,
                            EmbeddingInputType::ImageObject,
                            Some(claim.token().to_owned()),
                            invocation.control,
                        )
                    });
                    match result {
                        Ok(completed) => {
                            claim.stage_result(&serde_json::to_vec(&completed).map_err(
                                |_| KioError::schema("cannot serialize local image completion"),
                            )?)?;
                            completed
                        }
                        Err(crate::embedding::transport::LocalEmbeddingRunFailure::Classified(
                            failure,
                        )) => {
                            let next = fail_local_embedding_claim(&mut claim, &failure, &now)?;
                            mirror_local_embedding_failure(
                                task_store,
                                &refs,
                                claim.attempts(),
                                next,
                                failure.retry_kind,
                            )?;
                            count_embedding_failure(&mut outcome, failure.retry_kind, 1);
                            if next.is_some() {
                                outcome.failed_retryable += 1;
                            }
                            continue;
                        }
                        Err(crate::embedding::transport::LocalEmbeddingRunFailure::Admission(
                            error,
                        )) => {
                            claim.cancel_unsent()?;
                            mirror_local_unsent(task_store, &refs, claim.attempts())?;
                            return Err(error);
                        }
                        Err(
                            crate::embedding::transport::LocalEmbeddingRunFailure::InProcessIo(
                                error,
                            ),
                        ) => {
                            claim.interrupt()?;
                            mirror_local_interruption(
                                task_store,
                                &refs,
                                claim.attempts(),
                                claim.attempts() >= 3,
                                None,
                            )?;
                            return Err(crate::embedding::transport::local_runtime_error(error));
                        }
                    }
                };
                let EmbeddingMemberResult::Vector { vector } = &completed.members[0].result else {
                    return Err(KioError::schema("local image result is not a vector"));
                };
                embedding
                    .current_policy
                    .revalidate()
                    .map_err(pipeline_to_kio)?;
                if !replica_marked_unavailable {
                    mark_replica_unavailable_or_log(ctx, embedding.repo.kio_dir());
                    replica_marked_unavailable = true;
                }
                #[cfg(debug_assertions)]
                if invocation
                    .control
                    .debug()
                    .cli
                    .embedding_publication_fault
                    .known()
                    == Some(&EmbeddingPublicationFault::BeforeSourceWrite)
                {
                    return Err(KioError::schema(
                        "local image publication interrupted before source write",
                    ));
                }
                publish_image_embedding_object(
                    embedding.repo,
                    &embedding_hash,
                    image_hash,
                    vector,
                    embedding.profile,
                    embedding.execution,
                    plan,
                )?;
                embedding_store::write_image_embedding(
                    conn,
                    &embedding_hash,
                    image_hash,
                    &embedding_store::f32_to_le_bytes(vector),
                    embedding.profile.dimensions,
                    &embedding.profile.distance,
                    &embedding.profile.modality,
                    &embedding.profile.profile_hash,
                    plan.context_key(),
                )
                .map_err(index_to_kio)?;
                // The local owner already consumed this logical attempt at
                // dispatch. Completion mirrors its exact count without the
                // paid runner's additional completion increment.
                task_store
                    .update_matching(|task| {
                        if task.task_type != TaskType::Embedding
                            || task.output_ref != image_output_ref
                            || task.reservation_claim().is_some()
                        {
                            return false;
                        }
                        task.status = TaskStatus::Done;
                        task.attempts = claim.attempts();
                        task.next_retry_at = None;
                        task.heartbeat_at = None;
                        task.hold_reason = None;
                        task.fallback_reason = None;
                        task.deadline = None;
                        true
                    })
                    .map_err(pipeline_to_kio)?;
                local_publications.push(claim);
                outcome.executed += 1;
                continue;
            }
            let ledger = ledger.expect("paid image execution requires initialized ledger");
            let binding = EmbeddingJobBinding {
                logical_job_id: EmbeddingLogicalJobId(embedding_hash.clone()),
                dimensions: u32::try_from(embedding.profile.dimensions)
                    .map_err(|_| KioError::schema("embedding dimensions exceed u32"))?,
                distance: embedding.profile.distance.clone(),
                modality: embedding.profile.modality.clone(),
                members: vec![EmbeddingJobMember {
                    id: image_hash.clone(),
                    target: plan.target.clone(),
                }],
            };

            if let Some(row) = get_batch_request(ledger, &key).map_err(pipeline_to_kio)?
                && embedding_is_online(embedding.execution)
                && sync_result_unknown_fenced(Some(&row))
            {
                cost_ledger_rows_for_key(ledger, &key)
                    .map_err(pipeline_to_kio)?
                    .into_iter()
                    .find(|cost| {
                        cost.submission_seq == row.submission_seq
                            && cost.outcome == Outcome::UnknownSettled
                            && kio_pipeline::ledger::ops::is_uuid_v7(&cost.batch_job_id)
                    })
                    .ok_or_else(|| KioError::schema("fenced image lost its terminal attempt"))?;
                apply_image_embedding_failure(
                    task_store,
                    &image_output_ref,
                    &TaskExecutionFailure {
                        retry_kind: RetryErrorKind::ResultUnknown,
                        retry_after_ms: None,
                    },
                    &now,
                    &mut outcome,
                )?;
                continue;
            }
            let job = match reserve_embedding_job(
                ledger,
                &EmbeddingReserveRequest {
                    key: &key,
                    candidate_usd: 8192.0
                        * crate::embedding::admission::embedding_image_usd_per_token(
                            adapter_settings,
                            PreferredRequestKind::Sync,
                        ),
                    caps: &caps,
                    request_kind: RequestKind::Sync,
                    timeout_seconds: Some(
                        crate::ledger_access::TASK_SYNC_EFFECTIVE_TIMEOUT_SECONDS,
                    ),
                    bypass_cap_denial: override_budget || !budget_caps.hard_stop,
                    binding: &binding,
                },
            )
            .map_err(pipeline_to_kio)?
            {
                EmbeddingReservationOutcome::Reserved(job) => {
                    invocation
                        .control
                        .checkpoint(kio_core::durability::DurabilityPoint::EmbeddingReserved)?;
                    job
                }
                EmbeddingReservationOutcome::KnownResult(job) => job,
                EmbeddingReservationOutcome::InFlight(job) => {
                    let row = get_batch_request(ledger, &key)
                        .map_err(pipeline_to_kio)?
                        .ok_or_else(|| KioError::schema("image attempt lost reservation"))?;
                    if row
                        .stale_after_at
                        .is_none_or(|deadline| deadline > invocation.control.now_millis())
                    {
                        outcome.inflight += 1;
                        continue;
                    }
                    settle_task_charge_unknown(ledger, &key, &job.intent_token, row.estimated_usd)?;
                    set_image_task_status(task_store, &image_output_ref, false, None)?;
                    continue;
                }
                EmbeddingReservationOutcome::BudgetExceeded => {
                    task_store
                        .update_matching(|task| {
                            if task.output_ref != image_output_ref {
                                return false;
                            }
                            task.status = TaskStatus::Paused;
                            task.hold_reason = Some(HoldReason::Budget);
                            task.fallback_reason = Some("budget_exceeded".to_owned());
                            true
                        })
                        .map_err(pipeline_to_kio)?;
                    outcome.paused += 1;
                    continue;
                }
            };
            let staged = if job.known_result.is_some() {
                job
            } else {
                // A reservation is not permission to send after consent or
                // policy changes. A proven-unsent request closes at zero.
                let external_excluded = if descriptor.effects.egress {
                    match embedding.repo.image_external_send_allowed() {
                        Ok(allowed) => !allowed,
                        Err(error) => {
                            close_unsent_image_attempt(
                                ledger,
                                &job,
                                task_store,
                                TaskStatus::Skipped,
                                "image_policy_changed",
                            )?;
                            return Err(error);
                        }
                    }
                } else {
                    false
                };
                if external_excluded {
                    close_unsent_image_attempt(
                        ledger,
                        &job,
                        task_store,
                        TaskStatus::Skipped,
                        "images_excluded_from_external_send",
                    )?;
                    continue;
                }
                if let Err(error) = embedding.current_policy.revalidate() {
                    close_unsent_image_attempt(
                        ledger,
                        &job,
                        task_store,
                        TaskStatus::Skipped,
                        "image_policy_changed",
                    )?;
                    return Err(pipeline_to_kio(error));
                }
                if let Err(error) = admit_embedding_transport(
                    adapter_settings,
                    embedding.repo,
                    embedding.execution,
                    embedding.profile,
                    embedding.current_policy,
                    image_authority.requires_secret_approval(image_hash),
                ) {
                    let reason = if image_authority.requires_secret_approval(image_hash)
                        && matches!(
                            embedding_secrets_allowed(
                                adapter_settings,
                                embedding.repo,
                                embedding.profile,
                            ),
                            Ok(false)
                        ) {
                        "secrets_tier_b_hold"
                    } else {
                        "network_opt_in_required"
                    };
                    close_unsent_image_attempt(
                        ledger,
                        &job,
                        task_store,
                        TaskStatus::Pending,
                        reason,
                    )?;
                    return Err(error);
                }
                let result = run_embedding_adapter(
                    adapter_settings,
                    embedding.execution,
                    items,
                    EmbeddingInputType::ImageObject,
                    Some(job.intent_token.clone()),
                    invocation.control,
                );
                let outcome = match result {
                    Ok(outcome) => outcome,
                    Err(EmbeddingRunFailure::BeforeDispatch(error)) => {
                        kio_pipeline::ledger::embedding_job::reject_unsent_embedding_job(
                            ledger, &job,
                        )
                        .map_err(pipeline_to_kio)?;
                        crate::embedding::executor::release_unsent_embedding_tasks(
                            task_store, &job,
                        )?;
                        return Err(error);
                    }
                    Err(EmbeddingRunFailure::Failure(mut failure)) => {
                        let row = get_batch_request(ledger, &key)
                            .map_err(pipeline_to_kio)?
                            .ok_or_else(|| KioError::schema("image send lost reservation"))?;
                        let known_rejection = matches!(
                            failure.retry_kind,
                            RetryErrorKind::AuthError
                                | RetryErrorKind::RateLimit
                                | RetryErrorKind::QuotaExceeded
                        );
                        if known_rejection {
                            terminal_transaction(
                                ledger,
                                &TerminalWrite {
                                    key: &key,
                                    outcome: Outcome::SubmitRejected,
                                    billed: BilledAmount {
                                        usd: 0.0,
                                        estimated: false,
                                    },
                                    ledger_batch_job_id: &job.intent_token,
                                    next_state: BatchState::Terminal,
                                    error: Some(retry_reason(failure.retry_kind)),
                                    increment_contract_violation: false,
                                    attempts_delta: 1,
                                    clear_intent_token: true,
                                    intent_token_guard: Some(&job.intent_token),
                                    reseat_submission_seq: false,
                                },
                            )
                            .map_err(pipeline_to_kio)?;
                        } else {
                            settle_task_charge_unknown(
                                ledger,
                                &key,
                                &job.intent_token,
                                row.estimated_usd,
                            )?;
                            failure.retry_kind = RetryErrorKind::ResultUnknown;
                            failure.retry_after_ms = None;
                        }
                        cost_ledger_rows_for_key(ledger, &key)
                            .map_err(pipeline_to_kio)?
                            .into_iter()
                            .find(|cost| {
                                cost.batch_job_id == job.intent_token
                                    && cost.outcome
                                        == if known_rejection {
                                            Outcome::SubmitRejected
                                        } else {
                                            Outcome::UnknownSettled
                                        }
                                    && (cost.submission_seq == job.submission_seq
                                        || (job.submission_seq.checked_add(1)
                                            == Some(cost.submission_seq)
                                            && cost.outcome == Outcome::UnknownSettled))
                            })
                            .ok_or_else(|| {
                                KioError::schema("failed image lost its terminal attempt")
                            })?;
                        apply_image_embedding_failure(
                            task_store,
                            &image_output_ref,
                            &failure,
                            &now,
                            &mut outcome,
                        )?;
                        continue;
                    }
                };
                if outcome.members.len() != 1
                    || outcome.members[0].id != *image_hash
                    || !matches!(
                        outcome.members[0].result,
                        EmbeddingMemberResult::Vector { .. }
                    )
                {
                    let row = get_batch_request(ledger, &key)
                        .map_err(pipeline_to_kio)?
                        .ok_or_else(|| KioError::schema("image send lost reservation"))?;
                    settle_task_charge_unknown(ledger, &key, &job.intent_token, row.estimated_usd)?;

                    set_image_task_status(task_store, &image_output_ref, false, None)?;
                    continue;
                }
                stage_embedding_result(ledger, &job, &outcome).map_err(pipeline_to_kio)?;
                let row = get_batch_request(ledger, &key)
                    .map_err(pipeline_to_kio)?
                    .ok_or_else(|| KioError::schema("staged image lost ledger row"))?;
                embedding_job(ledger, &row)
                    .map_err(pipeline_to_kio)?
                    .ok_or_else(|| KioError::schema("staged image lost binding"))?
            };
            let result = staged
                .known_result
                .as_ref()
                .ok_or_else(|| KioError::schema("staged image lost result"))?;
            let EmbeddingMemberResult::Vector { vector } = &result.members[0].result else {
                return Err(KioError::schema("staged image is not a vector"));
            };
            if get_batch_request(ledger, &key)
                .map_err(pipeline_to_kio)?
                .is_some_and(|row| row.state.is_inflight())
            {
                settle_task_charge_success(ledger, &key, &staged.intent_token, {
                    let row = get_batch_request(ledger, &key)
                        .map_err(pipeline_to_kio)?
                        .ok_or_else(|| KioError::schema("image result lost reservation"))?;
                    embedding_billed_from_usage(
                        adapter_settings,
                        match &result.usage {
                            EmbeddingUsage::Metered { usage } => Some(usage),
                            _ => None,
                        },
                        PreferredRequestKind::Sync,
                        row.estimated_usd,
                        invocation.control,
                        invocation.locks,
                        EmbeddingInputType::ImageObject,
                    )
                })?;
            }
            let terminal = cost_ledger_rows_for_key(ledger, &key)
                .map_err(pipeline_to_kio)?
                .into_iter()
                .find(|row| {
                    row.submission_seq == staged.submission_seq
                        && row.batch_job_id == staged.intent_token
                        && row.outcome == Outcome::Succeeded
                })
                .ok_or_else(|| KioError::schema("successful image lost its terminal attempt"))?;
            // Settlement survives later policy changes; publication rechecks current consent.
            admit_embedding_transport(
                adapter_settings,
                embedding.repo,
                embedding.execution,
                embedding.profile,
                embedding.current_policy,
                image_authority.requires_secret_approval(image_hash),
            )?;
            // CAS first, table second. This keeps a crash recoverable by a
            // later rebuild instead of leaving a SQLite reference to no object.
            if !replica_marked_unavailable {
                // Mark before the first visible source mutation. If a later
                // image or authorization check fails, readers cannot mistake
                // a prior image vector for a complete replica projection.
                mark_replica_unavailable_or_log(ctx, embedding.repo.kio_dir());
                replica_marked_unavailable = true;
            }
            publish_image_embedding_object(
                embedding.repo,
                &embedding_hash,
                image_hash,
                vector,
                embedding.profile,
                embedding.execution,
                plan,
            )?;
            embedding_store::write_image_embedding(
                conn,
                &embedding_hash,
                image_hash,
                &embedding_store::f32_to_le_bytes(vector),
                embedding.profile.dimensions,
                &embedding.profile.distance,
                &embedding.profile.modality,
                &embedding.profile.profile_hash,
                plan.context_key(),
            )
            .map_err(index_to_kio)?;
            set_image_task_status(
                task_store,
                &image_output_ref,
                true,
                Some((
                    get_batch_request(ledger, &key)
                        .map_err(pipeline_to_kio)?
                        .ok_or_else(|| KioError::schema("published image lost reservation claim"))?
                        .estimated_usd,
                    &terminal.month,
                    &staged.intent_token,
                )),
            )?;
            published_jobs.push(staged);
            outcome.executed += 1;
        }
        Ok(())
    })();
    if outcome.executed > 0 {
        // `ScopeDelta` only represents chunk-vector additions. Image vectors
        // also need their image-reference relation, so publish a complete
        // projection after rotating the source generation.
        rotate_index_generation_unconditionally(
            embedding.repo.kio_dir(),
            embedding.repo.control(),
        )?;
        write_through_projection_or_log(ctx, embedding.repo.kio_dir());
    }
    result?;
    for mut claim in local_publications {
        claim.publish()?;
    }
    for job in published_jobs {
        finish_embedding_publication(ledger.expect("paid image publication ledger"), &job)
            .map_err(pipeline_to_kio)?;
    }
    Ok(outcome)
}

/// Close a reservation whose live owner proves it has not dispatched.
fn close_unsent_image_attempt(
    ledger: &LedgerDb,
    job: &EmbeddingJob,
    tasks: &TaskStore,
    status: TaskStatus,
    reason: &str,
) -> Result<()> {
    terminal_transaction(
        ledger,
        &TerminalWrite {
            key: &job.key,
            outcome: Outcome::Abandoned,
            billed: BilledAmount {
                usd: 0.0,
                estimated: false,
            },
            ledger_batch_job_id: &job.intent_token,
            next_state: BatchState::Terminal,
            error: Some(reason),
            increment_contract_violation: false,
            attempts_delta: 0,
            clear_intent_token: true,
            intent_token_guard: Some(&job.intent_token),
            reseat_submission_seq: false,
        },
    )
    .map_err(pipeline_to_kio)?;
    let target = &job.binding.members[0].target;
    tasks
        .update_matching(|task| {
            if task.task_type != TaskType::Embedding
                || task.embedding_target.as_ref() != Some(target)
            {
                return false;
            }
            task.status = status;
            task.fallback_reason = Some(reason.to_owned());
            task.hold_reason = None;
            task.deadline = None;
            task.heartbeat_at = None;
            task.next_retry_at = None;
            // An older retained claim belongs to its original attempt. Do not
            // erase it while demoting a task to a secret/network hold.
            if task
                .reservation_id
                .as_deref()
                .is_none_or(|token| token == job.intent_token)
            {
                task.reservation_id = None;
                task.reserved_usd = None;
                task.reserved_month = None;
            }
            true
        })
        .map_err(pipeline_to_kio)?;
    finish_embedding_publication(ledger, job).map_err(pipeline_to_kio)
}

/// Complete only this exact typed/materialized image. Done tasks keep their
/// successful attempt stamp; other eligible cached states follow the shared
/// paid-task eligibility policy. A retained stamp needs proof of its original
/// terminal attempt and never authorizes a write to another ledger key.
fn materialized_image_task_can_complete(
    task: &TaskDescriptor,
    target: &EmbeddingTaskTarget,
    scope_id: &str,
    ledger: Option<&LedgerDb>,
) -> Result<bool> {
    if task.task_type != TaskType::Embedding
        || !task_can_complete_from_materialized_output(task)
        || task.embedding_target.as_ref() != Some(target)
        || task.input_hash != target.input_hash()
        || task.output_ref != target.output_ref()
    {
        return Ok(false);
    }
    let Some(claim) = task.reservation_claim() else {
        return Ok(task.reservation_id.is_none()
            && task.reserved_usd.is_none()
            && task.reserved_month.is_none());
    };
    let Some(ledger) = ledger else {
        return Ok(false);
    };
    let EmbeddingTaskTarget::Image {
        embedding_hash,
        profile_hash,
        ..
    } = target
    else {
        return Ok(false);
    };
    let key = task_ledger_key(
        scope_id,
        EMBEDDING_ADAPTER_KIND,
        embedding_hash,
        profile_hash,
    );
    let Some(request) = get_batch_request(ledger, &key).map_err(pipeline_to_kio)? else {
        return Ok(false);
    };
    if request.state != BatchState::Completed
        || request.intent_token.is_some()
        || request.estimated_usd != claim.usd
    {
        return Ok(false);
    }
    Ok(cost_ledger_rows_for_key(ledger, &key)
        .map_err(pipeline_to_kio)?
        .iter()
        .any(|row| {
            row.submission_seq == request.submission_seq
                && row.batch_job_id == claim.reservation_id
                && row.month == claim.month
                && row.outcome == Outcome::Succeeded
        }))
}

fn set_image_task_status(
    task_store: &TaskStore,
    output_ref: &str,
    succeeded: bool,
    terminal_claim: Option<(f64, &str, &str)>,
) -> Result<()> {
    task_store
        .update_matching(|task| {
            if task.task_type != TaskType::Embedding || task.output_ref != output_ref {
                return false;
            }
            task.status = if succeeded {
                TaskStatus::Done
            } else {
                TaskStatus::Failed
            };
            task.attempts += 1;
            if let Some((usd, month, token)) = terminal_claim {
                task.reserved_usd = Some(usd);
                task.reserved_month = Some(month.to_owned());
                task.reservation_id = Some(token.to_owned());
            }
            task.next_retry_at = None;
            task.heartbeat_at = None;
            task.hold_reason = None;
            task.fallback_reason = (!succeeded).then(|| "contract_violation".to_owned());
            task.deadline = None;
            true
        })
        .map_err(pipeline_to_kio)?;
    Ok(())
}

fn apply_image_embedding_failure(
    task_store: &TaskStore,
    output_ref: &str,
    failure: &TaskExecutionFailure,
    now: &str,
    outcome: &mut ExecOutcome,
) -> Result<()> {
    let transitions =
        BTreeMap::from([(output_ref.to_owned(), embedding_failure_transition(failure))]);
    // Failure transitions preserve any previous claim, matching text tasks.
    outcome.failed_retryable +=
        apply_embedding_transitions(task_store, &transitions, now, &BTreeMap::new())?;
    count_embedding_failure(outcome, failure.retry_kind, 1);
    Ok(())
}
