use crate::egress::local_markdownize_transport_allowed;
use crate::image_ingest;
use crate::index::command::read_repository_scan_input;
use crate::index::command::repository_input_display_path;
use crate::index::command::repository_scan_policy_allows_file;
use crate::index::pipeline::offline_output_ref;
use crate::ledger_access::SYNC_RESEND_AUTHORIZED_REASON;
use crate::object_persistence::compute_manifest_hash;
use crate::object_persistence::normalized_output_ref;
use crate::object_persistence::pipeline_to_kio;
use crate::object_persistence::write_canonical_cas_object_or_reuse;
use crate::ocr_discovery::prepared_units_from_ocr_discovery;
use crate::ocr_discovery::supports_ocr_from_scratch;
use crate::scope_access::raw_ingest_is_purge_blocked;
use crate::settings::effective_incremental_config;
use crate::settings::effective_max_input_bytes;
use crate::task_execution::estimate_online_markdownize_cost;
use crate::task_execution::is_text_native_media;
use crate::task_execution::media_type_for_cli_path;
use crate::task_execution::retry_kind_from_reason;
use crate::task_execution::retry_reason;
use kio_adapter::catalog::StandardOnlineMarkdownizeRequest;
use kio_adapter::catalog::active_local_ocr_execution_with_control;
use kio_adapter::catalog::builtin_prepare_profile;
use kio_adapter::catalog::effective_prepared_unit_hints;
use kio_adapter::catalog::local_ocr_markdownize_adapter_with_control;
use kio_adapter::catalog::resolve_standard_online_markdownize_profile_with_bbox_with_control;
use kio_adapter::catalog::run_standard_online_markdownize_with_bytes_with_control;
use kio_adapter::local_ocr_markdownize::LocalOcrExecution;
use kio_adapter::office_convert::is_office_media;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::types::MarkdownUnit;
use kio_adapter::types::MarkdownizeMode as AdapterMarkdownizeMode;
use kio_adapter::types::MarkdownizeRequest;
use kio_adapter::types::PreparedUnitHint;
use kio_adapter::types::PreviousMarkdownizeContext;
use kio_adapter::types::RawInput;
use kio_adapter::types::UnitKind;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::dag::NormalizeRef;
use kio_core::scope::PendingNormalizeRef;
use kio_core::scope::Repository;
use kio_core::scope::new_ulid;
use kio_pipeline::markdownize::IncrementalHints;
use kio_pipeline::markdownize::IncrementalModeInput;
use kio_pipeline::markdownize::MarkdownizeMode;
use kio_pipeline::markdownize::NormalizedInstanceManifest;
use kio_pipeline::markdownize::NormalizedUnitManifestEntry;
use kio_pipeline::markdownize::NormalizedUnitObject;
use kio_pipeline::markdownize::UnitStatus;
use kio_pipeline::markdownize::choose_markdownize_mode;
use kio_pipeline::markdownize::load_validated_normalized_instance_with_control;
use kio_pipeline::markdownize::persist_normalized_instance_with_control;
use kio_pipeline::markdownize::validate_markdownize_response;
use kio_pipeline::prepare::PrepareStageBytesRequest;
use kio_pipeline::prepare::PreparedUnit;
use kio_pipeline::prepare::UnitFingerprint;
use kio_pipeline::prepare::UnitType;
use kio_pipeline::prepare::map_units;
use kio_pipeline::prepare::prepare_units_from_bytes_with_control;
use kio_pipeline::prepare::unit_ref;
use kio_pipeline::scan::ScanCandidate;
use kio_pipeline::scan::classify_secret;
use kio_pipeline::task::RetryErrorKind;
use kio_pipeline::task::TaskDescriptor;
use kio_pipeline::task::TaskOutputRef;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use kio_pipeline::task::hold_reason_for_reason;
use kio_pipeline::task::task_status_from_unit_counts;
use kio_pipeline::task::validate_task_output_ref;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(crate) struct OnlineExecutionOutcome {
    pub(crate) output_ref: String,
    pub(crate) status: TaskStatus,
    // R13-1: normalization-run provenance (docs/04:474-479) so the caller records
    // mode / previous_raw_hash / parent_run_id / changed_unit_keys on the task.
    // `Full` with empty/None for a full or unit-scoped-retry send.
    pub(crate) mode: MarkdownizeMode,
    pub(crate) previous_raw_hash: Option<String>,
    pub(crate) parent_run_id: Option<String>,
    pub(crate) changed_unit_keys: Vec<String>,
    /// QA17 (step4b-contract-tests-p3a.md §F): the adapter's self-reported
    /// billing usage for the send that produced this outcome (07 §4),
    /// forwarded from the `MarkdownizeResponse` the adapter returned. `None`
    /// when nothing was sent (a 0-change incremental) or the concrete
    /// adapter has no real per-call signal to report — the settle call site
    /// degrades to the reservation estimate in that case, unchanged from
    /// before this field existed.
    pub(crate) usage: Option<kio_adapter::types::AdapterUsage>,
}

impl OnlineExecutionOutcome {
    /// A `Full`-mode outcome (full send or unit-scoped retry) with no incremental
    /// provenance.
    fn full(output_ref: String, status: TaskStatus) -> Self {
        Self {
            output_ref,
            status,
            mode: MarkdownizeMode::Full,
            previous_raw_hash: None,
            parent_run_id: None,
            changed_unit_keys: Vec::new(),
            usage: None,
        }
    }

