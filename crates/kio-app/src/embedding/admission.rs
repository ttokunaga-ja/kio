use crate::embedding::plan::EmbeddableChunk;
use crate::embedding::plan::chunk_embedding_hash;
use crate::ledger_access::warn_uncovered_pricing_kinds_once;
use crate::markdownize::execution::TaskExecutionFailure;
use crate::object_persistence::adapter_to_kio;
use crate::settings::user_config_toml_path;
use kio_adapter::catalog::DeclaredEmbeddingProfile;
use kio_adapter::catalog::EmbeddingExecution;
use kio_adapter::catalog::active_embedding_execution_with_control;
use kio_adapter::catalog::declared_embedding_profile_for;
use kio_adapter::catalog::embedding_adapter_profile_with_control;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_adapter::traits::PreferredRequestKind;
use kio_adapter::types::ExecutionMode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_pipeline::ledger::ops::BilledAmount;
use kio_pipeline::ledger::ops::resolve_billing_from_reported_usage;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::task::RetryErrorKind;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

pub(crate) const EMBEDDING_ADAPTER_KIND: &str = "embedding";
const EMBEDDING_SYNC_USD_PER_TOKEN: f64 = 0.000_000_2; // $0.20 / 1M
const BATCH_RATE_MULTIPLIER: f64 = 0.5;

pub(crate) fn secret_embedding_hold_required(
    execution_mode: ExecutionMode,
    secrets_approved: bool,
    requires_secret_approval: bool,
) -> bool {
    execution_mode != ExecutionMode::DeterministicLibrary
        && !secrets_approved
        && requires_secret_approval
}

pub(crate) fn validate_embedding_aliases<'a>(
    policy: &CurrentPolicyEvaluator,
    profile: &DeclaredEmbeddingProfile,
    chunks: impl IntoIterator<Item = &'a EmbeddableChunk>,
) -> std::result::Result<(), TaskExecutionFailure> {
    let denied = || TaskExecutionFailure {
        retry_kind: RetryErrorKind::AuthError,
        retry_after_ms: None,
    };
    policy.revalidate().map_err(|_| denied())?;
    for chunk in chunks {
        if chunk.work_key.chunk_id() != chunk.chunk_id
            || chunk_embedding_hash(chunk, profile).map_err(|_| TaskExecutionFailure {
                retry_kind: RetryErrorKind::ContractViolation,
                retry_after_ms: None,
            })? != chunk.work_key.embedding_hash()
        {
            return Err(TaskExecutionFailure {
                retry_kind: RetryErrorKind::ContractViolation,
                retry_after_ms: None,
            });
        }
        if !policy.allows_path(&chunk.raw_path).map_err(|_| denied())? {
            return Err(denied());
        }
    }
    Ok(())
}

pub(crate) fn embedding_execution(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    control: &ControlHandle,
) -> Option<EmbeddingExecution> {
    active_embedding_execution_with_control(adapter_settings, control)
}

pub(crate) fn declared_embedding_profile(
    execution: EmbeddingExecution,
) -> DeclaredEmbeddingProfile {
    declared_embedding_profile_for(execution)
}

/// Whether the active embedding execution is the remote, billable API lane
/// (07 §3).
///
/// An `OfflineApi` may still be an authenticated local HTTPS peer, so this is
/// deliberately not a transport-safety predicate. That predicate is
/// [`embedding_transport_allowed`]. The cost ledger and batch lane each need
/// this remote-versus-local distinction and nothing finer.
pub(crate) fn embedding_is_online(execution: EmbeddingExecution) -> bool {
    execution.is_online()
}

/// The evaluator runs in this process and never opens a transport. It is the
/// only embedding execution exempt from network grants and `--offline`.
pub(crate) fn embedding_is_deterministic(execution: EmbeddingExecution) -> bool {
    embedding_mode_is_in_process(execution.execution_mode())
}

fn embedding_mode_is_in_process(mode: ExecutionMode) -> bool {
    mode == ExecutionMode::DeterministicLibrary
}

