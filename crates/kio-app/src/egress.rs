use crate::approvals;
use crate::commands::IndexArgs;
use crate::embedding::transport::declared_embedding_profile;
use crate::embedding::transport::embedding_execution;
use crate::embedding::transport::embedding_is_deterministic;
use crate::embedding::transport::embedding_tool_lock_entry;
use crate::embedding::transport::embedding_transport_allowed;
use crate::grants;
use crate::index::pipeline::active_markdown_adapter;
use crate::index::pipeline::offline_output_ref;
use crate::index::pipeline::online_markdownize_profile_for;
use crate::index::pipeline::online_output_ref;
use crate::ledger_access::RETIRED_NON_LIVE;
use crate::object_persistence::adapter_to_kio;
use crate::object_persistence::atomic_overwrite_file;
use crate::object_persistence::pipeline_to_kio;
use crate::search::scope::ExecScope;
use crate::search::scope::current_embedding_policy;
use crate::task_execution::task_retry_allowed;
use kio_adapter::catalog::DeclaredEmbeddingProfile;
use kio_adapter::catalog::EmbeddingExecution;
use kio_adapter::catalog::active_local_ocr_execution_with_control;
use kio_adapter::catalog::builtin_prepare_profile;
use kio_adapter::catalog::local_ocr_handles_media_type;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::tool_lock::load_tool_lock;
use kio_adapter::types::AdapterProfile;
use kio_adapter::types::ExecutionMode;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::scope::Repository;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::prepare::hash_bytes;
use kio_pipeline::scan::ScanPreview;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

pub(crate) fn persistent_network_allowed(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
) -> Result<bool> {
    let profile = match active_local_ocr_execution_with_control(adapter_settings, repo.control()) {
        Some(execution) => kio_adapter::local_ocr_markdownize::profile_for(execution),
        None => online_markdownize_profile_for(repo)?,
    };
    persistent_network_allowed_for(
        adapter_settings,
        repo,
        approvals::AdapterRole::Markdown,
        &profile.adapter_id,
        &profile.tool_profile_hash,
    )
}

pub(crate) fn persistent_network_allowed_for(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    role: approvals::AdapterRole,
    tool_id: &str,
    profile: &str,
) -> Result<bool> {
    approvals::allowed(
        adapter_settings,
        repo,
        role,
        tool_id,
        profile,
        grants::GrantOperation::Network,
    )
}

pub(crate) fn local_markdownize_transport_allowed(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    profile: &AdapterProfile,
    needs_secret: bool,
) -> Result<bool> {
    current_embedding_policy(repo)?
        .revalidate()
        .map_err(pipeline_to_kio)?;
    if !persistent_network_allowed_for(
        adapter_settings,
        repo,
        approvals::AdapterRole::Markdown,
        &profile.adapter_id,
        &profile.tool_profile_hash,
    )? {
        return Ok(false);
    }
    Ok(!needs_secret
        || secrets_send_approved(
            adapter_settings,
            repo,
            approvals::AdapterRole::Markdown,
            &profile.adapter_id,
            &profile.tool_profile_hash,
        ))
}

/// Admission at the last safe boundary before a non-deterministic embedding
/// transport. The caller chooses whether it is before a paid reservation
/// (online) or directly at an unmetered local HTTP call.
pub(crate) fn admit_embedding_transport(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    execution: EmbeddingExecution,
    profile: &DeclaredEmbeddingProfile,
    current_policy: &CurrentPolicyEvaluator,
    needs_secret: bool,
) -> Result<()> {
    if embedding_is_deterministic(execution) {
        return Ok(());
    }
    current_policy.revalidate().map_err(pipeline_to_kio)?;
    if !persistent_network_allowed_for(
        adapter_settings,
        repo,
        approvals::AdapterRole::Embedding,
        &profile.tool_id,
        &profile.profile_hash,
    )? {
        return Err(KioError::new(
            "KIO-E-ADAPTER-APPROVAL-REQUIRED-001",
            "embedding network approval is no longer current",
            json!({}),
            ExitCode::AuthError,
        ));
    }
    if needs_secret && !embedding_secrets_allowed(adapter_settings, repo, profile)? {
        return Err(KioError::new(
            "KIO-E-ADAPTER-APPROVAL-REQUIRED-001",
            "embedding secret-send approval is no longer current",
            json!({}),
            ExitCode::AuthError,
        ));
    }
    Ok(())
}

