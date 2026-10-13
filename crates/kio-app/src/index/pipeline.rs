use crate::batch::command::reclaim_orphaned_running_tasks;
use crate::commands::IndexArgs;
use crate::egress::SECRETS_TIER_B_HOLD;
use crate::egress::network_allowed;
use crate::historical_reindex::pinned_manifest_is_complete;
use crate::image_ingest;
use crate::index::command::OFFICE_CONVERSION_UNAVAILABLE_REASON;
use crate::index::command::read_repository_scan_input;
use crate::index::command::record_unsupported_if_changed;
use crate::index::command::repository_input_display_path;
use crate::index::command::repository_scan_policy_allows_file;
use crate::index::rebuild::read_head_tree_for_rebuild;
use crate::ledger_access::capture_search_budget_observation;
use crate::ledger_access::ledger_db_path;
use crate::ledger_access::ledger_snapshot_to_kio;
use crate::ledger_access::open_optional_ledger_db;
use crate::ledger_access::release_task_charge_if_open;
use crate::ledger_access::task_ledger_key;
use crate::markdownize::batch::retire_online_task;
use crate::markdownize::execution::NormalizedUnitsError;
use crate::markdownize::execution::adapter_hints;
use crate::markdownize::execution::adapter_mode;
use crate::markdownize::execution::all_changed_hints;
use crate::markdownize::execution::append_failed_markdownize_task;
use crate::markdownize::execution::consecutive_incremental_count;
use crate::markdownize::execution::execute_pending_offline_markdownize_tasks;
use crate::markdownize::execution::incremental_hints_from_mapping;
use crate::markdownize::execution::manifest_from_units;
use crate::markdownize::execution::normalized_unit_to_adapter_unit;
use crate::markdownize::execution::normalized_units_from_response;
use crate::markdownize::execution::prepared_unit_hints;
use crate::markdownize::execution::previous_instance_for_path;
use crate::markdownize::execution::task_descriptor;
use crate::object_persistence::adapter_to_kio;
use crate::object_persistence::compute_manifest_hash;
use crate::object_persistence::hash_and_write_manifest_object;
use crate::object_persistence::normalized_output_ref;
use crate::object_persistence::pipeline_to_kio;
use crate::object_persistence::write_prepared_objects;
use crate::online_task::targets_standard_online_markdownize;
use crate::response::index::{
    AdapterExecutionMode, IndexPreviewResponse, LargeFile, NetworkTransmissionPolicy,
    SensitiveCandidate,
};
use crate::scope_access::ensure_raw_ingest_allowed;
use crate::settings::bbox_annotation_enabled;
use crate::settings::effective_incremental_config;
use crate::settings::effective_max_input_bytes;
use crate::settings::user_config_toml_path;
use crate::task_execution::EnqueueBudget;
use crate::task_execution::budget_remaining_for_adapter;
use crate::task_execution::budget_remaining_from_totals;
use crate::task_execution::enqueue_online_placeholder_task;
use crate::task_execution::is_reservation_bearing_send_failure;
use kio_adapter::catalog::active_local_ocr_execution_with_control;
use kio_adapter::catalog::builtin_offline_markdownize_adapter;
use kio_adapter::catalog::builtin_prepare_profile;
use kio_adapter::catalog::convert_office_to_pdf_with_control;
use kio_adapter::catalog::local_ocr_handles_media_type;
use kio_adapter::catalog::standard_online_markdownize_profile;
use kio_adapter::catalog::standard_online_markdownize_profile_with_bbox;
#[cfg(debug_assertions)]
use kio_adapter::identity::tool_profile_hash;
use kio_adapter::office_convert::is_office_media;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::traits::MarkdownizeAdapter;
#[cfg(debug_assertions)]
use kio_adapter::types::AdapterKind;
use kio_adapter::types::AdapterProfile;
use kio_adapter::types::ExecutionMode;
use kio_adapter::types::MarkdownUnit;
use kio_adapter::types::MarkdownizeMode as AdapterMarkdownizeMode;
use kio_adapter::types::MarkdownizeRequest;
use kio_adapter::types::PreparedUnitHint;
use kio_adapter::types::PreviousMarkdownizeContext;
use kio_adapter::types::RawInput;
use kio_adapter::types::UnitKind;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::dag::NormalizeRef;
use kio_core::scope::PendingNormalizeRef;
use kio_core::scope::Repository;
use kio_core::scope::new_ulid;
#[cfg(debug_assertions)]
use kio_core::test_control::MarkdownizeAdapterMode;
use kio_pipeline::budget::read_budget_policy;
use kio_pipeline::markdownize::IncrementalModeDecision;
use kio_pipeline::markdownize::IncrementalModeInput;
use kio_pipeline::markdownize::MarkdownizeMode;
use kio_pipeline::markdownize::ValidatedNormalizedInstance;
use kio_pipeline::markdownize::choose_markdownize_mode;
use kio_pipeline::markdownize::load_validated_normalized_instance_with_control;
use kio_pipeline::markdownize::persist_normalized_instance_with_control;
use kio_pipeline::markdownize::validate_markdownize_response;
use kio_pipeline::prepare::PrepareStageBytesRequest;
use kio_pipeline::prepare::PreparedUnit;
use kio_pipeline::prepare::map_units;
use kio_pipeline::prepare::prepare_units_from_bytes_with_control;
use kio_pipeline::scan::ScanCandidate;
use kio_pipeline::scan::ScanPreview;
use kio_pipeline::task::RetryErrorKind;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use kio_pipeline::unsupported::UNSUPPORTED_REASON_RESOLVED;
use kio_pipeline::unsupported::UNSUPPORTED_REASON_UNRECOGNIZED_BINARY;
use kio_pipeline::unsupported::UnsupportedInputDisposition;
use kio_pipeline::unsupported::UnsupportedInputStore;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

pub(crate) fn index_preview_response(root: &Path, preview: &ScanPreview) -> IndexPreviewResponse {
    let included = preview
        .candidates
        .iter()
        .filter(|candidate| !candidate.ignored)
        .collect::<Vec<_>>();
    let estimated_size_bytes = included
        .iter()
        .map(|candidate| candidate.size_bytes)
        .sum::<u64>();
    let excluded_candidates = preview
        .candidates
        .iter()
        .filter(|candidate| candidate.ignored)
        .map(|candidate| candidate.input_path.clone())
        .collect::<Vec<_>>();
    let sensitive_candidates = preview
        .candidates
        .iter()
        .filter_map(|candidate| {
            candidate
                .quarantine_reason
                .as_ref()
                .map(|reason| SensitiveCandidate {
                    path: candidate.input_path.clone(),
                    reason: reason.clone(),
                    ignored: candidate.ignored,
                })
        })
        .collect();
    let large_files = preview
        .candidates
        .iter()
        .filter(|candidate| candidate.size_bytes >= 10 * 1024 * 1024)
        .map(|candidate| LargeFile {
            path: candidate.input_path.clone(),
            size_bytes: candidate.size_bytes,
        })
        .collect();
    IndexPreviewResponse {
        status: "preview".to_owned(),
        root: root.to_path_buf(),
        scope: preview.scope_id.clone(),
        estimated_file_count: included.len(),
        estimated_size_bytes,
        large_files,
        effective_ignore: ".kioignore + built-in secrets Tier A".to_owned(),
        excluded_candidates,
        sensitive_candidates,
        network_transmission_policy: NetworkTransmissionPolicy {
            default: "disabled until adapter approve".to_owned(),
            yes_grants_network: false,
        },
        adapter_execution_mode: AdapterExecutionMode {
            markdownize: "deterministic_library baseline".to_owned(),
            embedding: "not generated without online opt-in".to_owned(),
        },
        estimated_cost: preview.estimated_cost.clone(),
        budget_cap: preview
            .estimated_cost
            .as_ref()
            .and_then(|cost| cost.budget_cap_usd),
        estimated_completion:
            "baseline completes in this run; online enhancement depends on budget and opt-in"
                .to_owned(),
        candidates: preview.candidates.clone(),
        child_scopes: None,
        estimated_aggregate_file_count: None,
        estimated_aggregate_size_bytes: None,
        estimated_aggregate_cost_usd: None,
        child_scope_discovery: None,
    }
}