    /// QA17: attach the adapter-reported `usage` this send actually billed.
    fn with_usage(mut self, usage: Option<kio_adapter::types::AdapterUsage>) -> Self {
        self.usage = usage;
        self
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TaskExecutionFailure {
    pub(crate) retry_kind: RetryErrorKind,
    // QA3 (step4b-contract-tests-p3a.md §A, 04 §5.3): the provider's
    // `Retry-After` in milliseconds, carried from `AdapterError::RateLimit`
    // through to the send handler so `next_retry_at` can honor it
    // (`rate_limit_retry_at`). `None` for every non-adapter construction
    // site and for an adapter error that is not `RateLimit` with a header.
    pub(crate) retry_after_ms: Option<u64>,
}

/// R11-6: the online-markdownize cost to reserve/bill for `task`. A full send bills
/// the whole document; a unit-scoped retry (`task.unit_keys` = the still-failed
/// units) bills only its share = full × (retried units / total prepared units),
/// with a floor of one unit's worth. `total` comes from the same verified buffer and
/// prepare result that is later sent, so charge and request cardinality cannot race.
pub(crate) fn prorated_markdownize_cost(
    task: &TaskDescriptor,
    file_size: u64,
    prepared_unit_count: usize,
    bbox_annotation_enabled: bool,
) -> f64 {
    let full = estimate_online_markdownize_cost(file_size, bbox_annotation_enabled);
    let Some(unit_keys) = task.unit_keys.as_ref().filter(|keys| !keys.is_empty()) else {
        return full;
    };
    if prepared_unit_count == 0 {
        return full;
    }
    let sent = unit_keys.len().min(prepared_unit_count).max(1);
    full * (sent as f64 / prepared_unit_count as f64)
}

/// R15-2 (+ R14-2 / R9-2): the NETWORK-FREE preconditions an online markdownize task
/// must satisfy before it is charged/executed. Returns `false` when the task can never
/// reach the adapter: the current file no longer hashes to `input_hash` (edited after
/// enqueue → superseded by R14-2 inside the executor), the file is text-native (R9-2
/// rejects it), or its input can't be prepared into any unit. In every case the adapter
/// is never called, so reserving the cost first (F8 charges before the send) would be a
/// phantom charge against a send that never happens — it double-bills and can exhaust the
/// markdownize cap, falsely pausing the valid task (R15-2). The executor re-checks the
/// same conditions as defense in depth; this is the loop's pre-charge gate.
/// R21-3: outcome of the send-time online-markdownize precondition check.
pub(crate) enum OnlineMarkdownizePrecondition {
    /// File present, unchanged, within the input cap, non-text-native, with locally
    /// extracted units to enhance — ready to send.
    Send(PreparedOnlineMarkdownizeInput),
    /// Genuine failure — retire the task (file gone / edited-since-enqueue / text-native /
    /// oversize). The recovery is a fresh re-index, not a retry.
    Retire,
    /// The file is live and recognized, but Kio does not yet have a bounded local
    /// conversion contract that can feed it to the OCR adapter. Keep it Pending without
    /// charging or sending; currently this is the Office-container path.
    AwaitConversion,
    /// 07 §5.1: an Office (DOCX/PPTX) file's runtime conversion (re-)attempt
    /// failed while re-preparing this task's precondition -- never reached
    /// the adapter, never billed. Distinct from `Retire`: this "joins"
    /// contract_violation semantics (04 §5.3, retryable once) rather than a
    /// permanent retirement, since a renderer hiccup is not the same kind of
    /// failure as a genuinely stale/edited-since-enqueue file.
    ConversionFailed,
}

pub(crate) struct PreparedOnlineMarkdownizeInput {
    path: PathBuf,
    pub(crate) media_type: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) prepared_units: Vec<PreparedUnit>,
}

/// R22-5: whether a locally-preparable input is really a TEXT file that only landed in
/// `application/octet-stream` because its extension is absent from the MIME table
/// (`.yaml`, `.json`, `.toml`, `.sh`, `Dockerfile`, …). `prepare_units` sniffs the bytes and
/// emits exactly one `File` unit for such input (`prepare.rs:90`), whereas a true binary
/// yields no units at all. R21-4 stopped *enqueueing* these for online OCR, but the send
/// gates still classified them by canonical MIME only — so a task an older build had already
/// enqueued survived the upgrade and `batch resume` shipped the raw bytes to the OCR API and
/// billed for it. This is the send-side mirror of R21-4's enqueue guard.
fn is_local_passthrough_text(media_type: &str, prepared_units: &[PreparedUnit]) -> bool {
    media_type == "application/octet-stream"
        && prepared_units.len() == 1
        && prepared_units[0].unit_type == UnitType::File
}

fn retry_unit_subset_is_valid(task: &TaskDescriptor, prepared_units: &[PreparedUnit]) -> bool {
    let Some(unit_keys) = &task.unit_keys else {
        return true;
    };
    if unit_keys.is_empty() {
        return false;
    }
    let requested = unit_keys
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if requested.len() != unit_keys.len() {
        return false;
    }
    let prepared = prepared_units
        .iter()
        .map(|unit| unit.unit_key.as_str())
        .collect::<BTreeSet<_>>();
    requested.is_subset(&prepared)
}

pub(crate) fn classify_online_markdownize_precondition(
    repo: &Repository,
    task: &TaskDescriptor,
) -> OnlineMarkdownizePrecondition {
    if raw_ingest_is_purge_blocked(repo, &task.input_hash) {
        return OnlineMarkdownizePrecondition::Retire;
    }
    let path = repository_input_display_path(repo, &task.input_path);
    let media_type = media_type_for_cli_path(&path).to_owned();
    if !repository_scan_policy_allows_file(repo, &task.input_path).unwrap_or(false) {
        return OnlineMarkdownizePrecondition::Retire;
    }
    let Ok(verified) =
        read_repository_scan_input(repo, &task.input_path, effective_max_input_bytes(repo))
    else {
        return OnlineMarkdownizePrecondition::Retire;
    };
    if verified.raw_hash != task.input_hash {
        return OnlineMarkdownizePrecondition::Retire;
    }
    // R19-8: honor a `max_input_bytes` cap that was tightened AFTER this task was
    // enqueued. The enqueue-time gate (run_index_pipeline) only checked the cap in effect
    // then; a Pending task must not be sent to the online adapter if the operator has
    // since lowered the cap below the file's size — the same live re-check the sibling
    // `[adapter.policy]` key `allow_network` already gets at send time.
    if is_text_native_media(&media_type) {
        return OnlineMarkdownizePrecondition::Retire;
    }
    let prepare_profile_hash = builtin_prepare_profile().tool_profile_hash;
    let input_path = path.display().to_string();
    let prepare = match prepare_units_from_bytes_with_control(
        PrepareStageBytesRequest {
            raw_hash: &task.input_hash,
            media_type: &media_type,
            input_path: &input_path,
            tool_profile_hash: &prepare_profile_hash,
            bytes: &verified.bytes,
        },
        repo.control(),
    ) {
        Ok(prepare) => prepare,
        Err(error) => {
            // 07 §5.1: a runtime Office conversion failure discovered while
            // re-preparing this task's precondition (the SAME converter
            // resolution + `convert_to_pdf` this file's earlier successful
            // enqueue-time prepare used, now failing -- a renderer hiccup,
            // the fixture/binary having gone away mid-run, etc.) "joins"
            // contract_violation semantics (04 §5.3: retryable, one retry)
            // rather than a permanent Retire -- a transient failure should
            // get one more chance, unlike a genuinely stale/edited-since-
            // enqueue file. Every OTHER prepare failure (a corrupt/oversized
            // file, an unrelated contract error) keeps today's Retire
            // behavior unchanged.
            let is_office_conversion_failure =
                pipeline_to_kio(error).error_code() == "KIO-E-PREPARE-OFFICE-CONVERT-001";
            return if is_office_conversion_failure {
                OnlineMarkdownizePrecondition::ConversionFailed
            } else {
                OnlineMarkdownizePrecondition::Retire
            };
        }
    };
    if prepare.prepared_units.is_empty() {
        // A supported non-text-native file reaches the provider without local unit
        // hints; the OCR response discovers its canonical page/image units.
        // A unit-scoped retry can recover those units from the previously-persisted
        // Partial manifest. Office containers still need a separate bounded conversion
        // contract, and unknown octet-stream binaries are never valid OCR inputs.
        if !supports_ocr_from_scratch(&media_type) {
            return if media_type == "application/octet-stream" {
                OnlineMarkdownizePrecondition::Retire
            } else {
                OnlineMarkdownizePrecondition::AwaitConversion
            };
        }
        let prepared_units = if task.unit_keys.is_some() {
            let store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
            let Ok(Some(previous)) = load_previous_instance_for_task(&store, task) else {
                return OnlineMarkdownizePrecondition::Retire;
            };
            previous.prepared_units
        } else {
            Vec::new()
        };
        if !retry_unit_subset_is_valid(task, &prepared_units) {
            return OnlineMarkdownizePrecondition::Retire;
        }
        return OnlineMarkdownizePrecondition::Send(PreparedOnlineMarkdownizeInput {
            path,
            media_type,
            bytes: verified.bytes,
            prepared_units,
        });
    }
    // R22-5: retire a stale task for octet-stream TEXT. The recovery is a re-index (which
    // no longer enqueues it), not a retry — the deterministic pass already handled the file.
    if is_local_passthrough_text(&media_type, &prepare.prepared_units) {
        return OnlineMarkdownizePrecondition::Retire;
    }
    if !retry_unit_subset_is_valid(task, &prepare.prepared_units) {
        return OnlineMarkdownizePrecondition::Retire;
    }
    OnlineMarkdownizePrecondition::Send(PreparedOnlineMarkdownizeInput {
        path,
        media_type,
        bytes: verified.bytes,
        prepared_units: prepare.prepared_units,
    })
}

/// Run every Pending local-HTTP markdownize task this pass.
///
/// Called from `kio index` rather than `kio batch resume`, because the two
/// commands are split by *approval and billing*. The task itself is kept — it
/// carries retry, partial-failure
/// recording, crash recovery, and an honest `pending_enrichment_tasks` — so
/// this shares the online task lifecycle while using a non-billable local
/// transport guarded by the same dispatch-time approval.
///
/// No ledger row is opened and none is settled. That is not an omission to be
/// checked for: there is no reservation to release either, so the failure path
/// cannot strand one the way the online lane's can.
pub(crate) fn execute_pending_offline_markdownize_tasks(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    store: &TaskStore,
    normalize_by_path: &mut BTreeMap<String, PendingNormalizeRef>,
    network_allowed: bool,
) -> Result<usize> {
    // An `offline_api` adapter can still be an authenticated local HTTPS peer.
    // `--offline` and an absent exact device+scope grant therefore leave the
    // task pending before constructing its client or touching the peer.
    if !network_allowed {
        return Ok(0);
    }
    let Some(execution) = active_local_ocr_execution_with_control(adapter_settings, repo.control())
    else {
        return Ok(0);
    };
    let profile = kio_adapter::local_ocr_markdownize::profile_for(execution);
    let output_ref = offline_output_ref(&profile.adapter_id);
    let mut executed = 0_usize;
    for task in store.all().map_err(pipeline_to_kio)? {
        if task.task_type != TaskType::Markdownize
            || task.status != TaskStatus::Pending
            || task.output_ref != output_ref
        {
            continue;
        }
        // Reused verbatim from the online lane, and worth reusing: it is what
        // refuses a task whose file was edited, purged, or pushed past the
        // input cap since enqueue. Persisting v2 bytes under v1's raw_hash
        // would break content-addressing identically on either route.
        let prepared_input = match classify_online_markdownize_precondition(repo, &task) {
            OnlineMarkdownizePrecondition::Send(prepared_input) => prepared_input,
            OnlineMarkdownizePrecondition::Retire => {
                retire_offline_markdownize_task(store, &task.task_id, "invalid_input")?;
                continue;
            }
            _ => continue,
        };
        // Revalidate this exact peer and the current scope policy immediately
        // before constructing its HTTP client. Other OCR or embedding grants
        // cannot authorize a local OCR secret send.
        if !local_markdownize_transport_allowed(
            adapter_settings,
            repo,
            &profile,
            classify_secret(&task.input_path).is_some(),
        )? {
            continue;
        }
        let task_id = task.task_id.clone();
        match execute_offline_markdownize_task(
            adapter_settings,
            repo,
            &task,
            prepared_input,
            execution,
        ) {
            Ok(outcome) => {
                // `persist_normalized_instance` has just committed the immutable
                // normalized-unit CAS objects.  Bind this pass's tree entry to the
                // manifest selected from that authoritative representation before
                // the later rebuild reads it.  The path-named normalized instance
                // remains a cache; using its old pre-OCR manifest pin here would
                // correctly make the CAS-aware rebuild see no units at all.
                let manifest_hash =
                    compute_manifest_hash(repo, &task.input_hash, &profile.tool_profile_hash, 0)?;
                normalize_by_path.insert(
                    task.input_path.clone(),
                    PendingNormalizeRef {
                        expected_raw_hash: task.input_hash.clone(),
                        normalize: NormalizeRef {
                            tool_profile_hash: profile.tool_profile_hash.clone(),
                            r#gen: 0,
                            manifest_hash,
                        },
                    },
                );
                store
                    .update_matching(|candidate| {
                        if candidate.task_id == task_id {
                            candidate.status = outcome.status;
                            candidate.output_ref = outcome.output_ref.clone();
                            candidate.fallback_reason = Some("local_adapter_done".to_owned());
                            candidate.heartbeat_at = None;
                            candidate.mode = Some(outcome.mode);
                            candidate.changed_unit_keys = outcome.changed_unit_keys.clone();
                            if matches!(outcome.status, TaskStatus::Partial | TaskStatus::Failed) {
                                candidate.attempts = candidate.attempts.saturating_add(1);
                            }
                            true
                        } else {
                            false
                        }
                    })
                    .map_err(pipeline_to_kio)?;
                executed += 1;
            }
            Err(failure) => {
                // Left Pending on a retryable failure — a model server that is
                // not up yet is the ordinary case, and the next `kio index`
                // should simply pick the task up again. Only a genuinely
                // non-retryable one is retired, so a stopped server never
                // silently discards work.
                let retryable = !matches!(
                    failure.retry_kind,
                    RetryErrorKind::InvalidInput | RetryErrorKind::ContractViolation
                );
                let reason = if retryable {
                    "local_adapter_unavailable"
                } else {
                    "contract_violation"
                };
                store
                    .update_matching(|candidate| {
                        if candidate.task_id == task_id {
                            candidate.attempts = candidate.attempts.saturating_add(1);
                            candidate.fallback_reason = Some(reason.to_owned());
                            if !retryable {
                                candidate.status = TaskStatus::Failed;
                            }
                            true
                        } else {
                            false
                        }
                    })
                    .map_err(pipeline_to_kio)?;
            }
        }
    }
    Ok(executed)
}

fn retire_offline_markdownize_task(store: &TaskStore, task_id: &str, reason: &str) -> Result<()> {
    store
        .update_matching(|candidate| {
            if candidate.task_id == task_id {
                candidate.status = TaskStatus::Failed;
                candidate.fallback_reason = Some(reason.to_owned());
                candidate.heartbeat_at = None;
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)?;
    Ok(())
}

/// One document through the local pipeline.
///
/// Always a Full send. The incremental path the online lane tries first exists
/// to avoid re-billing pages that did not change; with nothing to bill, its
/// only remaining effect would be extra ways to be wrong about page
/// correspondence.
fn execute_offline_markdownize_task(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    task: &TaskDescriptor,
    prepared_input: PreparedOnlineMarkdownizeInput,
    execution: LocalOcrExecution,
) -> std::result::Result<OnlineExecutionOutcome, TaskExecutionFailure> {
    let PreparedOnlineMarkdownizeInput {
        media_type,
        bytes,
        prepared_units,
        ..
    } = prepared_input;
    let profile = kio_adapter::local_ocr_markdownize::profile_for(execution);
    let scope_id = repo
        .scope_identity()
        .map(|identity| identity.scope_id)
        .map_err(|_| TaskExecutionFailure {
            retry_kind: RetryErrorKind::InvalidInput,
            retry_after_ms: None,
        })?;
    let adapter = local_ocr_markdownize_adapter_with_control(
        adapter_settings,
        execution,
        &scope_id,
        &image_store_for_task(repo)?,
        &bytes,
        repo.control(),
    )
    .map_err(task_failure_from_adapter)?;
    let hints = prepared_unit_hints(&prepared_units);
    let response = adapter
        .markdownize(MarkdownizeRequest {
            raw: RawInput {
                raw_hash: task.input_hash.clone(),
                path: Some(task.input_path.clone()),
            },
            media_type: media_type.clone(),
            prepared_unit_hint: Some(hints.clone()),
            mode: AdapterMarkdownizeMode::Full,
            previous: None,
            hints: None,
            restrict_to_hint_pages: false,
            // 07 §5.2's bbox_annotation is Mistral's prompt-and-charge feature;
            // the local adapter refuses the flag rather than pretending.
            bbox_annotation_enabled: false,
            tool_profile_hash: profile.tool_profile_hash.clone(),
            spec_version: 1,
            // No ledger row exists, so there is no intent token to carry.
            idempotency_token: None,
        })
        .map_err(task_failure_from_adapter)?;
    // OCR-from-scratch is the normal case here: a scanned PDF has no text layer,
    // so `prepared_units` is empty and the unit identities exist only in the
    // response. Passing the empty request set straight through made every such
    // document persist as zero units and fail the contract check.
    let effective_hints = effective_prepared_unit_hints(&hints, &task.input_hash, &response)
        .map_err(task_failure_from_adapter)?;
    materialize_online_markdownize_response(
        repo,
        task,
        prepared_units,
        &media_type,
        &bytes,
        None,
        profile.tool_profile_hash,
        response,
        &effective_hints,
    )
}

pub(crate) fn execute_online_markdownize_task(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    task: &TaskDescriptor,
    prepared_input: PreparedOnlineMarkdownizeInput,
    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): the caller's
    // ledger `intent_token` for this send, threaded into every
    // `MarkdownizeRequest` this call builds (both the incremental and Full
    // paths below) — see `MarkdownizeRequest::idempotency_token`'s doc.
    idempotency_token: Option<String>,
) -> std::result::Result<OnlineExecutionOutcome, TaskExecutionFailure> {
    let PreparedOnlineMarkdownizeInput {
        path,
        media_type,
        bytes,
        prepared_units,
    } = prepared_input;
    // R14-2: an online markdownize task is always deferred — one pass enqueues it and
    // a later `batch resume` / `index --online` executes it — so the file may have been
    // edited in between. Executing a stale task reads the CURRENT bytes yet persists
    // them under the enqueue-time `input_hash` (identity = raw_hash), i.e. it stores v2
    // content under v1 identity. That breaks the content-addressing invariant, is
    // sticky (CAS idempotency never re-OCRs H(v1) again), mis-bills the v2 OCR under
    // v1, and poisons the next incremental's baseline (`latest_online_instance_for_path`
    // reads the tainted instance as "previous"). If the current file no longer hashes
    // to `task.input_hash`, supersede this task: do NOT execute or persist. The next
    // `index` enqueues a fresh task for the new content (the recovery is a re-index, not
    // a retry of this obsolete task — hence non-retryable InvalidInput; the task state
    // machine has no dedicated "superseded" state). Distinct from R14-1 (which degrades
    // an unreadable PREVIOUS instance to a Full re-send of the CURRENT content): here the
    // CURRENT content itself no longer matches the task, so there is nothing to send.
    if !repository_scan_policy_allows_file(repo, &task.input_path).unwrap_or(false) {
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::InvalidInput,
            retry_after_ms: None,
        });
    }
    // R9-2 (defense in depth): never send a text-native file to online OCR even if
    // a task for it already exists (e.g. enqueued by a pre-fix build or a poisoned
    // tasks.jsonl). Fail fast as a non-retryable input error instead of a silent,
    // billed send.
    if is_text_native_media(&media_type) {
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::InvalidInput,
            retry_after_ms: None,
        });
    }
    // R22-5 (defense in depth, mirroring the R9-2 guard above): never ship octet-stream TEXT
    // to online OCR even if a task for it exists (enqueued by a pre-R21-4 build, or a
    // poisoned tasks.jsonl). Fail fast as a non-retryable input error, never a billed send.
    if is_local_passthrough_text(&media_type, &prepared_units) {
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::InvalidInput,
            retry_after_ms: None,
        });
    }
    let scope_id = repo
        .scope_identity()
        .map(|identity| identity.scope_id)
        .map_err(|_| TaskExecutionFailure {
            retry_kind: RetryErrorKind::InvalidInput,
            retry_after_ms: None,
        })?;
    // R11-6: on a UNIT-SCOPED retry, `task.unit_keys` names the still-failed units.
    // Request ONLY those from the adapter (re-OCR + re-bill just the failed subset,
    // not the whole document); the full prepared set still drives the manifest so
    // done/failed accounting covers every unit. `unit_keys == None` = a full send.
    let retry_units: Option<BTreeSet<String>> = task
        .unit_keys
        .as_ref()
        .map(|keys| keys.iter().cloned().collect());
    // R13-1: for a FRESH send (not a unit-scoped retry), try incremental Markdownize
    // first — re-OCR only the changed/added pages and reuse the unchanged pages'
    // markdown from the prior online instance (docs/04 §2.2/§3.1, docs/07 §8 note).
    // Returns Some on success; None when it doesn't apply or the acceptance check
    // fails (fall through to the Full send below); Err on an adapter auth/rate error
    // (a Full re-send would hit the same error, so propagate).
    //
    // 07 §5.1 scope-trim: Office (DOCX/PPTX) media always sends `mode=Full`. The
    // prior online instance's `PreviousMarkdownizeContext.raw.raw_hash` (built
    // from `previous.manifest.raw_hash`, the ORIGINAL file's raw_hash) has no
    // sound relationship to a fresh per-send CONVERTED-PDF identity
    // (`run_standard_online_markdownize_with_bytes` computes a NEW hash from a
    // freshly reconverted PDF on every call, §5.1's converted-PDF path) --
    // reusing it for an incremental "diff against the previous document" send
    // would need the OCR adapter/provider to understand two DIFFERENT converted
    // PDFs' page correspondence, which nothing in this contract establishes.
    // Full re-markdownize is correct and safe; see the implementation report for
    // the fuller incremental-office design space this defers.
    if retry_units.is_none()
        && task.fallback_reason.as_deref() != Some(SYNC_RESEND_AUTHORIZED_REASON)
        && !prepared_units.is_empty()
        && !is_office_media(&media_type)
        && let Some(outcome) = try_online_incremental_markdownize(
            adapter_settings,
            repo,
            task,
            &prepared_units,
            &scope_id,
            &media_type,
            &path,
            &bytes,
            idempotency_token.clone(),
        )?
    {
        return Ok(outcome);
    }
    let request_units: Vec<PreparedUnit> = match &retry_units {
        Some(keys) => prepared_units
            .iter()
            .filter(|unit| keys.contains(&unit.unit_key))
            .cloned()
            .collect(),
        None => prepared_units.clone(),
    };
    let outcome = run_standard_online_markdownize_with_bytes_with_control(
        std::sync::Arc::clone(adapter_settings),
        StandardOnlineMarkdownizeRequest {
            scope_id: &scope_id,
            image_store: image_store_for_task(repo)?,
            raw_hash: &task.input_hash,
            path: &path,
            media_type: &media_type,
            prepared_unit_hints: prepared_unit_hints(&request_units),
            // Full send (or unit-scoped retry): every requested unit, no previous/hints.
            mode: AdapterMarkdownizeMode::Full,
            previous: None,
            hints: None,
            // R15-5: a unit-scoped retry (`retry_units.is_some()`) re-sends ONLY the failed
            // subset — scope the real OCR send/bill to those pages instead of the whole
            // document (the ledger already prorated the reserve to the subset). A fresh full
            // send (`retry_units.is_none()`) leaves this false → whole document, no `pages`.
            restrict_to_hint_pages: retry_units.is_some(),
            bbox_annotation_enabled: task.bbox_annotation_enabled.unwrap_or(false),
            // QA13 (04 §5.5 L880): see this function's `idempotency_token`
            // param doc.
            idempotency_token,
        },
        &bytes,
        repo.control(),
    )
    .map_err(task_failure_from_adapter)?;
    materialize_online_markdownize_response(
        repo,
        task,
        prepared_units,
        &media_type,
        &bytes,
        retry_units.as_ref(),
        outcome.profile.tool_profile_hash.clone(),
        outcome.response,
        &outcome.effective_prepared_unit_hints,
    )
}