pub(crate) fn embedding_mode_transport_allowed(mode: ExecutionMode, offline: bool) -> bool {
    embedding_mode_is_in_process(mode) || !offline
}

pub(crate) fn embedding_transport_allowed(execution: EmbeddingExecution, offline: bool) -> bool {
    embedding_mode_transport_allowed(execution.execution_mode(), offline)
}

pub(crate) fn embedding_tool_lock_entry(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    control: &ControlHandle,
) -> Result<Option<Value>> {
    let Some(execution) = embedding_execution(adapter_settings, control) else {
        return Ok(None);
    };
    let profile = declared_embedding_profile(execution);
    // `kind`/`mode` used to be literals, which recorded an offline adapter as
    // having run online. They are properties of the adapter, so they come from
    // its profile — the lock is provenance, and provenance that always says
    // "online" is not provenance (07 §6).
    let execution_mode =
        embedding_adapter_profile_with_control(adapter_settings, execution, control)
            .map_err(adapter_to_kio)?
            .execution_mode;
    let (kind, mode) = embedding_execution_labels(execution_mode);
    Ok(Some(json!({
        "tool_id": profile.tool_id,
        "profile_hash": profile.profile_hash,
        "dimensions": profile.dimensions,
        "distance": profile.distance,
        "modality": profile.modality,
        "kind": kind,
        "mode": mode,
    })))
}

pub(crate) fn embedding_execution_labels(mode: ExecutionMode) -> (&'static str, &'static str) {
    match mode {
        ExecutionMode::OnlineApi => ("online_api", "online"),
        ExecutionMode::OfflineApi => ("offline_api", "offline"),
        ExecutionMode::DeterministicLibrary => ("deterministic_library", "deterministic"),
    }
}

// ===========================================================================

pub(crate) fn lane_adjusted_pricing(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    role: &str,
    lane: PreferredRequestKind,
) -> std::collections::BTreeMap<String, f64> {
    adapter_settings
        .declared_pricing(role)
        .into_iter()
        .map(|(kind, standard)| (kind, lane_rate(standard, lane)))
        .collect()
}

/// The online send lane resolved for THIS invocation.
///
/// 2026-07-24 ユーザー裁定: OCR と embedding は**必ず同一レーン**で送る — 片方
/// だけ Batch という組み合わせは作らない。したがってレーンは adapter ごとでは
/// なく invocation ごとに 1 つ決まり、この 1 セルがそれを持つ。
///
/// 解決順は network opt-in (07 §3) と同じ形: **CLI → scope config → user config
/// → 既定 (Batch)**。既定を Batch にするのは半額 (OCR $2/1,000 pages・embedding
/// $0.10/1M tokens) だからで、`--realtime` は倍額を承知の明示 opt-in である。
/// これは 2026-07-23 の「OCR 課金は Batch レーンのみ」裁定を明示的に上書きする。
///
/// 0 = 未解決 (= 既定の Batch として読む), 1 = Batch, 2 = Realtime.
static INVOCATION_LANE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

const LANE_CELL_BATCH: u8 = 1;
const LANE_CELL_REALTIME: u8 = 2;

/// The `--realtime` / `--batch` pair as parsed off one command.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LaneOverride {
    realtime: bool,
    batch: bool,
}

impl LaneOverride {
    pub(crate) fn new(realtime: bool, batch: bool) -> Self {
        Self { realtime, batch }
    }
}