#[derive(Debug, Default)]
pub(crate) struct IndexPipelineResult {
    pub(crate) normalize_by_path: BTreeMap<String, PendingNormalizeRef>,
    pub(crate) network_allowed: bool,
    pub(crate) pending_online_tasks: usize,
    pub(crate) paused_tasks: usize,
    pub(crate) normalized_files: usize,
    pub(crate) pending_files: usize,
    pub(crate) failed_files: usize,
    // R12-2: files larger than adapter.policy.max_input_bytes, skipped for adapter
    // processing (input gate, 07 §7.1.2) but still archived.
    pub(crate) skipped_oversized_files: usize,
    // R22-4: binary inputs with no locally-extractable text whose media type is not an
    // OCR-able one (R21-4 skips their online task). Archived, but never enriched — surfaced
    // so `enriched_ratio: 1.0` cannot silently mean "this file does not exist".
    pub(crate) skipped_unrecognized_binary_files: usize,
}

/// Reuse only a HEAD-pinned local OCR instance whose profile is exactly the
/// local OCR profile selected by current configuration for this media type.
fn reusable_current_ocr_normalize(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    head: &BTreeMap<String, (String, NormalizeRef)>,
    path: &str,
    raw_hash: &str,
    media_type: &str,
) -> Result<Option<NormalizeRef>> {
    let Some((head_raw, normalize)) = head.get(path) else {
        return Ok(None);
    };
    if head_raw != raw_hash {
        return Ok(None);
    }
    let Some(execution) = active_local_ocr_execution_with_control(adapter_settings, repo.control())
        .filter(|_| local_ocr_handles_media_type(media_type))
    else {
        return Ok(None);
    };
    let selected = kio_adapter::local_ocr_markdownize::profile_for(execution);
    if normalize.tool_profile_hash != selected.tool_profile_hash {
        return Ok(None);
    }
    // A HEAD pointer is authority, not a best-effort cache. Reuse requires
    // a nonempty immutable closure whose every selected unit is durably Done;
    // a partial or failed local OCR run must remain pending for local drain.
    if !pinned_manifest_is_complete(repo, raw_hash, normalize)? {
        return Ok(None);
    }
    Ok(Some(normalize.clone()))
}

pub(crate) fn online_output_ref(adapter_id: &str) -> String {
    format!("online:{adapter_id}")
}

/// Stage 3: the enrichment task of a local-HTTP markdownize adapter.
///
/// The prefix is load-bearing, not cosmetic. `output_ref` is what the online
/// lane matches on (`targets_standard_online_markdownize`), so a local
/// pipeline's task keyed `online:` would be swept up by the ledger reservation
/// and batch sender. One prefix keeps local execution separate while its exact
/// device+scope approval is enforced at dispatch.
pub(crate) fn offline_output_ref(adapter_id: &str) -> String {
    format!("offline:{adapter_id}")
}

/// The status a freshly-enqueued `offline_api` markdownize task takes.
///
/// The budget branch has nothing to weigh — a local run has no price. Network
/// approval and `--offline` are evaluated at dispatch, where a missing grant
/// leaves this task Pending without constructing the local HTTP client. A Tier A
/// credential still needs `--send-secrets` before it is handed to a local model
/// server, which is a separate process that can log what it is given.
pub(crate) fn offline_markdownize_enqueue_status(
    secrets_hold: bool,
) -> (TaskStatus, Option<&'static str>) {
    if secrets_hold {
        (TaskStatus::Paused, Some(SECRETS_TIER_B_HOLD))
    } else {
        (TaskStatus::Pending, Some("ready_for_local_adapter"))
    }
}

pub(crate) fn explicitly_allowed_tier_a_paths(preview: &ScanPreview) -> BTreeSet<String> {
    preview
        .candidates
        .iter()
        .filter(|candidate| {
            !candidate.ignored
                && candidate.quarantine_reason.as_deref() == Some("secrets_tier_a_online_hold")
        })
        .map(|candidate| candidate.input_path.clone())
        .collect()
}

pub(crate) fn online_markdownize_profile() -> AdapterProfile {
    standard_online_markdownize_profile()
}

pub(crate) fn online_markdownize_profile_for(repo: &Repository) -> Result<AdapterProfile> {
    Ok(standard_online_markdownize_profile_with_bbox(
        bbox_annotation_enabled(repo)?,
    ))
}

#[cfg(debug_assertions)]
pub(crate) fn active_markdown_adapter(_repo: &Repository) -> Box<dyn MarkdownizeAdapter> {
    match markdownize_test_mode(_repo.control()) {
        Some(MarkdownizeTestMode::Incremental) => Box::new(TestIncrementalMarkdownizeAdapter {
            reject_incremental: false,
            reject_full: false,
        }),
        Some(MarkdownizeTestMode::RejectIncremental) => {
            Box::new(TestIncrementalMarkdownizeAdapter {
                reject_incremental: true,
                reject_full: false,
            })
        }
        Some(MarkdownizeTestMode::RejectIncrementalAndFull) => {
            Box::new(TestIncrementalMarkdownizeAdapter {
                reject_incremental: true,
                reject_full: true,
            })
        }
        _ => builtin_offline_markdownize_adapter(),
    }
}

#[cfg(not(debug_assertions))]
pub(crate) fn active_markdown_adapter(_repo: &Repository) -> Box<dyn MarkdownizeAdapter> {
    builtin_offline_markdownize_adapter()
}

#[cfg(debug_assertions)]
fn offline_markdownize_from_verified_bytes(
    adapter: &dyn MarkdownizeAdapter,
    request: MarkdownizeRequest,
    verified_raw_bytes: &[u8],
    control: &ControlHandle,
) -> kio_adapter::Result<kio_adapter::types::MarkdownizeResponse> {
    if markdownize_test_mode(control).is_some() {
        return adapter.markdownize(request);
    }
    // 07 §5.1: DOCX/PPTX get an offline baseline too, sourced from the SAME
    // converted-PDF page/slide text `write_prepared_objects` already verified
    // against Prepare's declared `prepared_hash` -- not the raw OOXML zip bytes
    // (which `markdownize_from_bytes`'s generic non-PDF fallback would otherwise
    // lossy-UTF8-decode as unsearchable garbage). This candidate only reaches
    // here with non-empty prepared units, i.e. a converter resolved for it at
    // Prepare time in this same `kio index` pass; absence here is the same rare
    // mid-run race `write_prepared_objects` already tolerates as a well-defined
    // (never crashing) failure rather than a silent corruption stand-in.
    if let Some(unit_kind) = office_unit_kind(&request.media_type) {
        let converted =
            convert_office_to_pdf_with_control(verified_raw_bytes, &request.media_type, control)?;
        return kio_adapter::deterministic::markdownize_converted_office(
            request, &converted, unit_kind,
        );
    }
    kio_adapter::deterministic::markdownize_from_bytes(request, verified_raw_bytes)
}

#[cfg(not(debug_assertions))]
fn offline_markdownize_from_verified_bytes(
    adapter: &dyn MarkdownizeAdapter,
    request: MarkdownizeRequest,
    verified_raw_bytes: &[u8],
    control: &ControlHandle,
) -> kio_adapter::Result<kio_adapter::types::MarkdownizeResponse> {
    let _ = adapter;
    if let Some(unit_kind) = office_unit_kind(&request.media_type) {
        let converted =
            convert_office_to_pdf_with_control(verified_raw_bytes, &request.media_type, control)?;
        return kio_adapter::deterministic::markdownize_converted_office(
            request, &converted, unit_kind,
        );
    }
    kio_adapter::deterministic::markdownize_from_bytes(request, verified_raw_bytes)
}

#[cfg(debug_assertions)]
#[derive(Clone, Copy)]
enum MarkdownizeTestMode {
    Incremental,
    RejectIncremental,
    RejectIncrementalAndFull,
}