/// The response half of [`execute_online_markdownize_task`] — everything that
/// happens AFTER a `MarkdownizeResponse` is in hand: OCR-from-scratch unit
/// discovery, first-instance-wins merge for a unit-scoped retry, unit
/// validation, and manifest + normalized-instance persistence. Split out so
/// the Batch collect lane (04 §5.8 相 3 — `poll_batch_markdownize_jobs`)
/// drives the EXACT same verification/materialization path as the sync lane:
/// the two lanes differ only in where the response came from (a synchronous
/// POST vs a fetched batch-output line).
#[allow(clippy::too_many_arguments)]
pub(crate) fn materialize_online_markdownize_response(
    repo: &Repository,
    task: &TaskDescriptor,
    mut prepared_units: Vec<PreparedUnit>,
    media_type: &str,
    bytes: &[u8],
    retry_units: Option<&BTreeSet<String>>,
    profile_tool_hash: String,
    response: kio_adapter::types::MarkdownizeResponse,
    effective_prepared_unit_hints: &[PreparedUnitHint],
) -> std::result::Result<OnlineExecutionOutcome, TaskExecutionFailure> {
    if prepared_units.is_empty() {
        if retry_units.is_some() {
            return Err(TaskExecutionFailure {
                retry_kind: RetryErrorKind::ContractViolation,
                retry_after_ms: None,
            });
        }
        prepared_units = prepared_units_from_ocr_discovery(
            effective_prepared_unit_hints,
            media_type,
            &task.input_hash,
            bytes,
            &builtin_prepare_profile().tool_profile_hash,
        )
        .map_err(|_| TaskExecutionFailure {
            retry_kind: RetryErrorKind::ContractViolation,
            retry_after_ms: None,
        })?;
        // No page artifact exists before OCR for a scanned PDF. The already-verified
        // immutable raw object is the bounded prepared source shared by each discovered
        // unit; publish it under the same content hash before the manifest can refer to it.
        write_canonical_cas_object_or_reuse(
            repo.kio_dir(),
            kio_core::cas::ContentObjectKind::Prepared,
            &task.input_hash,
            bytes,
        )
        .map_err(|_| TaskExecutionFailure {
            retry_kind: persist_failure_retry_kind(),
            retry_after_ms: None,
        })?;
    }
    // QA17 (step4b-contract-tests-p3a.md §F): carry the adapter's
    // self-reported billing usage (Mistral OCR's real processed-page count)
    // through to the caller's settle call, instead of discarding it here.
    let usage = response.usage.clone();
    // A unit-scoped retry requested only its failed subset. Validate exactly
    // that response boundary; immutable previously completed units are merged
    // below and must not be requested or billed again just to pass coverage.
    let response_prepared_units = prepared_units
        .iter()
        .filter(|unit| retry_units.is_none_or(|keys| keys.contains(&unit.unit_key)))
        .cloned()
        .collect::<Vec<_>>();
    let hints = all_changed_hints(&response_prepared_units);
    let acceptance = validate_markdownize_response(&response, &hints, &response_prepared_units);
    // Normalized Markdown is immutable once published. Every origin, including
    // paid sync and collected Batch output, must pass the same v1 acceptance gate
    // before any normalized CAS object is written. A fallback control response is
    // not archival content and is rejected here as well.
    let strict_valid = matches!(
        acceptance,
        Ok(kio_pipeline::markdownize::MarkdownizeAcceptance::Accepted)
    );
    if !strict_valid {
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::ContractViolation,
            retry_after_ms: None,
        });
    }
    let generated_at = repo.control().now_utc_seconds();
    // R11-6: preserve previously-done units (first-instance-wins). Load the prior
    // instance this run overwrites (same raw_hash + resolved tool_profile_hash +
    // gen 0). Regenerating a done unit under Markdown non-determinism would churn its
    // fingerprint → needless re-embedding + Evidence churn (docs/04 §5.2).
    let previous = if retry_units.is_some() {
        load_previous_instance_identity(
            repo.kio_dir(),
            &task.input_hash,
            &profile_tool_hash,
            0,
            repo.control(),
        )
        .ok()
    } else {
        None
    };
    let mut units = normalized_units_from_response(
        repo,
        media_type,
        bytes,
        &response,
        &prepared_units,
        previous.as_ref(),
        &task.input_hash,
        &profile_tool_hash,
        0,
        MarkdownizeMode::Full,
        &generated_at,
    )
    .map_err(NormalizedUnitsError::task_failure)?;
    // R11-6: merge in the previously-done units the retry did not target. The retry
    // request omitted them, so the adapter never returned them — keep the FIRST
    // instance's output verbatim (first-instance-wins).
    if let Some(prev) = &previous {
        let produced: BTreeSet<String> = units.iter().map(|unit| unit.unit_key.clone()).collect();
        for prev_unit in &prev.units {
            if !produced.contains(&prev_unit.unit_key) {
                units.push(prev_unit.clone());
            }
        }
        // Re-establish document order after appending the preserved units.
        let order_of: BTreeMap<&str, u64> = prepared_units
            .iter()
            .map(|unit| (unit.unit_key.as_str(), unit.order))
            .collect();
        units.sort_by_key(|unit| {
            order_of
                .get(unit.unit_key.as_str())
                .copied()
                .unwrap_or(u64::MAX)
        });
    }
    // A validated provider response may legally report every requested unit in
    // `failed_units`. It is a known result, not a transport failure: publish the
    // zero-object manifest with every prepared unit marked Failed so sync settles
    // the known response and Batch collection cannot loop by re-fetching it.
    let done = units.len();
    let failed = prepared_units.len().saturating_sub(done);
    // Validation accepts well-formed failed_units as well as successful units.
    // Completion therefore depends on the merged output count, not acceptance.
    let status = task_status_from_unit_counts(done, failed, false);
    let run_id = format!("run_{}", new_ulid(repo.canonical_root()));
    let failed_kinds = merged_failed_unit_retry_kinds(&response, previous.as_ref(), retry_units);
    let manifest = manifest_from_units_with_failed_kinds(
        &prepared_units,
        &units,
        &task.input_hash,
        &profile_tool_hash,
        0,
        None,
        &run_id,
        &generated_at,
        &failed_kinds,
        RetryErrorKind::NetworkError,
    );
    let _stamped_manifest =
        persist_normalized_instance_with_control(repo.kio_dir(), &manifest, &units, repo.control())
            .map_err(|_| TaskExecutionFailure {
                retry_kind: persist_failure_retry_kind(),
                retry_after_ms: None,
            })?;
    Ok(OnlineExecutionOutcome::full(
        normalized_output_ref(repo, &task.input_hash, &profile_tool_hash, 0),
        status,
    )
    .with_usage(usage))
}