/// Resolve and record the invocation lane. Call once per command, before any
/// online send is planned or priced — both the OCR lane selection and the
/// reservation estimates read the result.
///
/// `scope_config` is the scope's `.kio/config.toml` when the command has a
/// repository open; `None` skips straight to the user config.
pub(crate) fn resolve_invocation_lane(
    over: LaneOverride,
    scope_config: Option<&Path>,
    control: &ControlHandle,
) -> Result<()> {
    if over.realtime && over.batch {
        return Err(KioError::invalid_usage(
            "--realtime and --batch are mutually exclusive",
        ));
    }
    let lane = if over.realtime {
        LANE_CELL_REALTIME
    } else if over.batch {
        LANE_CELL_BATCH
    } else {
        let configured = scope_config
            .and_then(kio_core::scope::read_adapter_lane)
            .or_else(|| kio_core::scope::read_adapter_lane(&user_config_toml_path(control)));
        match configured.as_deref() {
            Some("realtime") => LANE_CELL_REALTIME,
            _ => LANE_CELL_BATCH,
        }
    };
    INVOCATION_LANE.store(lane, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// The lane BOTH online adapters send on this invocation. There is no
/// per-adapter answer by ruling — see [`INVOCATION_LANE`].
pub(crate) fn effective_invocation_lane() -> PreferredRequestKind {
    if INVOCATION_LANE.load(std::sync::atomic::Ordering::SeqCst) == LANE_CELL_REALTIME {
        PreferredRequestKind::Sync
    } else {
        PreferredRequestKind::Batch
    }
}

#[must_use]
pub(crate) fn realtime_lane_requested() -> bool {
    matches!(effective_invocation_lane(), PreferredRequestKind::Sync)
}

/// The lane the SYNCHRONOUS enrichment path sends on — always `Sync`.
///
/// R24 (3 系統一致) corrected this comment, which still claimed the Batch driver
/// "is still to land". It has landed: [`effective_invocation_lane`] chooses the
/// lane, and when it selects Batch the work goes to
/// [`submit_embedding_batch_jobs`] and never reaches the code that calls this.
///
/// So the callers of this function are, by construction, the ones already on the
/// synchronous lane — either because the invocation asked for `--realtime` or
/// because the batch lane was unavailable and the send fell back. Pricing them
/// at the sync rate is therefore correct, not a placeholder — and since I12 it
/// prices the SETTLEMENT too, not just the reservation, because
/// `embedding_billed_from_usage` resolves the measured tokens through this same
/// lane.
pub(crate) fn active_embedding_send_lane() -> PreferredRequestKind {
    PreferredRequestKind::Sync
}

/// The query lane is ALWAYS synchronous: a search cannot wait out a batch
/// job's turnaround (up to 24 h), so a query embedding is priced at the sync
/// rate regardless of what the enrichment path does.
pub(crate) fn query_embedding_send_lane() -> PreferredRequestKind {
    PreferredRequestKind::Sync
}

/// The per-token rate for `lane`.
///
/// R24 (5 系統一致): this used to `return declared` BEFORE looking at `lane`,
/// so declaring `tokens_in` in `tools.toml` — which the spec calls the
/// normative source, and which the dogfood config does declare — silently
/// collapsed the two lanes to one price. At the time the reservation estimate
/// was also the settled charge, so that recorded every Batch job at the sync
/// rate: a 2x over-record that consumes the budget cap twice as fast as the
/// provider actually bills. Settlement now prices measured tokens (I4/I12) —
/// through this same function — so a wrong lane here corrupts BOTH sides at
/// once rather than only the reservation.
pub(crate) fn embedding_usd_per_token(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    lane: PreferredRequestKind,
) -> f64 {
    let standard = adapter_settings
        .declared_pricing("embedding")
        .get("tokens_in")
        .copied()
        .unwrap_or(EMBEDDING_SYNC_USD_PER_TOKEN);
    lane_rate(standard, lane)
}

/// The lane split applied to a STANDARD per-token rate, whatever its source.
/// Split out from [`embedding_usd_per_token`] so the declared-pricing path is
/// testable: `register_declared_pricing` is a process-global `OnceLock`, so a
/// unit test cannot install a `tools.toml` price and observe the result.
pub(crate) fn lane_rate(standard: f64, lane: PreferredRequestKind) -> f64 {
    match lane {
        PreferredRequestKind::Batch => standard * BATCH_RATE_MULTIPLIER,
        PreferredRequestKind::Sync => standard,
    }
}

/// Rough token count for `text`.
///
/// The previous `chars / 4` assumed Latin script throughout and undercounts
/// CJK badly — a Japanese chunk is closer to one token per character, so a
/// mixed corpus was estimated (and, because `EmbeddingResponse.usage` is
/// `None`, *charged*) well under what it costs. Count CJK scalars as one token
/// each and the rest at the ~4-chars/token Latin ratio.
pub(crate) fn estimate_embedding_tokens(text: &str) -> f64 {
    let mut cjk = 0u64;
    let mut other = 0u64;
    for ch in text.chars() {
        if is_cjk_scalar(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk as f64 + other as f64 / 4.0
}

/// Scalars that tokenize at roughly one token each: Han, kana, Hangul, and the
/// CJK punctuation/fullwidth blocks that sit between them.
fn is_cjk_scalar(ch: char) -> bool {
    matches!(ch as u32,
        0x3000..=0x30FF      // CJK symbols and punctuation, hiragana, katakana
        | 0x3130..=0x318F    // Hangul compatibility jamo
        | 0x3400..=0x4DBF    // CJK unified ideographs extension A
        | 0x4E00..=0x9FFF    // CJK unified ideographs
        | 0xAC00..=0xD7AF    // Hangul syllables
        | 0xF900..=0xFAFF    // CJK compatibility ideographs
        | 0xFF00..=0xFFEF    // Halfwidth and fullwidth forms
        | 0x20000..=0x2FA1F  // CJK extensions B-F and compatibility supplement
    )
}

/// Cost estimate for embedding `text` on `lane`.
///
/// This is the RESERVATION, and — per I12's fix — only the settlement fallback
/// for a send whose response carried no usable usage report. It used to be the
/// settled charge outright, on the since-falsified reasoning that the endpoint
/// returns no token count. It still has to be right: I10 measured this
/// estimator against 33 real jobs and found -32%..+52% error with 24% of jobs
/// UNDER-estimated, and the budget cap gates on the reservation, not on the
/// settlement.
pub(crate) fn estimate_embedding_cost(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    text: &str,
    lane: PreferredRequestKind,
) -> f64 {
    estimate_embedding_tokens(text) * embedding_usd_per_token(adapter_settings, lane)
}

/// Price one embedding send from the usage the adapter reported, degrading to
/// `fallback_estimated_usd` (the reservation) whenever that report is absent or
/// fails to cover every kind the adapter declares it bills for.
///
/// I12: one function for all three send sites — Batch collect, sync query, sync
/// chunk — because pricing one adapter in three places is exactly how the bug
/// happened. The Batch site learned to read `usage` while the two sync sites
/// went on asserting the endpoint had none to give, and nothing in the type
/// system objected. A new send site now has to pick a lane and a fallback, not
/// invent a pricing rule.
pub(crate) fn embedding_billed_from_usage(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    reported: Option<&kio_adapter::types::AdapterUsage>,
    lane: PreferredRequestKind,
    fallback_estimated_usd: f64,
    control: &ControlHandle,
    session: &kio_core::scope::LockSession,
) -> BilledAmount {
    let (billed, uncovered) = resolve_billing_from_reported_usage(
        reported,
        // The embedding adapter declares exactly one billable kind
        // (`gemini_embedding`'s profile: `TokensIn`). Its module is private, so
        // the set is restated rather than fabricated from an `AdapterProfile`
        // these call sites do not have.
        &std::collections::BTreeSet::from(["tokens_in".to_owned()]),
        // The SAME rate resolution the reservation used, fallback included.
        // Pricing the two sides from different sources is how a reservation and
        // its settlement drift apart.
        &std::collections::BTreeMap::from([(
            "tokens_in".to_owned(),
            embedding_usd_per_token(adapter_settings, lane),
        )]),
        fallback_estimated_usd,
    );
    warn_uncovered_pricing_kinds_once(EMBEDDING_ADAPTER_KIND, &uncovered, control, session);
    billed
}