#[cfg(debug_assertions)]
fn markdownize_test_mode(control: &ControlHandle) -> Option<MarkdownizeTestMode> {
    match control.debug().cli.markdownize_adapter.known() {
        Some(MarkdownizeAdapterMode::Incremental) => Some(MarkdownizeTestMode::Incremental),
        Some(MarkdownizeAdapterMode::RejectIncremental) => {
            Some(MarkdownizeTestMode::RejectIncremental)
        }
        Some(MarkdownizeAdapterMode::RejectIncrementalAndFull) => {
            Some(MarkdownizeTestMode::RejectIncrementalAndFull)
        }
        None => None,
    }
}

/// 07-adapter-spec.md §5.1: which `UnitKind` an Office media type's
/// converted-PDF units use -- DOCX enumerates `page:N` (the converted PDF's
/// pages themselves), PPTX enumerates `slide:N` (1 converted-PDF page = 1
/// slide). `None` for every other media type, including XLSX (the sheet
/// conversion mechanism is explicitly out of this addendum's scope, 07 §5.1
/// closing sentence).
fn office_unit_kind(media_type: &str) -> Option<UnitKind> {
    match media_type {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            Some(UnitKind::Page)
        }
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            Some(UnitKind::Slide)
        }
        _ => None,
    }
}

#[cfg(debug_assertions)]
#[derive(Debug, Clone)]
pub(crate) struct TestIncrementalMarkdownizeAdapter {
    reject_incremental: bool,
    reject_full: bool,
}

#[cfg(debug_assertions)]
impl MarkdownizeAdapter for TestIncrementalMarkdownizeAdapter {
    fn profile(&self) -> AdapterProfile {
        let profile = json!({
            "adapter_kind": "markdownize",
            "adapter_role": "text",
            "model_or_tool_family": "kio-test-incremental",
            "model_version_pin": "1.0.0",
            "output_schema": "kio-markdown-v1",
            "runtime_kind": "local",
            "spec_version": 1
        });
        AdapterProfile {
            adapter_kind: AdapterKind::Markdownize,
            adapter_id: "test_incremental_markdownize".to_owned(),
            execution_mode: ExecutionMode::DeterministicLibrary,
            tool_profile_hash: tool_profile_hash(&profile)
                .expect("built-in test profile should hash"),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            capability_flags: vec!["incremental_update".to_owned()],
            allow_network: false,
            // QA18: this test-seam adapter is local/offline, same posture as
            // the built-in deterministic adapter.
            billable_kinds: Vec::new(),
            reject_billing: Some(kio_adapter::types::BillingDeclaration::Nonbillable),
            // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): local/
            // offline test seam, never a sync provider call.
            provider_idempotency: kio_adapter::types::ProviderIdempotency::NotProvided,
        }
    }

    fn markdownize(
        &self,
        request: MarkdownizeRequest,
    ) -> kio_adapter::Result<kio_adapter::types::MarkdownizeResponse> {
        let hints = request.prepared_unit_hint.clone().unwrap_or_default();
        if request.mode == AdapterMarkdownizeMode::Incremental {
            let incremental =
                request
                    .hints
                    .clone()
                    .unwrap_or(kio_adapter::types::IncrementalHints {
                        changed_unit_keys: hints.iter().map(|hint| hint.unit_key.clone()).collect(),
                        added_unit_keys: Vec::new(),
                        removed_unit_keys: Vec::new(),
                        page_fingerprints: BTreeMap::new(),
                    });
            let changed = incremental.changed_unit_keys;
            let added = incremental.added_unit_keys;
            let unchanged = hints
                .iter()
                .filter(|hint| !changed.contains(&hint.unit_key) && !added.contains(&hint.unit_key))
                .map(|hint| hint.unit_key.clone())
                .collect::<Vec<_>>();
            return Ok(kio_adapter::types::MarkdownizeResponse {
                mode_used: AdapterMarkdownizeMode::Incremental,
                updated_units: hints
                    .iter()
                    .filter(|hint| changed.contains(&hint.unit_key))
                    .map(|hint| test_markdown_unit(hint, "incremental"))
                    .collect(),
                unchanged_unit_keys: if self.reject_incremental {
                    Vec::new()
                } else {
                    unchanged
                },
                added_units: hints
                    .iter()
                    .filter(|hint| added.contains(&hint.unit_key))
                    .map(|hint| test_markdown_unit(hint, "incremental-added"))
                    .collect(),
                removed_unit_keys: incremental.removed_unit_keys,
                failed_units: Vec::new(),
                fallback_to_full: false,
                reason: None,
                // QA17: this is a local test-seam adapter, never a billable
                // online call.
                usage: None,
            });
        }
        Ok(kio_adapter::types::MarkdownizeResponse {
            mode_used: AdapterMarkdownizeMode::Full,
            updated_units: if self.reject_full {
                Vec::new()
            } else {
                hints
                    .iter()
                    .map(|hint| test_markdown_unit(hint, "full"))
                    .collect()
            },
            unchanged_unit_keys: Vec::new(),
            added_units: Vec::new(),
            removed_unit_keys: Vec::new(),
            failed_units: Vec::new(),
            fallback_to_full: false,
            reason: None,
            // QA17: see above — never a billable online call.
            usage: None,
        })
    }
}

#[cfg(debug_assertions)]
pub(crate) fn test_markdown_unit(hint: &PreparedUnitHint, prefix: &str) -> MarkdownUnit {
    MarkdownUnit {
        unit_key: hint.unit_key.clone(),
        unit_type: hint.unit_kind,
        markdown: format!("{prefix} {}\n", hint.unit_key),
        owned_image_hashes: BTreeSet::new(),
        metadata: BTreeMap::new(),
    }
}