pub(crate) fn embedding_secrets_allowed(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    profile: &DeclaredEmbeddingProfile,
) -> Result<bool> {
    approvals::allowed(
        adapter_settings,
        repo,
        approvals::AdapterRole::Embedding,
        &profile.tool_id,
        &profile.profile_hash,
        grants::GrantOperation::SendSecrets,
    )
}

fn persistent_network_allowed_for_kio_dir(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    kio_dir: &Path,
    role: approvals::AdapterRole,
    tool_id: &str,
    profile: &str,
    control: &kio_core::control::ControlHandle,
) -> Result<bool> {
    approvals::allowed_at(
        adapter_settings,
        kio_dir,
        role,
        tool_id,
        profile,
        grants::GrantOperation::Network,
        control,
    )
}

/// Query text is supplied by this invocation. One currently authorized eligible
/// scope may admit its single embedding request; --online does not create a grant.
pub(crate) fn embedding_opt_in_for_scopes(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    exec_scopes: &[ExecScope],
    tool_id: &str,
    tool_profile_hash: &str,
    offline: bool,
    control: &kio_core::control::ControlHandle,
) -> Result<bool> {
    if offline {
        return Ok(false);
    }
    for exec in exec_scopes {
        if persistent_network_allowed_for_kio_dir(
            adapter_settings,
            &exec.target.kio_dir,
            approvals::AdapterRole::Embedding,
            tool_id,
            tool_profile_hash,
            control,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn embedding_online_allowed(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    offline: bool,
    _online: bool,
    _online_confirmed: bool,
) -> Result<bool> {
    let Some(execution) = embedding_execution(adapter_settings, repo.control()) else {
        return Ok(false);
    };
    if embedding_is_deterministic(execution) {
        return Ok(true);
    }
    if !embedding_transport_allowed(execution, offline) {
        return Ok(false);
    }
    let profile = declared_embedding_profile(execution);
    persistent_network_allowed_for(
        adapter_settings,
        repo,
        approvals::AdapterRole::Embedding,
        &profile.tool_id,
        &profile.profile_hash,
    )
}

/// QA21/23: `active_embedding_adapter_id()` plus the adapter's CURRENT
/// `tool_profile_hash` (needed for the `approvals[]` profile match). `None`
/// when no embedding execution is active, mirroring
/// `active_embedding_adapter_id`.
pub(crate) fn active_embedding_adapter_identity(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    control: &ControlHandle,
) -> Result<Option<(String, String)>> {
    let Some(execution) = embedding_execution(adapter_settings, control) else {
        return Ok(None);
    };
    let profile = declared_embedding_profile(execution);
    Ok(Some((profile.tool_id, profile.profile_hash)))
}

pub(crate) fn materialize_tool_lock(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
) -> Result<()> {
    let prepare_profile = builtin_prepare_profile();
    let markdown_profile = active_markdown_adapter(repo).profile();
    let mut value = json!({
        "spec_version": 1,
        "prepare": {
            "tool_id": prepare_profile.adapter_id,
            "profile_hash": prepare_profile.tool_profile_hash,
            "kind": "deterministic_library"
        },
        "markdown": {
            "tool_id": markdown_profile.adapter_id,
            "profile_hash": markdown_profile.tool_profile_hash,
            "kind": execution_mode_name(markdown_profile.execution_mode),
            "capabilities": markdown_profile.capability_flags
        }
    });
    // Write the embedding entry (07 §6) when an embedding adapter is configured.
    // A non-multimodal profile is rejected here (03 §7): `load_tool_lock` fails
    // with KIO-E-EMBED-MODALITY-001, which we surface as exit 2 (scenario (e))
    // *before* any indexing happens.
    if let Some(entry) = embedding_tool_lock_entry(adapter_settings, repo.control())?
        && let Some(object) = value.as_object_mut()
    {
        object.insert("embedding".to_owned(), entry);
    }
    let bytes =
        serde_json::to_vec_pretty(&value).map_err(|err| KioError::schema(err.to_string()))?;
    // `adapter_to_kio` reads the code off `ConfigSchemaCoded` and gives it
    // exit 2, so KIO-E-EMBED-MODALITY-001 needs no special case here.
    load_tool_lock(&bytes).map_err(adapter_to_kio)?;
    let path = repo.kio_dir().join("tool-lock.json");
    atomic_overwrite_file(&path, &bytes)
}

fn execution_mode_name(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::OnlineApi => "online_api",
        ExecutionMode::OfflineApi => "offline_api",
        ExecutionMode::DeterministicLibrary => "deterministic_library",
    }
}

pub(crate) fn network_allowed(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    args: &IndexArgs,
) -> Result<bool> {
    if args.offline {
        return Ok(false);
    }
    persistent_network_allowed(adapter_settings, repo)
}

/// Current paired device and scope permission for this exact secret recipient.
pub(crate) fn secrets_send_approved(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    role: approvals::AdapterRole,
    tool_id: &str,
    profile: &str,
) -> bool {
    approvals::secrets_allowed(adapter_settings, repo, role, tool_id, profile).unwrap_or(false)
}

pub(crate) fn active_markdownize_profile_for(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
) -> Result<AdapterProfile> {
    match active_local_ocr_execution_with_control(adapter_settings, repo.control()) {
        Some(execution) => Ok(kio_adapter::local_ocr_markdownize::profile_for(execution)),
        None => online_markdownize_profile_for(repo),
    }
}

pub(crate) fn markdown_output_ref_for_profile(profile: &AdapterProfile) -> String {
    match profile.execution_mode {
        ExecutionMode::OfflineApi => offline_output_ref(&profile.adapter_id),
        ExecutionMode::OnlineApi => online_output_ref(&profile.adapter_id),
        ExecutionMode::DeterministicLibrary => unreachable!("OCR profiles are external adapters"),
    }
}

/// Release only the Markdownize holds whose exact selected external recipient
/// has a current send-secrets grant. Embedding holds are re-driven by their
/// separate embedding admission path.
pub(crate) fn release_markdownize_secret_holds(
    repo: &Repository,
    output_ref: &str,
) -> Result<usize> {
    let store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    let released = store
        .update_matching(|task| {
            if task.task_type == TaskType::Markdownize
                && task.output_ref == output_ref
                && task.status == TaskStatus::Paused
                && task.fallback_reason.as_deref() == Some(SECRETS_TIER_B_HOLD)
            {
                task.status = TaskStatus::Pending;
                task.fallback_reason = None;
                task.hold_reason = None;
                true
            } else {
                false
            }
        })
        .map_err(pipeline_to_kio)?;
    Ok(released)
}

/// A terminal embedding failure must remain terminal when a later secret hold
/// with the same output reference is encountered. Remove only that redundant
/// hold before the ordinary embedding hold/enqueue reconciliation runs.
pub(crate) fn prune_terminal_embedding_secret_holds(repo: &Repository) -> Result<usize> {
    let store = TaskStore::new_with_control(repo.kio_dir(), repo.control().clone());
    let mut tasks = store.all().map_err(pipeline_to_kio)?;
    let terminal_embedding_refs = tasks
        .iter()
        .filter(|task| {
            task.task_type == TaskType::Embedding
                && task.status == TaskStatus::Failed
                && task.fallback_reason.as_deref() != Some(RETIRED_NON_LIVE)
                && !task_retry_allowed(task)
        })
        .map(|task| task.output_ref.clone())
        .collect::<BTreeSet<_>>();
    let before = tasks.len();
    tasks.retain(|task| {
        !(task.task_type == TaskType::Embedding
            && task.status == TaskStatus::Paused
            && task.fallback_reason.as_deref() == Some(SECRETS_TIER_B_HOLD)
            && terminal_embedding_refs.contains(&task.output_ref))
    });
    let removed_duplicates = before.saturating_sub(tasks.len());
    if removed_duplicates > 0 {
        store.replace_all(&tasks).map_err(pipeline_to_kio)?;
    }
    Ok(removed_duplicates)
}

/// Fallback reason marking an online task held because its input is a Tier B
/// (candidate-secret) file awaiting `--send-secrets` (N1a).
pub(crate) const SECRETS_TIER_B_HOLD: &str = "secrets_tier_b_hold";

pub(crate) fn record_quarantine_candidates(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    preview: &ScanPreview,
) -> Result<()> {
    let path = repo.kio_dir().join("quarantine.jsonl");
    // R19-7: dedup on (path, approval_method), not path alone — otherwise a
    // `hold` record permanently blocks the later `send_approved` transition
    // (after `--send-secrets`), leaving `kio status` reporting an approved,
    // already-sent file as still pending-hold. The reader takes the latest row
    // per path, so appending the transition row corrects the disposition.
    let existing = read_quarantine_records(repo)?
        .into_iter()
        .filter_map(|entry| {
            let path = entry.get("path").and_then(Value::as_str)?.to_owned();
            let method = entry
                .get("approval_method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Some((path, method))
        })
        .collect::<BTreeSet<_>>();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    for candidate in &preview.candidates {
        let profile =
            match active_local_ocr_execution_with_control(adapter_settings, repo.control())
                .filter(|_| local_ocr_handles_media_type(&candidate.media_type))
            {
                Some(execution) => kio_adapter::local_ocr_markdownize::profile_for(execution),
                None => online_markdownize_profile_for(repo)?,
            };
        let secrets_approved = secrets_send_approved(
            adapter_settings,
            repo,
            approvals::AdapterRole::Markdown,
            &profile.adapter_id,
            &profile.tool_profile_hash,
        );
        // N1b: record Tier A (excluded from ingest) AND Tier B (ingested locally
        // but held from online send) so `kio status` surfaces both. Tier B carries
        // its live disposition — "hold" until `--send-secrets`, then "send_approved".
        let (reason, approval_method) = match candidate.quarantine_reason.as_deref() {
            Some("secrets_tier_a_excluded") if candidate.ignored => {
                ("secrets_tier_a", "quarantine")
            }
            // R19-1: a lifted (ingested) Tier A secret held from online send — record it
            // with its live disposition so `kio status` surfaces the pending online hold
            // and the eventual approval (audit trail; 07 §122).
            Some("secrets_tier_a_online_hold") => (
                "secrets_tier_a",
                if secrets_approved {
                    "send_approved"
                } else {
                    "hold"
                },
            ),
            Some("secrets_tier_b_warning") => (
                "secrets_tier_b",
                if secrets_approved {
                    "send_approved"
                } else {
                    "hold"
                },
            ),
            _ => continue,
        };
        if existing.contains(&(candidate.input_path.clone(), approval_method.to_owned())) {
            continue;
        }
        let record = json!({
            "path": candidate.input_path,
            "reason": reason,
            "recorded_at": repo.control().now_utc_seconds(),
            "approval_method": approval_method,
        });
        // M1(b): one framed record per single write_all (no interleaving).
        let mut line = serde_json::to_string(&record)
            .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    }
    Ok(())
}

fn read_quarantine_records(repo: &Repository) -> Result<Vec<Value>> {
    let path = repo.kio_dir().join("quarantine.jsonl");
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect())
}

/// R20-9: quarantine records collapsed to the LATEST disposition per path, for `kio status`
/// display. `record_quarantine_candidates` appends a `hold` row, then (after
/// `--send-secrets`) a `send_approved` row — R19-7's `(path, approval_method)` dedup lets
/// both land. The raw `read_quarantine_records` returns BOTH, so status reported a file as
/// simultaneously "hold" and "send_approved". quarantine.jsonl is append-only in
/// chronological order, so the last row per path is the current disposition — the reader
/// behavior R19-7's fix comment assumed but never implemented. The raw reader is left
/// untouched (the writer's own `(path, method)` dedup set needs every row).
pub(crate) fn quarantine_status_records(repo: &Repository) -> Result<Vec<Value>> {
    let mut order: Vec<String> = Vec::new();
    let mut latest: BTreeMap<String, Value> = BTreeMap::new();
    for row in read_quarantine_records(repo)? {
        let key = row
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if !latest.contains_key(&key) {
            order.push(key.clone());
        }
        latest.insert(key, row);
    }
    Ok(order
        .into_iter()
        .filter_map(|key| latest.remove(&key))
        .collect())
}

/// QA7 (step4b-contract-tests-p3a.md §B, arbitration #1, 10 §1.1 L128-130):
/// `effective_ignore_hash` hashes the ACTUAL Tier A/B pattern content (via
/// `kio_core::scope::tier_a_template_text`), not a hand-maintained version
/// literal — a pattern-list edit changes this hash automatically.
fn effective_ignore_hash() -> String {
    hash_bytes(kio_core::scope::tier_a_template_text(kio_pipeline::scan::TIER_B_NEEDLES).as_bytes())
}

pub(crate) fn write_approval_record(
    repo: &Repository,
    preview: &ScanPreview,
    approval_method: &str,
) -> Result<()> {
    // QA5 (step4b-contract-tests-p3a.md §B, 10 §1 L97-113): the one-time
    // scope-level scan approval, distinct from the adapter-level opt-in rows
    // below (`approvals.jsonl`/`consents.jsonl`) — `record_scan_approval` is
    // idempotent, so calling it again on a later `index --approve` with no
    // NEW adapter to opt in (the `pending.is_empty()` early return below) is
    // harmless. QA19 (step4b-contract-tests-p3a.md §F, 10 §1 L48-53): cost
    // estimates come from `preview.estimated_cost` (`kio-pipeline`'s
    // `build_scan_preview`, now wired to `tools.toml`'s declared `[pricing]`)
    // — `0.0` only when no `pricing` is declared for that role, an honest
    // "unknown" rather than a fabricated figure.
    let (estimated_markdownize_usd, estimated_embedding_usd) = preview
        .estimated_cost
        .as_ref()
        .map(|cost| (cost.estimated_markdownize_usd, cost.estimated_embedding_usd))
        .unwrap_or((0.0, 0.0));
    repo.record_scan_approval(json!({
        "scope_id": preview.scope_id,
        "root_path": repo.canonical_root().display().to_string(),
        "approved_at": repo.control().now_utc_seconds(),
        "actor": std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned()),
        "approval_method": approval_method,
        "kio_version": env!("CARGO_PKG_VERSION"),
        "effective_ignore_hash": effective_ignore_hash(),
        "estimated_file_count": preview.candidates.iter().filter(|candidate| !candidate.ignored).count(),
        "estimated_total_bytes": preview.candidates.iter().filter(|candidate| !candidate.ignored).map(|candidate| candidate.size_bytes).sum::<u64>(),
        "estimated_markdownize_usd": estimated_markdownize_usd,
        "estimated_embedding_usd": estimated_embedding_usd,
    }))?;

    Ok(())
}