/// R13-1: attempt incremental Markdownize on the online (Mistral OCR) route.
/// `Ok(Some(outcome))` = incremental fired and passed the KIO-side acceptance
/// check (docs/04 §3.2). `Ok(None)` means the incremental route was rejected
/// before any provider request, so the caller may start one Full request. Once an
/// incremental request has been sent, every invalid or unpersistable response is
/// returned as `Err`; a non-idempotent provider call must never be followed by an
/// implicit second Full request under the same reservation. Only the changed+added pages are sent to the
/// API; unchanged pages are reused from the prior instance (`reused_from`), which
/// is the cost fix (a light revision no longer re-sends/re-bills every page).
#[allow(clippy::too_many_arguments)]
fn try_online_incremental_markdownize(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    task: &TaskDescriptor,
    prepared_units: &[PreparedUnit],
    scope_id: &str,
    media_type: &str,
    path: &Path,
    verified_raw_bytes: &[u8],
    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): forwarded from
    // `execute_online_markdownize_task` — see its doc.
    idempotency_token: Option<String>,
) -> std::result::Result<Option<OnlineExecutionOutcome>, TaskExecutionFailure> {
    let invalid = || TaskExecutionFailure {
        retry_kind: RetryErrorKind::InvalidInput,
        retry_after_ms: None,
    };
    let incremental_config = effective_incremental_config(repo).map_err(|_| invalid())?;
    if !incremental_config.enabled {
        return Ok(None);
    }
    let task_store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    // The prior online instance for this path (its own resolved tool_profile_hash).
    let Some(previous) =
        latest_online_instance_for_path(&task_store, &task.input_path).map_err(|_| invalid())?
    else {
        return Ok(None);
    };
    // unit_mapping (docs/04 §2.2) between the prior and current prepared units, then
    // the documented 5-condition decision (R12-1's `choose_markdownize_mode`, which
    // now reaches the incremental gate because the adapter declares the capability).
    let mapping = map_units(&previous.prepared_units, prepared_units);
    // R14-6: resolve the online adapter's profile NOW (this resolves the model pin — a
    // `GET /v1/models` for a `*-latest` alias, never an OCR upload/bill) so the pin/profile
    // gate below runs BEFORE any incremental send. Previously the mismatch was only checked
    // AFTER the incremental request had been sent (and, post-R14-4, the whole document
    // uploaded), wasting a send + bill and then re-sending Full. `capability_flags` are
    // pin-independent, so the mode decision is unchanged.
    let resolved_profile = resolve_standard_online_markdownize_profile_with_bbox_with_control(
        std::sync::Arc::clone(adapter_settings),
        scope_id,
        task.bbox_annotation_enabled.unwrap_or(false),
        repo.control(),
    )
    .map_err(task_failure_from_adapter)?;
    let decision = choose_markdownize_mode(&IncrementalModeInput {
        has_previous_done_run: true,
        raw_hash_only_changed: true,
        adapter_capabilities: resolved_profile.capability_flags.clone(),
        change_rate: mapping.change_rate,
        threshold: incremental_config.threshold,
        consecutive_incremental_count: consecutive_online_incremental_count(
            &task_store,
            &task.input_path,
        )
        .map_err(|_| invalid())?,
        max_consecutive_incremental: incremental_config.max_consecutive,
    });
    if decision.mode != MarkdownizeMode::Incremental {
        return Ok(None);
    }
    // R14-6 gate (docs/04 §3.1 condition 2): a changed resolved pin is a different
    // tool_profile → NOT an eligible incremental. Fall back to a Full send BEFORE sending
    // (and, post-R14-4, before uploading) anything. This replaces the former post-send
    // re-check.
    if resolved_profile.tool_profile_hash != previous.manifest.tool_profile_hash {
        return Ok(None);
    }
    let incremental_hints = incremental_hints_from_mapping(&mapping, prepared_units);
    // Send ONLY the changed+added units to the OCR API (the cost fix).
    let requested: Vec<PreparedUnit> = prepared_units
        .iter()
        .filter(|unit| {
            incremental_hints.changed_unit_keys.contains(&unit.unit_key)
                || incremental_hints.added_unit_keys.contains(&unit.unit_key)
        })
        .cloned()
        .collect();
    // R15-6: a 0-change incremental (`requested` empty — nothing changed or added; only
    // unchanged units, possibly with pure removals) must NOT touch the adapter. There is
    // no page to OCR, so reuse every current unit from the prior instance directly.
    // Before this, an empty `requested` still issued an Incremental request whose empty
    // `pages` the real Mistral client paired with the WHOLE base64 document (all-pages
    // OCR/bill), contradicting docs/04 §3.2 / 04-pipeline "unchanged units are reused
    // KIO-side, not sent". The resolved profile was gated equal to `previous` above, so
    // no send is needed to learn the tool_profile_hash.
    let incremental_sent = !requested.is_empty();
    let (mut response, profile_tool_hash) = if requested.is_empty() {
        (
            kio_adapter::types::MarkdownizeResponse {
                mode_used: AdapterMarkdownizeMode::Incremental,
                updated_units: Vec::new(),
                unchanged_unit_keys: Vec::new(),
                added_units: Vec::new(),
                removed_unit_keys: Vec::new(),
                failed_units: Vec::new(),
                fallback_to_full: false,
                reason: None,
                // QA17: a 0-change incremental never calls the adapter (see
                // comment above), so there is no real usage to report.
                usage: None,
            },
            resolved_profile.tool_profile_hash.clone(),
        )
    } else {
        let adapter_previous = PreviousMarkdownizeContext {
            raw: RawInput {
                raw_hash: previous.manifest.raw_hash.clone(),
                path: Some(path.display().to_string()),
            },
            normalized_units: previous
                .units
                .iter()
                .map(normalized_unit_to_adapter_unit)
                .collect(),
            tool_profile_hash: previous.manifest.tool_profile_hash.clone(),
        };
        let outcome = run_standard_online_markdownize_with_bytes_with_control(
            std::sync::Arc::clone(adapter_settings),
            StandardOnlineMarkdownizeRequest {
                scope_id,
                image_store: image_store_for_task(repo)?,
                raw_hash: &task.input_hash,
                path,
                media_type,
                prepared_unit_hints: prepared_unit_hints(&requested),
                mode: AdapterMarkdownizeMode::Incremental,
                previous: Some(adapter_previous),
                hints: Some(adapter_hints(&incremental_hints)),
                // Incremental already scopes pages via `mode`; the retry-only signal stays off.
                restrict_to_hint_pages: false,
                bbox_annotation_enabled: task.bbox_annotation_enabled.unwrap_or(false),
                // QA13 (04 §5.5 L880): see this function's `idempotency_token`
                // param doc.
                idempotency_token,
            },
            verified_raw_bytes,
            repo.control(),
        )
        .map_err(task_failure_from_adapter)?;
        let response = outcome.response;
        // R14-6: the pin/profile mismatch was gated BEFORE the send above, so
        // `outcome.profile` matches `previous` here (same resolution) — no post-send
        // re-check is needed. Acceptance (docs/04 §3.2): the OCR response must cover
        // every requested (changed+added) unit, and the adapter must not have declined.
        // A response failure is after a non-idempotent call, so it cannot trigger
        // an implicit Full re-send.
        if response.fallback_to_full {
            return Err(TaskExecutionFailure {
                retry_kind: RetryErrorKind::ContractViolation,
                retry_after_ms: None,
            });
        }
        (response, outcome.profile.tool_profile_hash)
    };
    // Kio orchestrates the unchanged reuse (docs/07 §8: the document-processing
    // route reuses unchanged units KIO-side rather than via the adapter). Inject the
    // unchanged new keys so `normalized_units_from_response` copies their markdown
    // from the prior instance with `reused_from` set. In the 0-change path this is
    // EVERY current unit.
    response.unchanged_unit_keys = mapping
        .unchanged
        .iter()
        .map(|reuse| reuse.new_unit_key.clone())
        .collect();
    // The adapter response plus Kio-owned unchanged set is the exact v1
    // incremental partition. Validate it before a normalized unit can be
    // constructed; a bad post-send response must not trigger an implicit Full
    // request under the same non-idempotent reservation.
    if !matches!(
        validate_markdownize_response(&response, &incremental_hints, prepared_units),
        Ok(kio_pipeline::markdownize::MarkdownizeAcceptance::Accepted)
    ) {
        if !incremental_sent {
            return Ok(None);
        }
        return Err(TaskExecutionFailure {
            retry_kind: RetryErrorKind::ContractViolation,
            retry_after_ms: None,
        });
    }
    let generated_at = repo.control().now_utc_seconds();
    let units = match normalized_units_from_response(
        repo,
        media_type,
        verified_raw_bytes,
        &response,
        prepared_units,
        Some(&previous),
        &task.input_hash,
        &profile_tool_hash,
        0,
        MarkdownizeMode::Incremental,
        &generated_at,
    ) {
        Ok(units) => units,
        // With no requested units no provider call occurred, so an ordinary Full
        // request remains safe. After a real incremental request every failure is
        // result-unknown and must return to the sync recovery boundary.
        Err(_error) if !incremental_sent => return Ok(None),
        Err(error) => return Err(error.task_failure()),
    };
    let run_id = format!("run_{}", new_ulid(repo.canonical_root()));
    let failed_kinds = failed_unit_retry_kinds(&response);
    let manifest = manifest_from_units_with_failed_kinds(
        prepared_units,
        &units,
        &task.input_hash,
        &profile_tool_hash,
        0,
        Some(previous.manifest.r#gen),
        &run_id,
        &generated_at,
        &failed_kinds,
        RetryErrorKind::NetworkError,
    );
    if let Err(_error) =
        persist_normalized_instance_with_control(repo.kio_dir(), &manifest, &units, repo.control())
    {
        if !incremental_sent {
            return Ok(None);
        }
        return Err(TaskExecutionFailure {
            retry_kind: persist_failure_retry_kind(),
            retry_after_ms: None,
        });
    }
    let status = task_status_from_unit_counts(
        units.len(),
        prepared_units.len().saturating_sub(units.len()),
        false,
    );
    Ok(Some(OnlineExecutionOutcome {
        output_ref: normalized_output_ref(repo, &task.input_hash, &profile_tool_hash, 0),
        status,
        mode: MarkdownizeMode::Incremental,
        previous_raw_hash: Some(previous.manifest.raw_hash.clone()),
        parent_run_id: Some(previous.manifest.run_id.clone()),
        changed_unit_keys: incremental_hints.changed_unit_keys.clone(),
        // QA17: the real send's self-reported usage (`None` for the 0-change
        // branch above, which never called the adapter).
        usage: response.usage.clone(),
    }))
}

/// R13-1: the latest prior online-markdownize instance for `input_path` (the DONE
/// online task carries `fallback_reason = "online_adapter_done"` and an output_ref
/// pointing at its normalized instance). Distinct from `previous_instance_for_path`
/// (which the offline route uses, filtering out online refs) — here we WANT the
/// online instance and its own resolved `tool_profile_hash`.
fn latest_online_instance_for_path(
    task_store: &TaskStore,
    input_path: &str,
) -> Result<Option<PreviousInstance>> {
    let mut tasks = task_store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| {
            task.input_path == input_path
                && matches!(task.status, TaskStatus::Done | TaskStatus::Partial)
                && task.fallback_reason.as_deref() == Some("online_adapter_done")
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    for task in tasks {
        if let Some(previous) = load_previous_instance_for_task(task_store, &task)? {
            return Ok(Some(previous));
        }
    }
    Ok(None)
}

/// R13-1: consecutive prior online incremental runs for `input_path` (docs/04
/// §3.1 condition 5 — force Full after N to bound style drift). Counts back from
/// the most recent DONE online task while it was `mode = Incremental`.
fn consecutive_online_incremental_count(task_store: &TaskStore, input_path: &str) -> Result<u32> {
    let mut tasks = task_store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| {
            task.input_path == input_path
                && task.status == TaskStatus::Done
                && task.fallback_reason.as_deref() == Some("online_adapter_done")
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let mut count = 0u32;
    for task in tasks {
        if task.mode == Some(MarkdownizeMode::Incremental) {
            count = count.saturating_add(1);
        } else {
            break;
        }
    }
    Ok(count)
}

/// R10-5: a `persist_normalized_instance` failure occurs AFTER a successful (and
/// already-billed, F8) OCR call, so it is a write-side I/O fault (ENOSPC / EIO /
/// interrupted fsync / a transient permission glitch), never bad input. Classify it
/// as a retryable `NetworkError` (retryable I/O) so `batch retry` can re-drive it
/// once the disk condition clears, instead of a non-retryable `InvalidInput` that
/// strands the billed, normalized output forever (retry & re-index both refuse it).
pub(crate) fn persist_failure_retry_kind() -> RetryErrorKind {
    RetryErrorKind::NetworkError
}

fn image_store_for_task(
    repo: &Repository,
) -> std::result::Result<kio_core::cas::ImageStore, TaskExecutionFailure> {
    repo.object_store()
        .image_store()
        .map_err(|_| TaskExecutionFailure {
            retry_kind: RetryErrorKind::ContractViolation,
            retry_after_ms: None,
        })
}

pub(crate) fn task_failure_from_adapter(error: kio_adapter::AdapterError) -> TaskExecutionFailure {
    // QA3: capture BEFORE `match error` moves it — `Some` only for a
    // `RateLimit` whose provider response carried a parseable `Retry-After`
    // header (kio-adapter's `AdapterError::retry_after_ms`).
    let retry_after_ms = error.retry_after_ms();
    let retry_kind = match error {
        kio_adapter::AdapterError::Auth(_) => RetryErrorKind::AuthError,
        kio_adapter::AdapterError::LocalPeerAuth(_) => RetryErrorKind::LocalPeerAuth,
        kio_adapter::AdapterError::LocalPeerConfig { .. } => RetryErrorKind::LocalPeerConfig,
        kio_adapter::AdapterError::RateLimit { .. } => RetryErrorKind::RateLimit,
        kio_adapter::AdapterError::QuotaExceeded(_) => RetryErrorKind::QuotaExceeded,
        kio_adapter::AdapterError::Network(_) | kio_adapter::AdapterError::Io { .. } => {
            RetryErrorKind::NetworkError
        }
        kio_adapter::AdapterError::ContractViolation(_)
        | kio_adapter::AdapterError::ConfigSchema(_)
        | kio_adapter::AdapterError::ConfigSchemaCoded { .. } => RetryErrorKind::ContractViolation,
    };
    TaskExecutionFailure {
        retry_kind,
        retry_after_ms,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn task_descriptor(
    repo: &Repository,
    task_type: TaskType,
    mode: Option<MarkdownizeMode>,
    candidate: &ScanCandidate,
    input_hash: &str,
    output_ref: &str,
    status: TaskStatus,
    fallback_reason: Option<&str>,
    created_at: &str,
) -> TaskDescriptor {
    TaskDescriptor {
        task_id: format!("task_{}", new_ulid(repo.canonical_root())),
        task_type,
        embedding_target: None,
        mode,
        input_path: candidate.input_path.clone(),
        input_hash: input_hash.to_owned(),
        previous_raw_hash: None,
        parent_run_id: None,
        changed_unit_keys: Vec::new(),
        output_ref: output_ref.to_owned(),
        unit_keys: None,
        status,
        attempts: 0,
        next_retry_at: None,
        deadline: None,
        heartbeat_at: None,
        fallback_reason: fallback_reason.map(str::to_owned),
        created_at: created_at.to_owned(),
        bbox_annotation_enabled: None,
        // QA1 (step4b-contract-tests-p3a.md §A): derive the closed hold_reason
        // from `fallback_reason` whenever this descriptor is Paused; `None`
        // for any other status or an unrecognized reason.
        hold_reason: (status == TaskStatus::Paused)
            .then(|| fallback_reason.and_then(hold_reason_for_reason))
            .flatten(),
        reserved_usd: None,
        reserved_month: None,
        reservation_id: None,
    }
}

pub(crate) fn append_failed_markdownize_task(
    repo: &Repository,
    task_store: &TaskStore,
    candidate: &ScanCandidate,
    input_hash: &str,
    output_ref: &str,
    fallback_reason: &str,
    created_at: &str,
) -> Result<()> {
    let task = task_descriptor(
        repo,
        TaskType::Markdownize,
        Some(MarkdownizeMode::Full),
        candidate,
        input_hash,
        output_ref,
        TaskStatus::Failed,
        Some(fallback_reason),
        created_at,
    );
    task_store.append(&task).map_err(pipeline_to_kio)
}

pub(crate) fn prepared_unit_hints(prepared_units: &[PreparedUnit]) -> Vec<PreparedUnitHint> {
    prepared_units
        .iter()
        .map(|unit| PreparedUnitHint {
            unit_key: unit.unit_key.clone(),
            prepared_hash: unit.prepared_hash.clone(),
            unit_kind: adapter_unit_kind(unit.unit_type),
            order: unit.order,
        })
        .collect()
}

pub(crate) fn adapter_unit_kind(unit_type: UnitType) -> UnitKind {
    match unit_type {
        UnitType::Page => UnitKind::Page,
        UnitType::Slide => UnitKind::Slide,
        UnitType::HeadingSection => UnitKind::HeadingSection,
        UnitType::Sheet => UnitKind::Sheet,
        UnitType::Image => UnitKind::Image,
        UnitType::File => UnitKind::File,
        UnitType::Symbol => UnitKind::Symbol,
    }
}

pub(crate) fn adapter_mode(mode: MarkdownizeMode) -> AdapterMarkdownizeMode {
    match mode {
        MarkdownizeMode::Full => AdapterMarkdownizeMode::Full,
        MarkdownizeMode::Incremental => AdapterMarkdownizeMode::Incremental,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PreviousInstance {
    pub(crate) manifest: NormalizedInstanceManifest,
    pub(crate) units: Vec<NormalizedUnitObject>,
    pub(crate) prepared_units: Vec<PreparedUnit>,
}

pub(crate) fn previous_instance_for_path(
    task_store: &TaskStore,
    input_path: &str,
    tool_profile_hash: &str,
) -> Result<Option<PreviousInstance>> {
    let mut tasks = task_store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| {
            task.input_path == input_path
                && matches!(task.status, TaskStatus::Done | TaskStatus::Partial)
                && !task.output_ref.starts_with("online:")
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    for task in tasks {
        let Some(previous) = load_previous_instance_for_task(task_store, &task)? else {
            continue;
        };
        if previous.manifest.tool_profile_hash == tool_profile_hash {
            return Ok(Some(previous));
        }
    }
    Ok(None)
}

fn load_previous_instance_for_task(
    task_store: &TaskStore,
    task: &TaskDescriptor,
) -> Result<Option<PreviousInstance>> {
    let identity = validate_task_output_ref(task_store.kio_dir(), task).map_err(pipeline_to_kio)?;
    let TaskOutputRef::NormalizedInstance {
        raw_hash,
        tool_profile_hash,
        r#gen,
        ..
    } = identity
    else {
        return Ok(None);
    };
    // A missing/corrupt previous instance degrades to a Full run for this document,
    // while a forged task reference is rejected above before it reaches the filesystem.
    Ok(load_previous_instance_identity(
        task_store.kio_dir(),
        &raw_hash,
        &tool_profile_hash,
        r#gen,
        task_store.control(),
    )
    .ok())
}

fn load_previous_instance_identity(
    kio_dir: &Path,
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
    control: &ControlHandle,
) -> Result<PreviousInstance> {
    let instance = load_validated_normalized_instance_with_control(
        kio_dir,
        raw_hash,
        tool_profile_hash,
        r#gen,
        control,
    )
    .map_err(pipeline_to_kio)?;
    let manifest = instance.manifest;
    let units = instance.units;
    let prepared_units = manifest
        .units
        .iter()
        .map(|entry| PreparedUnit {
            order: entry.order,
            unit_key: entry.unit_key.clone(),
            unit_type: entry.unit_type,
            prepared_hash: entry.prepared_hash.clone(),
            preparation_profile_hash: entry.preparation_profile_hash.clone(),
            fingerprint: UnitFingerprint {
                perceptual_hash: entry.prepared_hash.clone(),
                text_hash: entry.prepared_hash.clone(),
                visual_hash: entry.prepared_hash.clone(),
            },
            mime: None,
            page_number: (entry.unit_type == UnitType::Page).then_some(entry.order + 1),
        })
        .collect();
    Ok(PreviousInstance {
        manifest,
        units,
        prepared_units,
    })
}

pub(crate) fn incremental_hints_from_mapping(
    mapping: &kio_pipeline::prepare::UnitMapping,
    prepared_units: &[PreparedUnit],
) -> IncrementalHints {
    IncrementalHints {
        changed_unit_keys: mapping.changed_unit_keys.clone(),
        added_unit_keys: mapping.added_unit_keys.clone(),
        removed_unit_keys: mapping.removed_unit_keys.clone(),
        page_fingerprints: prepared_units
            .iter()
            .map(|unit| (unit.unit_key.clone(), unit.fingerprint.clone()))
            .collect(),
    }
}

pub(crate) fn all_changed_hints(prepared_units: &[PreparedUnit]) -> IncrementalHints {
    IncrementalHints {
        changed_unit_keys: prepared_units
            .iter()
            .map(|unit| unit.unit_key.clone())
            .collect(),
        added_unit_keys: Vec::new(),
        removed_unit_keys: Vec::new(),
        page_fingerprints: prepared_units
            .iter()
            .map(|unit| (unit.unit_key.clone(), unit.fingerprint.clone()))
            .collect(),
    }
}

pub(crate) fn adapter_hints(hints: &IncrementalHints) -> kio_adapter::types::IncrementalHints {
    kio_adapter::types::IncrementalHints {
        changed_unit_keys: hints.changed_unit_keys.clone(),
        added_unit_keys: hints.added_unit_keys.clone(),
        removed_unit_keys: hints.removed_unit_keys.clone(),
        page_fingerprints: hints
            .page_fingerprints
            .iter()
            .map(|(key, fingerprint)| {
                (
                    key.clone(),
                    kio_adapter::types::UnitFingerprint {
                        perceptual_hash: fingerprint.perceptual_hash.clone(),
                        text_hash: fingerprint.text_hash.clone(),
                        visual_hash: fingerprint.visual_hash.clone(),
                    },
                )
            })
            .collect(),
    }
}

pub(crate) fn normalized_unit_to_adapter_unit(unit: &NormalizedUnitObject) -> MarkdownUnit {
    MarkdownUnit {
        unit_key: unit.unit_key.clone(),
        unit_type: adapter_unit_kind(unit.unit_type),
        markdown: unit.markdown.clone(),
        owned_image_hashes: unit.owned_image_hashes.clone(),
        metadata: unit.metadata.clone(),
    }
}

pub(crate) fn consecutive_incremental_count(
    task_store: &TaskStore,
    input_path: &str,
) -> Result<u32> {
    let mut tasks = task_store
        .all()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .filter(|task| task.input_path == input_path && task.status == TaskStatus::Done)
        .collect::<Vec<_>>();
    tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let mut count = 0u32;
    for task in tasks {
        if task.mode == Some(MarkdownizeMode::Incremental) {
            count = count.saturating_add(1);
        } else {
            break;
        }
    }
    Ok(count)
}

#[derive(Debug)]
pub(crate) enum NormalizedUnitsError {
    Contract(KioError),
    LocalPersistence(KioError),
}

impl NormalizedUnitsError {
    pub(crate) fn into_kio(self) -> KioError {
        match self {
            Self::Contract(error) | Self::LocalPersistence(error) => error,
        }
    }

    fn task_failure(self) -> TaskExecutionFailure {
        let retry_kind = match self {
            Self::Contract(_) => RetryErrorKind::ContractViolation,
            // The provider response has already been accepted. CAS write/inspect
            // failures are local recovery faults and batch collection must retain
            // its known result instead of submitting a paid request again.
            Self::LocalPersistence(_) => persist_failure_retry_kind(),
        };
        TaskExecutionFailure {
            retry_kind,
            retry_after_ms: None,
        }
    }
}

impl From<KioError> for NormalizedUnitsError {
    fn from(error: KioError) -> Self {
        Self::Contract(error)
    }
}

impl From<image_ingest::SourceImageRetentionError> for NormalizedUnitsError {
    fn from(error: image_ingest::SourceImageRetentionError) -> Self {
        match error {
            image_ingest::SourceImageRetentionError::SourceValidation(error) => {
                Self::Contract(error)
            }
            image_ingest::SourceImageRetentionError::LocalPersistence(error) => {
                Self::LocalPersistence(error)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn normalized_units_from_response(
    repo: &Repository,
    source_media_type: &str,
    verified_raw_bytes: &[u8],
    response: &kio_adapter::types::MarkdownizeResponse,
    prepared_units: &[PreparedUnit],
    previous: Option<&PreviousInstance>,
    raw_hash: &str,
    tool_profile_hash: &str,
    // QB41 (03 §2.1's second legal gen+1 path, 07 §5.1): every EXISTING call
    // site passes `0` (first-instance-wins: a fresh `(raw_hash,
    // tool_profile_hash)` identity always starts at gen 0). The ONLY caller
    // that passes non-zero is `create_prepared_hash_drift_instance` -- a
    // prepared_hash drift under an UNCHANGED raw_hash is the second gen+1
    // path this field newly makes representable.
    r#gen: u64,
    mode: MarkdownizeMode,
    generated_at: &str,
) -> std::result::Result<Vec<NormalizedUnitObject>, NormalizedUnitsError> {
    let prepared = prepared_units
        .iter()
        .map(|unit| (unit.unit_key.as_str(), unit))
        .collect::<BTreeMap<_, _>>();
    let previous_units = previous
        .map(|previous| {
            previous
                .units
                .iter()
                .map(|unit| (unit.unit_key.as_str(), unit))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut units = response
        .updated_units
        .iter()
        .chain(response.added_units.iter())
        .map(|unit| {
            let prepared = prepared
                .get(unit.unit_key.as_str())
                .ok_or_else(|| KioError::schema("adapter returned unknown unit"))?;
            let markdown = image_ingest::retain_original_source_image_reference(
                repo,
                raw_hash,
                verified_raw_bytes,
                source_media_type,
                prepared.unit_type,
                &unit.markdown,
            )?;
            let mut owned_image_hashes = unit.owned_image_hashes.clone();
            // A standalone image's raw identity was verified against these
            // exact bytes before source retention created its image CAS object.
            // It is therefore a typed owner even when a text-only OCR response
            // did not return its own image list.
            if prepared.unit_type == UnitType::Image {
                owned_image_hashes.insert(raw_hash.to_owned());
            }
            Ok(NormalizedUnitObject {
                unit_key: unit.unit_key.clone(),
                unit_type: prepared.unit_type,
                raw_hash: raw_hash.to_owned(),
                prepared_hash: prepared.prepared_hash.clone(),
                preparation_profile_hash: prepared.preparation_profile_hash.clone(),
                tool_profile_hash: tool_profile_hash.to_owned(),
                r#gen,
                mode,
                markdown,
                owned_image_hashes,
                metadata: unit.metadata.clone(),
                reused_from: None,
                generated_at: generated_at.to_owned(),
            })
        })
        .collect::<std::result::Result<Vec<_>, NormalizedUnitsError>>()?;
    for unit_key in &response.unchanged_unit_keys {
        let prepared = prepared
            .get(unit_key.as_str())
            .ok_or_else(|| KioError::schema("adapter returned unknown unchanged unit"))?;
        let previous_unit = previous_units
            .get(unit_key.as_str())
            .ok_or_else(|| KioError::schema("unchanged unit has no previous normalized unit"))?;
        units.push(NormalizedUnitObject {
            unit_key: unit_key.clone(),
            unit_type: prepared.unit_type,
            raw_hash: raw_hash.to_owned(),
            prepared_hash: prepared.prepared_hash.clone(),
            preparation_profile_hash: prepared.preparation_profile_hash.clone(),
            tool_profile_hash: tool_profile_hash.to_owned(),
            r#gen,
            mode,
            // `unchanged_unit_keys` denotes a pinned unit from the prior
            // normalized instance. Reuse must preserve its immutable Markdown
            // verbatim; source-image retention applies only when an adapter
            // emits a fresh or updated image unit.
            markdown: previous_unit.markdown.clone(),
            // Unchanged reuse is immutable: preserve the prior typed source
            // ownership exactly, rather than reinterpreting retained Markdown.
            owned_image_hashes: previous_unit.owned_image_hashes.clone(),
            metadata: previous_unit.metadata.clone(),
            reused_from: Some(kio_pipeline::markdownize::ReusedFrom {
                raw_hash: previous_unit.raw_hash.clone(),
                r#gen: previous_unit.r#gen,
                unit_key: previous_unit.unit_key.clone(),
            }),
            generated_at: generated_at.to_owned(),
        });
    }
    units.sort_by_key(|unit| {
        prepared
            .get(unit.unit_key.as_str())
            .map(|unit| unit.order)
            .unwrap_or(u64::MAX)
    });
    Ok(units)
}

/// Convert a response's already-validated per-unit failures into the immutable
/// manifest's retry vocabulary. `validate_markdownize_response` admits only the
/// closed `RetryErrorKind` spelling before this boundary; the fallback remains a
/// defensive invariant for callers that construct a manifest without a response.
fn failed_unit_retry_kinds(
    response: &kio_adapter::types::MarkdownizeResponse,
) -> BTreeMap<String, RetryErrorKind> {
    response
        .failed_units
        .iter()
        .map(|failed| {
            (
                failed.unit_key.clone(),
                retry_kind_from_reason(Some(&failed.error_kind)),
            )
        })
        .collect()
}

/// Rebuilding a retry subset still writes the full prepared-unit manifest. Keep
/// the prior immutable failure kind for every unit not requested this time, then
/// let the accepted provider response replace kinds for the requested subset.
pub(crate) fn merged_failed_unit_retry_kinds(
    response: &kio_adapter::types::MarkdownizeResponse,
    previous: Option<&PreviousInstance>,
    retry_units: Option<&BTreeSet<String>>,
) -> BTreeMap<String, RetryErrorKind> {
    let mut kinds = BTreeMap::new();
    if let (Some(previous), Some(retry_units)) = (previous, retry_units) {
        for entry in &previous.manifest.units {
            if entry.status == UnitStatus::Failed && !retry_units.contains(&entry.unit_key) {
                kinds.insert(
                    entry.unit_key.clone(),
                    retry_kind_from_reason(entry.error_kind.as_deref()),
                );
            }
        }
    }
    kinds.extend(failed_unit_retry_kinds(response));
    kinds
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn manifest_from_units(
    prepared_units: &[PreparedUnit],
    units: &[NormalizedUnitObject],
    raw_hash: &str,
    tool_profile_hash: &str,
    r#gen: u64,
    parent_gen: Option<u64>,
    run_id: &str,
    generated_at: &str,
    failed_kind: RetryErrorKind,
) -> NormalizedInstanceManifest {
    manifest_from_units_with_failed_kinds(
        prepared_units,
        units,
        raw_hash,
        tool_profile_hash,
        r#gen,
        parent_gen,
        run_id,
        generated_at,
        &BTreeMap::new(),
        failed_kind,
    )
}

#[allow(clippy::too_many_arguments)]
fn manifest_from_units_with_failed_kinds(
    prepared_units: &[PreparedUnit],
    units: &[NormalizedUnitObject],
    raw_hash: &str,
    tool_profile_hash: &str,
    // QB41 (03 §2.1's second legal gen+1 path): see `normalized_units_from_response`'s
    // matching `gen` doc -- every existing call site passes `0`.
    r#gen: u64,
    parent_gen: Option<u64>,
    run_id: &str,
    generated_at: &str,
    failed_kinds: &BTreeMap<String, RetryErrorKind>,
    fallback_failed_kind: RetryErrorKind,
) -> NormalizedInstanceManifest {
    let done = units
        .iter()
        .map(|unit| unit.unit_key.as_str())
        .collect::<BTreeSet<_>>();
    NormalizedInstanceManifest {
        raw_hash: raw_hash.to_owned(),
        tool_profile_hash: tool_profile_hash.to_owned(),
        r#gen,
        parent_gen,
        run_id: run_id.to_owned(),
        units: prepared_units
            .iter()
            .map(|unit| NormalizedUnitManifestEntry {
                order: unit.order,
                unit_key: unit.unit_key.clone(),
                unit_ref: unit_ref(&unit.unit_key),
                unit_type: unit.unit_type,
                status: if done.contains(unit.unit_key.as_str()) {
                    UnitStatus::Done
                } else {
                    UnitStatus::Failed
                },
                prepared_hash: unit.prepared_hash.clone(),
                preparation_profile_hash: unit.preparation_profile_hash.clone(),
                unit_object_hash: None,
                // Preserve the provider's admitted per-unit outcome. A fallback
                // only serves internal callers that have no response object.
                error_kind: (!done.contains(unit.unit_key.as_str())).then(|| {
                    retry_reason(
                        failed_kinds
                            .get(&unit.unit_key)
                            .copied()
                            .unwrap_or(fallback_failed_kind),
                    )
                    .to_owned()
                }),
            })
            .collect(),
        generated_at: generated_at.to_owned(),
    }
}