pub(crate) fn run_index_pipeline(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    preview: &ScanPreview,
    args: &IndexArgs,
) -> Result<IndexPipelineResult> {
    let task_store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    // Q3: index holds the folder store lock end-to-end, so any Running online task
    // is an orphan from a crashed run. Reclaim it to Pending before the enqueue /
    // dedup pass so it is re-counted as pending_online_tasks (and later re-sent by
    // `batch resume`) rather than being stuck forever in the Running absorbing
    // state — invisible to the pending counter and unrecoverable by any command.
    reclaim_orphaned_running_tasks(&task_store)?;
    let now = repo.control().now_utc_seconds();
    let scope_id = preview.scope_id.clone();
    let prepare_profile_hash = builtin_prepare_profile().tool_profile_hash;
    let markdown_adapter = active_markdown_adapter(repo);
    let markdown_profile = markdown_adapter.profile();
    let markdown_profile_hash = markdown_profile.tool_profile_hash.clone();
    let network_allowed = network_allowed(adapter_settings, repo, args)?;
    let ledger = if args.offline {
        None
    } else {
        open_optional_ledger_db(repo.control())?
    };
    let budget_caps = read_budget_policy(
        user_config_toml_path(repo.control()),
        repo.kio_dir().join("config.toml"),
    )
    .map_err(pipeline_to_kio)?;

    // An offline invocation intentionally has no mutable ledger handle. Capture
    // the budget separately so that this is not mistaken for an uninitialized
    // ledger when classifying new online tasks.
    let enqueue_budget = if let Some(ledger) = ledger.as_ref() {
        let (device, folder) =
            budget_remaining_for_adapter(ledger, &budget_caps, &scope_id, "markdownize")?;
        EnqueueBudget::Available { device, folder }
    } else if args.offline {
        let path = ledger_db_path(repo.control());
        let (month, snapshot) = capture_search_budget_observation(&path, repo.control())?;
        if let Some(snapshot) = snapshot {
            let total = |scope: Option<&str>, adapter: Option<&str>| {
                snapshot
                    .month_total(scope, adapter, &month)
                    .map_err(|error| ledger_snapshot_to_kio(&path.display().to_string(), error))
            };
            let (device, folder) = budget_remaining_from_totals(
                &budget_caps,
                "markdownize",
                total(None, None)?,
                total(Some(&scope_id), None)?,
                total(None, Some("markdownize"))?,
            );
            EnqueueBudget::Available { device, folder }
        } else {
            EnqueueBudget::Uninitialized
        }
    } else {
        EnqueueBudget::Uninitialized
    };

    // R18-2: release the ledger reservation of DELETED / renamed files' orphaned
    // tasks. R17-3's enqueue supersede (`enqueue_online_placeholder_task`) releases
    // a stale task's reservation only when the SAME path is re-scanned
    // (`task.input_path == candidate.input_path`); a DELETED or renamed file never
    // reappears as a scan candidate, so its still-open ledger row would eat the
    // per-adapter markdownize cap for the rest of the month and falsely pause
    // unrelated tasks (the same harm R17-3 fixed, surviving on the delete path).
    // With the full live-candidate set in hand, retire + release any
    // reservation-bearing online markdownize task whose input_path is no longer a
    // live candidate. Runs before the enqueue loop so the freed cap is available to
    // this index's tasks.
    //
    // QA2/QA3 (step4b-contract-tests-p3a.md §A, 04 §5.2/§5.3): a
    // reservation-bearing send failure no longer always lands `Failed` —
    // rate_limit stays `Pending` and auth_error lands `Paused`. Admit both
    // statuses here too (`is_reservation_bearing_send_failure` already scopes
    // this to the 4 kinds that actually carry an open reservation, so a fresh
    // never-attempted Pending task or a budget/secrets Paused hold — neither
    // reservation-bearing — is unaffected).
    if let Some(ledger) = ledger.as_ref() {
        let online_profile = online_markdownize_profile_for(repo)?;
        let placeholder_output_ref = online_output_ref(&online_profile.adapter_id);
        let reservation_scope_id = repo.scope_identity()?.scope_id;
        let live_paths: BTreeSet<&str> = preview
            .candidates
            .iter()
            .filter(|candidate| !candidate.ignored && candidate.media_type != "inode/directory")
            .map(|candidate| candidate.input_path.as_str())
            .collect();
        let mut orphan_ids = BTreeSet::new();
        for task in task_store.all().map_err(pipeline_to_kio)? {
            let orphaned = task.task_type == TaskType::Markdownize
                && targets_standard_online_markdownize(
                    task_store.kio_dir(),
                    &task,
                    &placeholder_output_ref,
                )
                && matches!(
                    task.status,
                    TaskStatus::Failed | TaskStatus::Pending | TaskStatus::Paused
                )
                && is_reservation_bearing_send_failure(&task)
                && !live_paths.contains(task.input_path.as_str());
            if !orphaned {
                continue;
            }
            let key = task_ledger_key(
                &reservation_scope_id,
                "markdownize",
                &task.input_hash,
                &online_profile.tool_profile_hash,
            );
            release_task_charge_if_open(ledger, &key)?;
            orphan_ids.insert(task.task_id);
        }
        task_store
            .update_matching(|task| {
                if !orphan_ids.contains(&task.task_id) {
                    return false;
                }
                retire_online_task(task);
                true
            })
            .map_err(pipeline_to_kio)?;
    }

    let mut result = IndexPipelineResult {
        network_allowed,
        ..IndexPipelineResult::default()
    };
    let unsupported_store = UnsupportedInputStore::new(repo.kio_dir());
    let mut unsupported_by_path = unsupported_store
        .latest_by_path()
        .map_err(pipeline_to_kio)?
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut unsupported_paths = unsupported_by_path
        .values()
        .filter(|entry| entry.reason == UNSUPPORTED_REASON_UNRECOGNIZED_BINARY)
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    // 07 §5.1: mirrors `unsupported_paths` above, scoped to the
    // office-conversion-unavailable reason so `enqueue_online_placeholder_task`
    // can record the resolved-transition once a converter becomes available.
    let mut office_conversion_unavailable_paths = unsupported_by_path
        .values()
        .filter(|entry| entry.reason == OFFICE_CONVERSION_UNAVAILABLE_REASON)
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    // R12-2: the documented `adapter.policy.max_input_bytes` input gate (07 §7.1.2 —
    // "Kio 側の入力制御" is an MVP contract). Scope config wins over user config,
    // default 100 MB. A file larger than the cap is never handed to the Markdownize
    // adapter (below); it stays archived but unenriched, and the count is disclosed.
    let max_input_bytes = effective_max_input_bytes(repo);
    // R12-1: the documented `[markdownize.incremental]` overrides (were hardcoded).
    let incremental_config = effective_incremental_config(repo)?;
    let head_ocr_normalizes = repo
        .head_commit_hash()?
        .map(|head| read_head_tree_for_rebuild(repo, &head))
        .transpose()?
        .map(|tree| {
            tree.entries
                .into_iter()
                .filter_map(|entry| {
                    entry
                        .normalize
                        .map(|normalize| (entry.path, (entry.raw_hash, normalize)))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    for candidate in preview
        .candidates
        .iter()
        .filter(|candidate| !candidate.ignored && candidate.media_type != "inode/directory")
    {
        if candidate.size_bytes > max_input_bytes {
            result.skipped_oversized_files += 1;
            repo.control().append_event_log_in_session(
                repo.lock_session(),
                "KIO-I-INDEX-INPUT-OVERSIZED-001",
                "input file exceeds adapter.policy.max_input_bytes; skipped adapter processing",
                json!({
                    "size_bytes": candidate.size_bytes,
                    "max_input_bytes": max_input_bytes,
                }),
            )?;
            continue;
        }
        let path = repository_input_display_path(repo, &candidate.input_path);
        if !repository_scan_policy_allows_file(repo, &candidate.input_path)
            .map_err(pipeline_to_kio)?
        {
            repo.control().append_event_log_in_session(
                repo.lock_session(),
                "KIO-I-INDEX-INPUT-POLICY-CHANGED-001",
                "input is no longer authorized by current scope policy; skipped",
                json!({ "input_path": candidate.input_path }),
            )?;
            result.failed_files += 1;
            continue;
        }
        let verified =
            match read_repository_scan_input(repo, &candidate.input_path, max_input_bytes) {
                Ok(verified) => verified,
                Err(error) => {
                    repo.control().append_event_log_in_session(
                        repo.lock_session(),
                        "KIO-I-INDEX-INPUT-CHANGED-001",
                        "input could not be rebound to a regular bounded scope child; skipped",
                        json!({ "input_path": candidate.input_path }),
                    )?;
                    let _ = error;
                    result.failed_files += 1;
                    continue;
                }
            };
        // R15-8: `candidate.raw_hash` was computed at SCAN time; `bytes` is read HERE. If
        // the file was externally edited in that window, persisting the normalized
        // instance under the stale SCAN hash while the closing snapshot hashes the CURRENT
        // bytes for the tree entry would diverge their identities (the offline twin of
        // R14-2). Re-hash what we actually read and, on a mismatch, SKIP this candidate —
        // the next clean index re-scans and normalizes it under its correct identity. The
        // window is one store-lock-held `kio index` plus an external write, so this is a
        // rare defensive guard. (No deterministic test: reproducing it needs an external
        // edit landing inside the lock-held critical section between the scan and this
        // read, which no public seam exposes.)
        let current_hash = verified.raw_hash.clone();
        if let Some(scan_hash) = &candidate.raw_hash
            && scan_hash != &current_hash
        {
            repo.control().append_event_log_in_session(
                repo.lock_session(),
                "KIO-I-INDEX-INPUT-CHANGED-001",
                "input file changed between scan and normalize; skipped to preserve \
                     content-addressing (re-run index)",
                json!({ "input_path": candidate.input_path }),
            )?;
            result.failed_files += 1;
            continue;
        }
        let raw_hash = current_hash;
        // Reject before Prepare/Markdownize writes any derived state. The closing
        // snapshot independently rechecks every staged raw under the store lock.
        ensure_raw_ingest_allowed(repo, &raw_hash)?;
        let prepare = prepare_units_from_bytes_with_control(
            PrepareStageBytesRequest {
                raw_hash: &raw_hash,
                media_type: &candidate.media_type,
                input_path: &path.display().to_string(),
                tool_profile_hash: &prepare_profile_hash,
                bytes: &verified.bytes,
            },
            repo.control(),
        )
        .map_err(pipeline_to_kio)?;

        write_prepared_objects(
            repo,
            &prepare.prepared_units,
            &prepare.prepared_object_hashes,
            &verified.bytes,
            &candidate.media_type,
        )?;

        if let Some(normalize) = reusable_current_ocr_normalize(
            adapter_settings,
            repo,
            &head_ocr_normalizes,
            &candidate.input_path,
            &raw_hash,
            &candidate.media_type,
        )? {
            result.normalize_by_path.insert(
                candidate.input_path.clone(),
                PendingNormalizeRef {
                    expected_raw_hash: raw_hash.clone(),
                    normalize,
                },
            );
            result.normalized_files += 1;
            continue;
        }

        // Standalone raster images are locally indexable as immutable image
        // objects. Their normalized unit contains only a kio:// image reference,
        // so no binary bytes are treated as Markdown and local image-vector
        // enrichment can discover the pixels through the ordinary chunk path.
        if matches!(
            candidate.media_type.as_str(),
            "image/png" | "image/jpeg" | "image/webp"
        ) {
            let native_profile_hash = image_ingest::native_image_profile_hash();
            let native_instance = kio_pipeline::markdownize::normalized_instance_dir(
                repo.kio_dir(),
                &raw_hash,
                &native_profile_hash,
                0,
            );
            let normalize = if native_instance.is_dir() {
                // A same raw/profile/gen instance is immutable authority. Reuse
                // only after the normal validated loader rebinds its manifest.
                NormalizeRef {
                    tool_profile_hash: native_profile_hash,
                    r#gen: 0,
                    manifest_hash: compute_manifest_hash(
                        repo,
                        &raw_hash,
                        &image_ingest::native_image_profile_hash(),
                        0,
                    )?,
                }
            } else {
                image_ingest::ingest_standalone_image(
                    repo,
                    &candidate.input_path,
                    &raw_hash,
                    &verified.bytes,
                    &candidate.media_type,
                    &prepare_profile_hash,
                    &markdown_profile_hash,
                )?
                .normalize
            };
            result.normalize_by_path.insert(
                candidate.input_path.clone(),
                PendingNormalizeRef {
                    expected_raw_hash: raw_hash.clone(),
                    normalize,
                },
            );
            result.normalized_files += 1;
            // Native image ingestion establishes the pixel authority locally;
            // OCR remains an optional later enrichment and has its own online
            // task/output identity, so it cannot replace this image object.
            enqueue_online_placeholder_task(
                adapter_settings,
                repo,
                &task_store,
                candidate,
                &raw_hash,
                &scope_id,
                network_allowed,
                args,
                &now,
                &mut result,
                ledger.as_ref(),
                enqueue_budget,
                &budget_caps,
                &unsupported_store,
                &mut unsupported_by_path,
                &mut office_conversion_unavailable_paths,
            )?;
            continue;
        }

        if prepare.prepared_units.is_empty() {
            // R20-5: this media has no locally-extractable text — a text-layer-less/scanned
            // PDF, or (after R20-6) an image / OOXML / unrecognized binary. It needs ONLINE
            // OCR, so route it through the SAME enqueue path as a text-layer PDF's online
            // enhancement: an idempotent task carrying the executable online output_ref,
            // gated by secret / budget / network opt-in. The old branch appended a fixed
            // "pending:scanned_pdf_without_text_layer" output_ref that NO executor consumed
            // (`execute_pending_markdownize_tasks` filters on the online output_ref) and had
            // no idempotency guard (04 §5.5), so it silently duplicated on every idle
            // re-index and the file never reached OCR.
            //
            // R21-4: only a RECOGNIZED OCR-able medium (PDF / image / OOXML — docs/07 §5.2)
            // is worth an online OCR task. An unrecognized `application/octet-stream` binary
            // (a .sqlite, a .zip, a compiled blob) is not a document and can never be OCR'd,
            // so enqueuing one only pollutes pending enrichment forever. Skip it.
            if candidate.media_type != "application/octet-stream" {
                if unsupported_paths.remove(&candidate.input_path) {
                    record_unsupported_if_changed(
                        &unsupported_store,
                        &mut unsupported_by_path,
                        UnsupportedInputDisposition {
                            path: candidate.input_path.clone(),
                            raw_hash: raw_hash.clone(),
                            media_type: candidate.media_type.clone(),
                            size_bytes: verified.size_bytes,
                            reason: UNSUPPORTED_REASON_RESOLVED.to_owned(),
                        },
                    )?;
                }
                enqueue_online_placeholder_task(
                    adapter_settings,
                    repo,
                    &task_store,
                    candidate,
                    &raw_hash,
                    &scope_id,
                    network_allowed,
                    args,
                    &now,
                    &mut result,
                    ledger.as_ref(),
                    enqueue_budget,
                    &budget_caps,
                    &unsupported_store,
                    &mut unsupported_by_path,
                    &mut office_conversion_unavailable_paths,
                )?;
            } else {
                // R22-4: the R21-4 skip left NO trace — no task, no counter, no event — so a
                // real document whose extension is merely missing from the MIME table (a
                // `.bmp` / `.tiff` / `.heic` image, a legacy `.doc` / `.xls`, an `.epub`)
                // vanished from enrichment while `index_status` still reported
                // `enriched_ratio: 1.0` and `kio status` showed the file as `unchanged`. The
                // bytes are archived, but nothing is searchable and no recovery command has
                // anything to act on. Disclose it exactly the way the sibling oversized-input
                // gate does: a counter plus an INFO event (no new error code — docs frozen).
                result.skipped_unrecognized_binary_files += 1;
                record_unsupported_if_changed(
                    &unsupported_store,
                    &mut unsupported_by_path,
                    UnsupportedInputDisposition {
                        path: candidate.input_path.clone(),
                        raw_hash: raw_hash.clone(),
                        media_type: candidate.media_type.clone(),
                        size_bytes: verified.size_bytes,
                        reason: UNSUPPORTED_REASON_UNRECOGNIZED_BINARY.to_owned(),
                    },
                )?;
                unsupported_paths.insert(candidate.input_path.clone());
                repo.control().append_event_log_in_session(repo.lock_session(),
                    "KIO-I-INDEX-INPUT-UNRECOGNIZED-BINARY-001",
                    "binary input has no locally-extractable text and no OCR-able media type; archived but not enriched",
                    json!({
                        "media_type": candidate.media_type,
                        "size_bytes": candidate.size_bytes,
                    }),
                )?;
            }
            continue;
        }

        if unsupported_paths.remove(&candidate.input_path) {
            record_unsupported_if_changed(
                &unsupported_store,
                &mut unsupported_by_path,
                UnsupportedInputDisposition {
                    path: candidate.input_path.clone(),
                    raw_hash: raw_hash.clone(),
                    media_type: candidate.media_type.clone(),
                    size_bytes: verified.size_bytes,
                    reason: UNSUPPORTED_REASON_RESOLVED.to_owned(),
                },
            )?;
        }

        let output_ref = normalized_output_ref(repo, &raw_hash, &markdown_profile_hash, 0);
        if task_store
            .done_output_for(&raw_hash, &output_ref)
            .map_err(pipeline_to_kio)?
            .is_some()
        {
            // QB41 (03 §2.1's second legal gen+1 path, 07 §5.1): raw_hash is
            // UNCHANGED (a done/partial gen-0 instance already exists for it),
            // but `prepare` above (already computed fresh THIS pass, for THIS
            // candidate -- no extra corpus scan) may still disagree with that
            // instance's per-unit `prepared_hash`: a prepare profile /
            // renderer version change (the Office converted-PDF path is the
            // motivating case) can change `prepared_hash` without changing
            // raw_hash at all. Detect and gate it BEFORE the unconditional
            // reuse below would otherwise silently keep serving the STALE
            // generation forever.
            if let Some(new_gen) = prepared_hash_drift_new_gen(
                repo.kio_dir(),
                &raw_hash,
                &markdown_profile_hash,
                &prepare.prepared_units,
                repo.control(),
            ) {
                // Mirrors `kio reindex --force`'s confirmation gate exactly
                // (same error code / exit code / `--yes` flag) -- this
                // codebase has no real interactive (TTY) confirmation prompt
                // anywhere yet (`run_index`'s own top-level approval flow
                // says so explicitly), so non-interactive `--yes` is the only
                // accepted consent, matching that existing precedent rather
                // than inventing a new interaction model here.
                if !args.yes {
                    return Err(KioError::new(
                        "KIO-E-CONFIRM-REJECTED-001",
                        "prepared_hash changed for an unchanged raw_hash (prepare profile / \
                         renderer drift); re-markdownize at a new generation requires \
                         confirmation -- pass --yes in non-interactive mode",
                        json!({
                            "input_path": candidate.input_path,
                            "new_gen": new_gen,
                        }),
                        ExitCode::ConfirmationRejected,
                    ));
                }
                create_prepared_hash_drift_instance(
                    repo,
                    &task_store,
                    markdown_adapter.as_ref(),
                    candidate,
                    &path,
                    &raw_hash,
                    &markdown_profile_hash,
                    &prepare.prepared_units,
                    &verified.bytes,
                    new_gen,
                    &now,
                    &mut result,
                )?;
                continue;
            }
            // PB04: this candidate's gen=0 output is already `done`
            // (verified above), so its manifest.json is hashable now.
            let manifest_hash = compute_manifest_hash(repo, &raw_hash, &markdown_profile_hash, 0)?;
            result.normalize_by_path.insert(
                candidate.input_path.clone(),
                PendingNormalizeRef {
                    expected_raw_hash: raw_hash.clone(),
                    normalize: NormalizeRef {
                        tool_profile_hash: markdown_profile_hash.clone(),
                        r#gen: 0,
                        manifest_hash,
                    },
                },
            );
            result.normalized_files += 1;
            // R21-4: an online OCR "enhancement" task after a SUCCESSFUL local markdownize
            // is only meaningful for a real text-layer PDF (docs/07 §8) or, per 07 §5.1, an
            // Office file whose converter resolved (the enqueue gate itself decides whether
            // to actually enqueue -- item 1's `enqueue_online_placeholder_task` early-return).
            // A non-`.md`/non-code TEXT file folded to `application/octet-stream` (a `.yaml` /
            // `.json` / `Dockerfile`, or an uppercase-extension text-native file) is already
            // fully and finally handled by the local passthrough — enqueuing an OCR task
            // shipped its bytes to the external API under `--online` (R9-2 routing violation)
            // and, when offline, left a permanent phantom pending that deflated enriched_ratio.
            if candidate.media_type == "application/pdf" || is_office_media(&candidate.media_type) {
                enqueue_online_placeholder_task(
                    adapter_settings,
                    repo,
                    &task_store,
                    candidate,
                    &raw_hash,
                    &scope_id,
                    network_allowed,
                    args,
                    &now,
                    &mut result,
                    ledger.as_ref(),
                    enqueue_budget,
                    &budget_caps,
                    &unsupported_store,
                    &mut unsupported_by_path,
                    &mut office_conversion_unavailable_paths,
                )?;
            }
            continue;
        }

        // R14-1 (defense in depth): loading the prior instance for THIS candidate must
        // not abort the whole index and silently skip alphabetically-later files. On any
        // error (a store read fault, an unreadable prior instance the `Ok(None)` fix
        // above did not already absorb), degrade to no-previous = a Full re-send for this
        // candidate only, keeping the blast radius to the one file.
        let previous =
            previous_instance_for_path(&task_store, &candidate.input_path, &markdown_profile_hash)
                .unwrap_or_else(|_err| {
                    let _ = repo.control().append_event_log_in_session(
                        repo.lock_session(),
                        "KIO-I-INDEX-PREVIOUS-UNREADABLE-001",
                        "prior normalized instance unreadable; indexing this file as Full",
                        json!({ "input_path": candidate.input_path }),
                    );
                    None
                });
        let mapping = if incremental_config.enabled {
            previous
                .as_ref()
                .map(|previous| map_units(&previous.prepared_units, &prepare.prepared_units))
        } else {
            None
        };
        let incremental_hints = mapping
            .as_ref()
            .map(|mapping| incremental_hints_from_mapping(mapping, &prepare.prepared_units))
            .unwrap_or_else(|| all_changed_hints(&prepare.prepared_units));
        // R12-1: `enabled = false` disables incremental entirely — always full mode
        // (05 / docs/10:537). Otherwise the effective threshold / max_consecutive
        // (were hardcoded 0.30 / 5) drive the documented decision.
        let mode_decision = if incremental_config.enabled {
            choose_markdownize_mode(&IncrementalModeInput {
                has_previous_done_run: previous.is_some(),
                raw_hash_only_changed: true,
                adapter_capabilities: markdown_profile.capability_flags.clone(),
                change_rate: mapping
                    .as_ref()
                    .map(|mapping| mapping.change_rate)
                    .unwrap_or(1.0),
                threshold: incremental_config.threshold,
                consecutive_incremental_count: consecutive_incremental_count(
                    &task_store,
                    &candidate.input_path,
                )?,
                max_consecutive_incremental: incremental_config.max_consecutive,
            })
        } else {
            IncrementalModeDecision {
                mode: MarkdownizeMode::Full,
                reason: Some("incremental_disabled".to_owned()),
            }
        };
        let mode = mode_decision.mode;
        let hints = prepared_unit_hints(&prepare.prepared_units);
        let adapter_previous = previous
            .as_ref()
            .map(|previous| PreviousMarkdownizeContext {
                raw: RawInput {
                    raw_hash: previous.manifest.raw_hash.clone(),
                    path: Some(
                        repo.canonical_root()
                            .join(&candidate.input_path)
                            .display()
                            .to_string(),
                    ),
                },
                normalized_units: previous
                    .units
                    .iter()
                    .map(normalized_unit_to_adapter_unit)
                    .collect(),
                tool_profile_hash: previous.manifest.tool_profile_hash.clone(),
            });
        let request = MarkdownizeRequest {
            raw: RawInput {
                raw_hash: raw_hash.clone(),
                path: Some(path.display().to_string()),
            },
            media_type: candidate.media_type.clone(),
            prepared_unit_hint: Some(hints),
            mode: adapter_mode(mode),
            previous: (mode == MarkdownizeMode::Incremental)
                .then_some(adapter_previous)
                .flatten(),
            hints: (mode == MarkdownizeMode::Incremental)
                .then(|| adapter_hints(&incremental_hints)),
            // Offline (deterministic) markdownize path: the builtin adapter ignores
            // page scoping (R15-5 concerns only the real Mistral client).
            restrict_to_hint_pages: false,
            bbox_annotation_enabled: false,
            tool_profile_hash: markdown_profile_hash.clone(),
            spec_version: 1,
            // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): this is
            // the free/local (no ledger charge) offline Markdownize path —
            // there is no `intent_token` to carry.
            idempotency_token: None,
        };

        let mut response = offline_markdownize_from_verified_bytes(
            markdown_adapter.as_ref(),
            request.clone(),
            &verified.bytes,
            repo.control(),
        )
        .map_err(adapter_to_kio)?;
        let mut final_mode = mode;
        let mut fallback_reason = mode_decision.reason.clone();
        let validation = if response.fallback_to_full {
            Err(kio_pipeline::PipelineError::contract(
                "KIO-E-ADAPTER-CONTRACT-001",
                "adapter_requested_full_fallback",
            ))
        } else {
            validate_markdownize_response(&response, &incremental_hints, &prepare.prepared_units)
        };
        if let Err(error) = validation {
            if mode != MarkdownizeMode::Incremental {
                append_failed_markdownize_task(
                    repo,
                    &task_store,
                    candidate,
                    &raw_hash,
                    &output_ref,
                    "contract_violation",
                    &now,
                )?;
                result.failed_files += 1;
                // The validation verdict used to be silently discarded here,
                // which made every such failure a blind "contract_violation"
                // task (2026-07-23 fixture registration: 48 opaque failures
                // whose real cause — unfenced raw HTML in octet-text
                // passthrough — took a live probe to recover). Keep the
                // task-record shape, but preserve the actual reason in the
                // event log.
                repo.control().append_event_log_in_session(repo.lock_session(),
                    "KIO-E-ADAPTER-CONTRACT-001",
                    "offline markdownize response failed acceptance; task recorded as contract_violation",
                    json!({
                        "input_path": candidate.input_path,
                        "detail": error.to_string(),
                    }),
                )?;
                continue;
            }
            let mut full_request = request;
            full_request.mode = AdapterMarkdownizeMode::Full;
            full_request.previous = None;
            full_request.hints = None;
            let fallback_response = offline_markdownize_from_verified_bytes(
                markdown_adapter.as_ref(),
                full_request,
                &verified.bytes,
                repo.control(),
            )
            .map_err(adapter_to_kio)
            .and_then(|response| {
                validate_markdownize_response(
                    &response,
                    &incremental_hints,
                    &prepare.prepared_units,
                )
                .map_err(pipeline_to_kio)?;
                Ok(response)
            });
            let Ok(fallback_response) = fallback_response else {
                append_failed_markdownize_task(
                    repo,
                    &task_store,
                    candidate,
                    &raw_hash,
                    &output_ref,
                    "full_fallback_failed",
                    &now,
                )?;
                result.failed_files += 1;
                continue;
            };
            response = fallback_response;
            final_mode = MarkdownizeMode::Full;
            fallback_reason = Some("full_fallback_after_incremental_reject".to_owned());
        }

        let generated_at = repo.control().now_utc_seconds();
        let run_id = format!("run_{}", new_ulid(repo.canonical_root()));
        let units = normalized_units_from_response(
            repo,
            &candidate.media_type,
            &verified.bytes,
            &response,
            &prepare.prepared_units,
            previous.as_ref(),
            &raw_hash,
            &markdown_profile_hash,
            0,
            final_mode,
            &generated_at,
        )
        .map_err(NormalizedUnitsError::into_kio)?;
        let manifest = manifest_from_units(
            &prepare.prepared_units,
            &units,
            &raw_hash,
            &markdown_profile_hash,
            0,
            previous.as_ref().map(|previous| previous.manifest.r#gen),
            &run_id,
            &generated_at,
            // The local deterministic markdownize always returns every unit, so this
            // kind is inert here; a hypothetical missing unit would be a
            // non-retryable library/contract fault rather than a transient.
            RetryErrorKind::ContractViolation,
        );
        let stamped_manifest = persist_normalized_instance_with_control(
            repo.kio_dir(),
            &manifest,
            &units,
            repo.control(),
        )
        .map_err(pipeline_to_kio)?;
        // The returned manifest includes the immutable normalized-unit CAS
        // pins that the writer just committed; derive the tree pin from those
        // exact bytes rather than the pre-persist nullable manifest.
        let manifest_hash = hash_and_write_manifest_object(repo, &stamped_manifest)?;
        let task = task_descriptor(
            repo,
            TaskType::Markdownize,
            Some(mode),
            candidate,
            &raw_hash,
            &output_ref,
            TaskStatus::Done,
            fallback_reason.as_deref(),
            &generated_at,
        );
        let mut task = task;
        task.mode = Some(final_mode);
        task.previous_raw_hash = previous
            .as_ref()
            .map(|previous| previous.manifest.raw_hash.clone());
        task.parent_run_id = previous
            .as_ref()
            .map(|previous| previous.manifest.run_id.clone());
        task.changed_unit_keys = incremental_hints.changed_unit_keys.clone();
        task.unit_keys = Some(
            prepare
                .prepared_units
                .iter()
                .map(|unit| unit.unit_key.clone())
                .collect(),
        );
        task_store.append(&task).map_err(pipeline_to_kio)?;
        result.normalize_by_path.insert(
            candidate.input_path.clone(),
            PendingNormalizeRef {
                expected_raw_hash: raw_hash.clone(),
                normalize: NormalizeRef {
                    tool_profile_hash: markdown_profile_hash.clone(),
                    r#gen: 0,
                    manifest_hash,
                },
            },
        );
        result.normalized_files += 1;
        // R21-4: only a real text-layer PDF, or (07 §5.1) an Office file whose converter
        // resolved, warrants an online OCR "enhancement" task after a successful local
        // markdownize -- the enqueue gate itself (item 1) decides whether to actually
        // enqueue an Office file. A TEXT file folded to `application/octet-stream`
        // (`.yaml`/`.json`/`Dockerfile`/uppercase-extension text-native) is fully handled
        // locally; enqueuing OCR sent its bytes online (R9-2 violation) or left an offline
        // phantom pending.
        if candidate.media_type == "application/pdf" || is_office_media(&candidate.media_type) {
            enqueue_online_placeholder_task(
                adapter_settings,
                repo,
                &task_store,
                candidate,
                &raw_hash,
                &scope_id,
                network_allowed,
                args,
                &now,
                &mut result,
                ledger.as_ref(),
                enqueue_budget,
                &budget_caps,
                &unsupported_store,
                &mut unsupported_by_path,
                &mut office_conversion_unavailable_paths,
            )?;
        }
    }
    // Stage 3: drain local-HTTP OCR only when this invocation is not offline and
    // the active local peer has an exact device+scope network grant. The task is
    // created first and drained second, so a crash mid-OCR leaves a Pending task
    // that the next authorized index invocation can pick up.
    let executed = execute_pending_offline_markdownize_tasks(
        adapter_settings,
        repo,
        &task_store,
        &mut result.normalize_by_path,
        network_allowed,
    )?;
    result.pending_online_tasks = result.pending_online_tasks.saturating_sub(executed);
    Ok(result)
}

/// QB41 (03 §2.1's second legal gen+1 path, 07 §5.1): the newest existing
/// normalized instance for `(raw_hash, tool_profile_hash)`, found by a bounded
/// linear probe from gen 0 upward. Mirrors `kio reindex --force`'s own gen-walk
/// tolerance (`copy_normalized_instance_gen`'s caller, `run_reindex`): a
/// missing OR unreadable gen N is treated as "N-1 was the latest", the same
/// forward-degradation this codebase already accepts elsewhere for a
/// corrupt/partial gen (R17-2's doc comment on the analogous reindex loop).
fn latest_normalized_instance(
    kio_dir: &Path,
    raw_hash: &str,
    tool_profile_hash: &str,
    control: &ControlHandle,
) -> kio_pipeline::Result<ValidatedNormalizedInstance> {
    // Gen numbers are always small in practice (only `kio reindex --force` and
    // this QB41 path ever create gen > 0) -- this bound only protects against a
    // pathological/adversarial store, not a real workload.
    const MAX_GEN_PROBE: u64 = 4096;
    let mut latest = load_validated_normalized_instance_with_control(
        kio_dir,
        raw_hash,
        tool_profile_hash,
        0,
        control,
    )?;
    for r#gen in 1..MAX_GEN_PROBE {
        match load_validated_normalized_instance_with_control(
            kio_dir,
            raw_hash,
            tool_profile_hash,
            r#gen,
            control,
        ) {
            Ok(instance) => latest = instance,
            Err(_) => break,
        }
    }
    Ok(latest)
}

/// QB41: `Some(new_gen)` when the LATEST existing normalized instance's
/// per-unit prepared identity disagrees with `fresh_prepared_units` (the
/// SAME-pass, already-computed fresh Prepare output for this candidate -- no
/// extra corpus scan) -- a prepare profile / renderer version change drove a
/// prepared identity change without any `raw_hash` change at all (03 §2.1:
/// "`gen = 現最大 + 1` の新 instance を作れるのは `kio reindex --force` と、prepare
/// profile / renderer 変更による `prepared_hash` 変化が駆動する再 Markdownize...
/// だけ"; 07 §5.1's Office converted-PDF renderer-drift is the motivating
/// case). `None` when they agree (the ordinary reuse path is unaffected) OR
/// when the existing instance cannot be loaded at all (graceful degrade --
/// this detector must never be a NEW source of a hard `kio index` failure for
/// a corruption it did not cause; the existing reuse/rebuild paths already
/// handle that case on their own terms).
pub(crate) fn prepared_hash_drift_new_gen(
    kio_dir: &Path,
    raw_hash: &str,
    tool_profile_hash: &str,
    fresh_prepared_units: &[PreparedUnit],
    control: &ControlHandle,
) -> Option<u64> {
    // Guard (a) — defense in depth: an EMPTY fresh prepare is never a drift
    // verdict. The local extractor legitimately yields zero units for content
    // it cannot read (R20-4 garbage-gates every FlateDecode-compressed
    // real-world PDF, including real soffice-converted Office output) while a
    // done ONLINE instance discovered its units from the OCR response —
    // comparing those would flag such files as "drifted" on every `kio index`
    // (exit 9 without --yes; with --yes an offline gen+1 built from unreadable
    // input would shadow the OCR content as the latest generation). TODAY this
    // state cannot reach the caller at all — run_index_pipeline's R20-5
    // empty-prepare arm peels these candidates off into the online-enqueue
    // path before the done-instance branch (verified by revert-testing
    // office_07 with this guard disabled) — but that reachability is one
    // refactor (or a FlateDecode-capable extractor changing what "empty"
    // means) away from silently flipping, so the invariant is enforced here
    // where the verdict is minted, not assumed from caller topology.
    if fresh_prepared_units.is_empty() {
        return None;
    }
    let latest = latest_normalized_instance(kio_dir, raw_hash, tool_profile_hash, control).ok()?;
    let existing: BTreeMap<&str, (&str, &str)> = latest
        .manifest
        .units
        .iter()
        .map(|unit| {
            (
                unit.unit_key.as_str(),
                (
                    unit.prepared_hash.as_str(),
                    unit.preparation_profile_hash.as_str(),
                ),
            )
        })
        .collect();
    let fresh: BTreeMap<&str, (&str, &str)> = fresh_prepared_units
        .iter()
        .map(|unit| {
            (
                unit.unit_key.as_str(),
                (
                    unit.prepared_hash.as_str(),
                    unit.preparation_profile_hash.as_str(),
                ),
            )
        })
        .collect();
    let existing_profiles = existing
        .values()
        .map(|(_, profile)| *profile)
        .collect::<BTreeSet<_>>();
    let fresh_profiles = fresh
        .values()
        .map(|(_, profile)| *profile)
        .collect::<BTreeSet<_>>();
    let preparation_profile_changed = existing_profiles != fresh_profiles;
    // Guard (b): an identity disagreement on an IDENTICAL unit-key set is a
    // safe renderer-drift verdict. A key-set difference without a profile
    // change is indistinguishable
    // from an OCR-discovered manifest whose unit set never came from the local
    // prepare basis (OCR-from-scratch discovery mints placeholder prepared
    // hashes for pages the extractor never produced) — treating that as drift
    // would re-prompt on every index instead of once. With the current
    // hint-echoing adapters a key-set mismatch is not end-to-end constructible
    // (hints round-trip verbatim; OCR-from-scratch implies the empty-prepare
    // arm), so like guard (a) this enforces the invariant at the mint rather
    // than trusting adapter topology to keep it true. A renderer update that
    // changes pagination therefore stays `kio reindex --force` territory
    // (documented residual, step4b backlog).
    if existing.keys().ne(fresh.keys()) && !preparation_profile_changed {
        return None;
    }
    (preparation_profile_changed || existing != fresh)
        .then(|| latest.manifest.r#gen.saturating_add(1))
}

/// QB41 (03 §2.1's second legal gen+1 path): create a FRESH normalized
/// instance at `new_gen` for a `raw_hash` whose prepare output drifted from
/// the existing instance's (`prepared_hash_drift_new_gen` already confirmed
/// the drift, and the caller already gated confirmation -- `--yes`, mirroring
/// `kio reindex --force`'s `KIO-E-CONFIRM-REJECTED-001` pattern). Deliberately
/// FULL mode only (no incremental): this is a rare, renderer-drift-driven
/// event, not a hot path, and the OLD instance's per-unit content has no sound
/// correspondence to the NEW prepare output for incremental-reuse purposes
/// (mirrors `execute_online_markdownize_task`'s office incremental-skip doc,
/// the analogous online-side reasoning). The OLD gen is left completely
/// untouched (first-instance-wins) -- only a NEW `g<new_gen>` directory is
/// created; `write_prepared_objects` already durably wrote the (gen-independent,
/// content-addressed) prepared CAS objects earlier in this same candidate's
/// pass, so this function does not repeat that step.
///
/// Scope-trim (documented, not a silent gap -- see the implementation report):
/// this does not ALSO drive a fresh online-adapter send for the new
/// generation. `enqueue_online_placeholder_task`'s idempotency keys off
/// `(input_path, input_hash)` only (not gen), so an already-Done online task
/// for this raw_hash would dedup-skip a fresh enqueue even though its content
/// is now stale for `new_gen`. Making the online lane gen-aware is a larger,
/// independent change deferred out of this pass.
#[allow(clippy::too_many_arguments)]
fn create_prepared_hash_drift_instance(
    repo: &Repository,
    task_store: &TaskStore,
    markdown_adapter: &dyn MarkdownizeAdapter,
    candidate: &ScanCandidate,
    path: &Path,
    raw_hash: &str,
    tool_profile_hash: &str,
    fresh_prepared_units: &[PreparedUnit],
    verified_bytes: &[u8],
    new_gen: u64,
    now: &str,
    result: &mut IndexPipelineResult,
) -> Result<()> {
    let output_ref = normalized_output_ref(repo, raw_hash, tool_profile_hash, new_gen);
    let request = MarkdownizeRequest {
        raw: RawInput {
            raw_hash: raw_hash.to_owned(),
            path: Some(path.display().to_string()),
        },
        media_type: candidate.media_type.clone(),
        prepared_unit_hint: Some(prepared_unit_hints(fresh_prepared_units)),
        mode: AdapterMarkdownizeMode::Full,
        previous: None,
        hints: None,
        restrict_to_hint_pages: false,
        bbox_annotation_enabled: false,
        tool_profile_hash: tool_profile_hash.to_owned(),
        spec_version: 1,
        idempotency_token: None,
    };
    let response = match offline_markdownize_from_verified_bytes(
        markdown_adapter,
        request,
        verified_bytes,
        repo.control(),
    ) {
        Ok(response) => response,
        Err(_) => {
            append_failed_markdownize_task(
                repo,
                task_store,
                candidate,
                raw_hash,
                &output_ref,
                "contract_violation",
                now,
            )?;
            result.failed_files += 1;
            return Ok(());
        }
    };
    let full_hints = all_changed_hints(fresh_prepared_units);
    if response.fallback_to_full
        || validate_markdownize_response(&response, &full_hints, fresh_prepared_units).is_err()
    {
        append_failed_markdownize_task(
            repo,
            task_store,
            candidate,
            raw_hash,
            &output_ref,
            "contract_violation",
            now,
        )?;
        result.failed_files += 1;
        return Ok(());
    }
    let generated_at = repo.control().now_utc_seconds();
    let run_id = format!("run_{}", new_ulid(repo.canonical_root()));
    let units = normalized_units_from_response(
        repo,
        &candidate.media_type,
        verified_bytes,
        &response,
        fresh_prepared_units,
        None,
        raw_hash,
        tool_profile_hash,
        new_gen,
        MarkdownizeMode::Full,
        &generated_at,
    )
    .map_err(NormalizedUnitsError::into_kio)?;
    let manifest = manifest_from_units(
        fresh_prepared_units,
        &units,
        raw_hash,
        tool_profile_hash,
        new_gen,
        Some(new_gen.saturating_sub(1)),
        &run_id,
        &generated_at,
        RetryErrorKind::ContractViolation,
    );
    let stamped_manifest =
        persist_normalized_instance_with_control(repo.kio_dir(), &manifest, &units, repo.control())
            .map_err(pipeline_to_kio)?;
    let manifest_hash = hash_and_write_manifest_object(repo, &stamped_manifest)?;
    let mut task = task_descriptor(
        repo,
        TaskType::Markdownize,
        Some(MarkdownizeMode::Full),
        candidate,
        raw_hash,
        &output_ref,
        TaskStatus::Done,
        Some("prepared_hash_drift_gen_bump"),
        &generated_at,
    );
    task.unit_keys = Some(
        fresh_prepared_units
            .iter()
            .map(|unit| unit.unit_key.clone())
            .collect(),
    );
    task_store.append(&task).map_err(pipeline_to_kio)?;
    result.normalize_by_path.insert(
        candidate.input_path.clone(),
        PendingNormalizeRef {
            expected_raw_hash: raw_hash.to_owned(),
            normalize: NormalizeRef {
                tool_profile_hash: tool_profile_hash.to_owned(),
                r#gen: new_gen,
                manifest_hash,
            },
        },
    );
    result.normalized_files += 1;
    Ok(())
}
