//! Built-in adapter catalog and execution entry points.
//!
//! Non-adapter crates should depend on these stable catalog functions instead of
//! naming or constructing concrete built-in adapters directly.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::deterministic::DeterministicAdapter;
use crate::deterministic_embedding::{
    DETERMINISTIC_EVAL_EMBEDDING_PROFILE, DeterministicEmbeddingAdapter,
};
#[cfg(debug_assertions)]
use crate::gemini_embedding::GeminiEmbeddingClient;
use crate::gemini_embedding::{ADOPTED_DIMENSIONS, ADOPTED_MODEL_PIN, GeminiEmbeddingAdapter};
use crate::local_embedding::{
    LOCAL_EMBEDDING_DEFAULT_MODEL, LocalEmbeddingAdapter, LocalEmbeddingExecution,
};
use crate::mistral_ocr::{
    EnvMistralOcrClient, MistralOcrClient, MistralOcrMarkdownizeAdapter, OcrImage, OcrPage,
    OcrResponse,
};
use crate::office_convert::{is_office_media, resolve_office_converter};
use crate::traits::{EmbeddingAdapter, MarkdownizeAdapter, PrepareAdapter};
use crate::types::{
    AdapterProfile, AdapterUsage, EmbeddingInputType, EmbeddingItem, EmbeddingRequest,
    EmbeddingVector, ExecutionMode, FailedUnit, IncrementalHints, MarkdownizeMode,
    MarkdownizeRequest, MarkdownizeResponse, PreparedUnitHint, PreviousMarkdownizeContext,
    ProviderIdempotency, RawInput,
};
use crate::{AdapterError, Result};

// This module deliberately translates the typed, debug-only core control at
// the adapter boundary.  No production adapter parses KIO_TEST_* directly.
#[cfg(debug_assertions)]
mod test_control {
    use kio_core::test_control::{LocalOcrMode, MistralOcrMode, Selector};
    use std::path::PathBuf;

    fn known<T>(
        selector: Selector<T>,
        render: impl FnOnce(T) -> &'static str,
    ) -> Option<&'static str> {
        match selector {
            Selector::Known(value) => Some(render(value)),
            Selector::Unset | Selector::Unknown(_) => None,
        }
    }
    pub fn mistral_ocr() -> Option<&'static str> {
        known(
            crate::debug_test_control().adapters.mistral_ocr,
            |v| match v {
                MistralOcrMode::AuthError => "auth_error",
                MistralOcrMode::RateLimit => "rate_limit",
                MistralOcrMode::RateLimitAfter => "rate_limit_after",
                MistralOcrMode::NetworkError => "network_error",
                MistralOcrMode::Mock => "mock",
                MistralOcrMode::Partial => "partial",
                MistralOcrMode::PartialNetwork => "partial_network",
                MistralOcrMode::AllFailedNetwork => "all_failed_network",
                MistralOcrMode::AllFailedInvalidInput => "all_failed_invalid_input",
                MistralOcrMode::MockLinkImage => "mock_link_image",
                MistralOcrMode::IncrementalIncomplete => "incr_incomplete",
                MistralOcrMode::PinChanged => "pin_changed",
                MistralOcrMode::NoChangeNoSend => "no_change_no_send",
                MistralOcrMode::RequireIdempotencyToken => "require_idempotency_token",
            },
        )
    }
    pub fn local_ocr() -> Option<&'static str> {
        known(
            crate::debug_test_control().adapters.local_ocr,
            |v| match v {
                LocalOcrMode::Mock => "mock",
            },
        )
    }
    pub fn mistral_ocr_with_capture() -> (Option<&'static str>, Option<PathBuf>) {
        let adapters = crate::debug_test_control().adapters;
        let mode = known(adapters.mistral_ocr, |v| match v {
            MistralOcrMode::AuthError => "auth_error",
            MistralOcrMode::RateLimit => "rate_limit",
            MistralOcrMode::RateLimitAfter => "rate_limit_after",
            MistralOcrMode::NetworkError => "network_error",
            MistralOcrMode::Mock => "mock",
            MistralOcrMode::Partial => "partial",
            MistralOcrMode::PartialNetwork => "partial_network",
            MistralOcrMode::AllFailedNetwork => "all_failed_network",
            MistralOcrMode::AllFailedInvalidInput => "all_failed_invalid_input",
            MistralOcrMode::MockLinkImage => "mock_link_image",
            MistralOcrMode::IncrementalIncomplete => "incr_incomplete",
            MistralOcrMode::PinChanged => "pin_changed",
            MistralOcrMode::NoChangeNoSend => "no_change_no_send",
            MistralOcrMode::RequireIdempotencyToken => "require_idempotency_token",
        });
        (mode, adapters.capture_sent_media)
    }
}
#[cfg(not(debug_assertions))]
mod test_control {
    use std::path::PathBuf;
    pub fn mistral_ocr() -> Option<&'static str> {
        None
    }
    pub fn local_ocr() -> Option<&'static str> {
        None
    }
    pub fn mistral_ocr_with_capture() -> (Option<&'static str>, Option<PathBuf>) {
        (None, None)
    }
}

#[cfg(debug_assertions)]
pub const TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV: &str = "KIO_TEST_MISTRAL_OCR";
#[cfg(debug_assertions)]
pub const TEST_ADOPTED_EMBEDDING_ENV: &str = "KIO_TEST_GEMINI_EMBED";
/// Exact activation for the evaluator's deterministic scale embedding adapter.
///
/// This is deliberately not a test control: it is available in release builds
/// so the evaluator uses the same index/search adapter wire as production.
pub const DETERMINISTIC_EVAL_EMBEDDING_ENV: &str = "KIO_EVAL_DETERMINISTIC_EMBED";
/// The offline markdownize seam, separate from both of the above for the same
/// reason they are separate from each other: a test that drives local OCR must
/// not accidentally switch the embedding backend, or vice versa.
#[cfg(debug_assertions)]
pub const TEST_LOCAL_OCR_ENV: &str = "KIO_TEST_LOCAL_OCR";

/// Re-exported so the index path can gate image enrichment without naming the
/// concrete local adapter module (which stays private, like `gemini_embedding`).
pub use crate::local_embedding::IMAGE_OBJECT_CAPABILITY;

#[cfg(debug_assertions)]
use crate::local_ocr_markdownize::MockLocalOcrClient;
use crate::local_ocr_markdownize::{
    EnvLocalOcrClient, LayoutFileType, LocalOcrExecution, LocalOcrMarkdownizeAdapter,
};

/// Whether the local OCR pipeline can handle this media type at all.
///
/// Re-exported through the catalog so the CLI can ask without naming the
/// adapter module: everything it cannot route stays on the deterministic
/// built-in, which is the pre-Stage-3 behaviour for those types.
#[must_use]
pub fn local_ocr_handles_media_type(media_type: &str) -> bool {
    LayoutFileType::from_media_type(media_type).is_ok()
}

#[must_use]
pub fn builtin_prepare_profile() -> AdapterProfile {
    let mut profile = PrepareAdapter::profile(&DeterministicAdapter);
    profile.adapter_id = "prepare_default".to_owned();
    profile
}

#[must_use]
pub fn standard_online_markdownize_profile() -> AdapterProfile {
    standard_online_markdownize_profile_with_bbox(true)
}

#[must_use]
pub fn standard_online_markdownize_profile_with_bbox(enabled: bool) -> AdapterProfile {
    MistralOcrMarkdownizeAdapter::default()
        .with_bbox_annotation(enabled)
        .profile()
}

pub fn builtin_offline_markdownize_adapter() -> Box<dyn MarkdownizeAdapter> {
    Box::new(DeterministicAdapter)
}

/// The active `offline_api` markdownize backend, or `None` when none is
/// configured — in which case the deterministic built-in keeps running exactly
/// as before.
///
/// Mirrors `active_local_embedding_execution`, including the reason a
/// declaration selects `Real` rather than `Mock`: an `offline_api` adapter
/// authenticates to nothing, so the declaration itself is the activation
/// signal, and the mock stays reachable only through the env seam. That
/// asymmetry is what stops a `tools.toml` entry from silently writing mock
/// Markdown into a real archive.
#[must_use]
pub fn active_local_ocr_execution() -> Option<LocalOcrExecution> {
    match test_control::local_ocr() {
        #[cfg(debug_assertions)]
        Some("mock") => Some(LocalOcrExecution::Mock),
        #[cfg(not(debug_assertions))]
        Some("mock") => None,
        Some(_) => None,
        None => declared_offline_markdownize().then_some(LocalOcrExecution::Real),
    }
}

/// Whether `tools.toml` declares the markdown role as `offline_api`.
fn declared_offline_markdownize() -> bool {
    crate::tool_lock::registered_declared_adapter("markdown")
        .and_then(|declared| declared.kind)
        .is_some_and(|kind| kind == "offline_api")
}

/// Build the offline markdownize adapter for a resolved execution.
///
/// `verified_raw_bytes` are the caller's already-verified bytes, passed in for
/// the same reason the online OCR adapter takes them: re-opening the path here
/// would read bytes nobody checked against `raw_hash`.
pub fn local_ocr_markdownize_adapter(
    execution: LocalOcrExecution,
    scope_id: &str,
    kio_dir: &Path,
    verified_raw_bytes: &[u8],
) -> Result<Box<dyn MarkdownizeAdapter>> {
    let adapter = match execution {
        #[cfg(debug_assertions)]
        LocalOcrExecution::Mock => LocalOcrMarkdownizeAdapter::new(
            MockLocalOcrClient::from_test_control(),
            LocalOcrExecution::Mock,
            scope_id,
        )
        .into_boxed(kio_dir, verified_raw_bytes),
        LocalOcrExecution::Real => {
            let declared =
                crate::tool_lock::registered_declared_adapter("markdown").ok_or_else(|| {
                    AdapterError::ConfigSchema(
                        "the offline markdownize adapter resolved without a declaration".to_owned(),
                    )
                })?;
            // Re-runs D1's literal-loopback check at the moment of use. An
            // entry that passed at load time and points elsewhere now must not
            // send — the document is the payload, and it is the user's.
            crate::tool_lock::validate_declared_runtime_target("markdown", &declared)?;
            let base_url = declared.url.clone().ok_or_else(|| {
                AdapterError::ConfigSchema(
                    "an offline_api markdownize adapter must declare `url`".to_owned(),
                )
            })?;
            let endpoint = crate::tool_lock::authenticated_local_endpoint("markdown", &base_url)?;
            // D7: `[adapter.policy.offline_api].timeout_seconds` (07 §7). A
            // document pipeline is the case that number exists for — a
            // multi-page PDF can hold the socket quiet for minutes.
            let timeout_seconds = crate::tool_lock::registered_execution_timeout("offline_api");
            LocalOcrMarkdownizeAdapter::new(
                EnvLocalOcrClient::new(endpoint, timeout_seconds),
                LocalOcrExecution::Real,
                scope_id,
            )
            .into_boxed(kio_dir, verified_raw_bytes)
        }
    };
    Ok(adapter)
}

/// The adopted embedding model pin, re-exported so the CLI's Batch lane can
/// name the model a job is created with without reaching into the private
/// adapter module.
pub use crate::gemini_embedding::ADOPTED_MODEL_PIN as ADOPTED_EMBEDDING_MODEL_PIN;

#[must_use]
pub fn adopted_embedding_profile() -> AdapterProfile {
    GeminiEmbeddingAdapter::default().profile()
}

pub struct StandardOnlineMarkdownizeRequest<'a> {
    pub scope_id: &'a str,
    pub kio_dir: &'a Path,
    pub raw_hash: &'a str,
    pub path: &'a Path,
    pub media_type: &'a str,
    pub prepared_unit_hints: Vec<PreparedUnitHint>,
    // R13-1 + R14-4: incremental Markdownize on the online route. When `mode =
    // Incremental`, `prepared_unit_hints` carries ONLY the changed+added units and
    // `previous` / `hints` carry the prior instance + unit_mapping result. The Mistral
    // client turns those hints' 0-based `order`s into the OCR `pages` parameter so only
    // those pages are processed — before R14-4 the hint was IGNORED by the real client
    // and the whole document was re-sent/re-billed every revision (the "changed only"
    // claim held only under the mock seam). `Full` sends every unit and no `pages`
    // (`previous`/`hints` are `None`), the pre-R13-1 behavior. (Whether the `pages`
    // parameter actually lowers Mistral billing is confirmed by real-API verification,
    // a user-gated step; the code-side hint-ignoring defect is fixed here.)
    pub mode: MarkdownizeMode,
    pub previous: Option<PreviousMarkdownizeContext>,
    pub hints: Option<IncrementalHints>,
    /// R15-5: set on a UNIT-SCOPED retry (the `prepared_unit_hints` carry only the
    /// failed subset, but `mode` is `Full` with no previous/hints). Forwarded to the
    /// Mistral client so it OCRs/bills only those pages instead of the whole document.
    /// A fresh full send leaves this `false`.
    pub restrict_to_hint_pages: bool,
    pub bbox_annotation_enabled: bool,
    /// QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): the ledger
    /// phase-1 `intent_token` for this send — see
    /// `MarkdownizeRequest::idempotency_token`'s doc. `None` when this call
    /// has no ledger charge (e.g. a resolve-only profile lookup site never
    /// reaches this struct at all — only an actual send does).
    pub idempotency_token: Option<String>,
}

pub struct StandardOnlineMarkdownizeOutcome {
    pub profile: AdapterProfile,
    pub response: MarkdownizeResponse,
    /// The complete unit set used by the adapter before any test-only response seam
    /// mutates the returned units. Fresh OCR-from-scratch requests have no caller
    /// hints, so this is how the provider-discovered page/image identities cross the
    /// adapter boundary and become the normalized manifest's Prepared units.
    pub effective_prepared_unit_hints: Vec<PreparedUnitHint>,
}

pub fn run_standard_online_markdownize(
    request: StandardOnlineMarkdownizeRequest<'_>,
) -> Result<StandardOnlineMarkdownizeOutcome> {
    let bytes = std::fs::read(request.path).map_err(|err| AdapterError::Io {
        path: request.path.display().to_string(),
        message: err.to_string(),
    })?;
    run_standard_online_markdownize_with_bytes(request, &bytes)
}

/// Execute online markdownization from bytes already owned by the caller. The
/// same verified buffer crosses the adapter boundary and is serialized into the
/// provider request; no pathname is reopened by the HTTP client.
pub fn run_standard_online_markdownize_with_bytes(
    request: StandardOnlineMarkdownizeRequest<'_>,
    verified_raw_bytes: &[u8],
) -> Result<StandardOnlineMarkdownizeOutcome> {
    // Test controls are process-global. Snapshot them before any request work so
    // an individual request cannot observe a mid-flight environment mutation.
    let (test_mode, capture_sent_media) = test_control::mistral_ocr_with_capture();
    let actual_hash = crate::identity::hash_bytes(verified_raw_bytes);
    if actual_hash != request.raw_hash {
        return Err(AdapterError::ContractViolation(format!(
            "online markdownize input identity changed: expected {}, got {actual_hash}",
            request.raw_hash
        )));
    }
    // 07 §5.1: the identity check above proved `verified_raw_bytes` is the
    // untampered ORIGINAL file content. For Office media, convert THAT verified
    // buffer to PDF now (never reopen/reread) -- the online adapter media gate
    // (`supports_ocr_from_scratch`, and the real Mistral client) only accepts
    // PDF/images, so Office bytes must never reach it directly. `send_raw_hash`
    // describes the CONVERTED bytes for the adapter's own internal tamper-check
    // (mistral_ocr.rs hashes what it is about to upload); it is purely internal
    // to this call -- output storage identity stays `request.raw_hash` (the
    // ORIGINAL file's raw_hash) at every caller, since callers persist under
    // `task.input_hash`/`raw_hash` directly and never read this function's
    // internal `adapter_request.raw.raw_hash` back out.
    let converted_pdf = is_office_media(request.media_type)
        .then(|| convert_office_to_pdf(verified_raw_bytes, request.media_type))
        .transpose()?;
    let effective_media_type: &str = if converted_pdf.is_some() {
        "application/pdf"
    } else {
        request.media_type
    };
    let send_bytes: &[u8] = converted_pdf.as_deref().unwrap_or(verified_raw_bytes);
    let send_raw_hash: String = converted_pdf
        .as_ref()
        .map(|bytes| crate::identity::hash_bytes(bytes))
        .unwrap_or_else(|| request.raw_hash.to_owned());
    if request.prepared_unit_hints.is_empty() {
        // Discovery is only a fresh, whole-document operation. Reject unsupported
        // media and contradictory retry/incremental fields before constructing a real
        // client, so no document can be uploaded or billed before the contract error.
        if request.mode != MarkdownizeMode::Full
            || request.previous.is_some()
            || request.hints.is_some()
            || request.restrict_to_hint_pages
        {
            return Err(AdapterError::ContractViolation(
                "OCR-from-scratch requires a fresh unrestricted Full request".to_owned(),
            ));
        }
        if !supports_ocr_from_scratch(effective_media_type) {
            return Err(AdapterError::ContractViolation(format!(
                "OCR-from-scratch media type is unsupported: {effective_media_type}"
            )));
        }
    }
    let requested_prepared_unit_hints = request.prepared_unit_hints.clone();
    let prepared_unit_hint =
        (!request.prepared_unit_hints.is_empty()).then_some(request.prepared_unit_hints);
    let adapter_request = MarkdownizeRequest {
        raw: RawInput {
            raw_hash: send_raw_hash.clone(),
            path: Some(request.path.display().to_string()),
        },
        media_type: effective_media_type.to_owned(),
        // An empty local Prepare result is meaningful for scanned PDFs and images:
        // the online OCR adapter must discover the page/unit set
        // from the provider response. Preserve that distinction as `None`; `Some([])`
        // previously forced the hint-driven path to return zero units forever.
        prepared_unit_hint,
        // R13-1: forward the caller-computed mode/previous/hints (was hard-coded
        // Full/None/None, which is why incremental never fired on the online route).
        mode: request.mode,
        previous: request.previous,
        hints: request.hints,
        // R15-5: forward the retry page-scoping signal to the real client.
        restrict_to_hint_pages: request.restrict_to_hint_pages,
        bbox_annotation_enabled: request.bbox_annotation_enabled,
        tool_profile_hash: String::new(),
        spec_version: 1,
        // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): thread the
        // caller's ledger `intent_token` through so the Adapter boundary can
        // enforce/attach a provider idempotency header when the resolved
        // profile declares one.
        idempotency_token: request.idempotency_token,
    };
    match test_mode {
        Some("auth_error") => return Err(AdapterError::Auth("mock auth failure".to_owned())),
        Some("rate_limit") => return Err(AdapterError::rate_limit("mock 429")),
        // QA3 (step4b-contract-tests-p3a.md §A, 04 §5.3): a rate limit WITH a
        // provider `Retry-After` header (30s), proving `retry_after_ms`
        // wiring end-to-end into `next_retry_at` — unlike the headerless
        // "rate_limit" seam above, which exercises the synthetic +2s
        // fallback.
        Some("rate_limit_after") => {
            return Err(AdapterError::RateLimit {
                message: "mock 429".to_owned(),
                retry_after_ms: Some(30_000),
            });
        }
        // R16-7: a retryable NetworkError (mapped from `AdapterError::Network`) — unlike
        // rate_limit it may have been billed server-side, so each retry re-reserves.
        Some("network_error") => {
            return Err(AdapterError::Network("mock network failure".to_owned()));
        }
        // R13-1: `incr_incomplete` simulates an OCR response that drops a requested
        // unit ONLY in incremental mode (a full re-send returns everything), so the
        // KIO-side acceptance check fails and the online route falls back to Full.
        Some("mock")
        | Some("partial")
        | Some("partial_network")
        | Some("all_failed_network")
        | Some("all_failed_invalid_input")
        | Some("mock_link_image")
        | Some("incr_incomplete")
        | Some("pin_changed")
        | Some("no_change_no_send") => {
            let client = MockStandardOnlineMarkdownizeClient {
                test_mode,
                capture_sent_media,
            };
            let model_pin = client.resolve_model_pin("mistral-ocr-latest")?;
            let adapter = MistralOcrMarkdownizeAdapter::new(client, model_pin, request.scope_id)
                .with_image_store(request.kio_dir)
                .with_bbox_annotation(request.bbox_annotation_enabled)
                .with_verified_raw_bytes(send_bytes.to_vec());
            let profile = adapter.profile();
            let mut adapter_request = adapter_request;
            adapter_request.tool_profile_hash = profile.tool_profile_hash.clone();
            let response_mode = adapter_request.mode;
            let mut response = adapter.markdownize(adapter_request)?;
            // Capture the complete discovered/requested unit set before the partial
            // response seams remove an output. Otherwise a simulated missing page would
            // disappear from the manifest instead of remaining a retryable Failed unit.
            let effective_prepared_unit_hints = effective_prepared_unit_hints(
                &requested_prepared_unit_hints,
                &send_raw_hash,
                &response,
            )?;
            // Test-only Kio response seams run after the provider page mapping has
            // passed its exact-bijection checks. `partial_network` preserves the
            // valid Partial lifecycle; `partial` deliberately remains a malformed
            // dropped-output response for contract-rejection coverage.
            if test_mode == Some("partial") {
                response.updated_units.pop();
            }
            if test_mode == Some("partial_network")
                && let Some(unit) = response.updated_units.pop()
            {
                response.failed_units.push(FailedUnit {
                    unit_key: unit.unit_key,
                    error_kind: "network_error".to_owned(),
                });
            }
            if let Some(error_kind) = match test_mode {
                Some("all_failed_network") => Some("network_error"),
                Some("all_failed_invalid_input") => Some("invalid_input"),
                _ => None,
            } {
                response
                    .failed_units
                    .extend(response.updated_units.drain(..).map(|unit| FailedUnit {
                        unit_key: unit.unit_key,
                        error_kind: error_kind.to_owned(),
                    }));
            }
            if test_mode == Some("incr_incomplete")
                && response_mode == MarkdownizeMode::Incremental
                && response.updated_units.len() > 1
            {
                response.updated_units.pop();
            }
            return Ok(StandardOnlineMarkdownizeOutcome {
                profile,
                response,
                effective_prepared_unit_hints,
            });
        }
        // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): behaves like
        // "mock", but the adapter declares `ProviderIdempotency::HttpHeader`
        // — proving the CLI threads `idempotency_token` end-to-end. A missing
        // token surfaces as `AdapterError::ContractViolation` (fail closed,
        // before any send); a present one lets the mock send succeed exactly
        // like "mock" does. Never true for the real, shipped Mistral profile
        // (`MistralOcrMarkdownizeAdapter::default` stays `NotProvided`).
        Some("require_idempotency_token") => {
            let client = MockStandardOnlineMarkdownizeClient {
                test_mode,
                capture_sent_media,
            };
            let model_pin = client.resolve_model_pin("mistral-ocr-latest")?;
            let adapter = MistralOcrMarkdownizeAdapter::new(client, model_pin, request.scope_id)
                .with_image_store(request.kio_dir)
                .with_bbox_annotation(request.bbox_annotation_enabled)
                .with_verified_raw_bytes(send_bytes.to_vec())
                .with_provider_idempotency(ProviderIdempotency::HttpHeader(
                    "Idempotency-Key".to_owned(),
                ));
            let profile = adapter.profile();
            let mut adapter_request = adapter_request;
            adapter_request.tool_profile_hash = profile.tool_profile_hash.clone();
            let response = adapter.markdownize(adapter_request)?;
            let effective_prepared_unit_hints = effective_prepared_unit_hints(
                &requested_prepared_unit_hints,
                &send_raw_hash,
                &response,
            )?;
            return Ok(StandardOnlineMarkdownizeOutcome {
                profile,
                response,
                effective_prepared_unit_hints,
            });
        }
        _ => {}
    }
    let client = EnvMistralOcrClient::new();
    // R13-2: use the declared `tools.toml` `[markdown] model` alias when present
    // (docs/03 §11: config may carry a mutable alias, resolved to an immutable pin
    // at execution — §5.1/§6), rather than the previously hard-coded
    // `"mistral-ocr-latest"`.
    let configured_model = declared_markdown_model()?;
    let model_pin = client.resolve_model_pin(&configured_model)?;
    let adapter = MistralOcrMarkdownizeAdapter::new(client, model_pin, request.scope_id)
        .with_image_store(request.kio_dir)
        .with_bbox_annotation(request.bbox_annotation_enabled)
        .with_verified_raw_bytes(send_bytes.to_vec());
    let profile = adapter.profile();
    let mut adapter_request = adapter_request;
    adapter_request.tool_profile_hash = profile.tool_profile_hash.clone();
    let response = adapter.markdownize(adapter_request)?;
    let effective_prepared_unit_hints =
        effective_prepared_unit_hints(&requested_prepared_unit_hints, &send_raw_hash, &response)?;
    Ok(StandardOnlineMarkdownizeOutcome {
        profile,
        response,
        effective_prepared_unit_hints,
    })
}

/// 07-adapter-spec.md §5.1: convert Office (DOCX/PPTX) `bytes` to PDF via the
/// resolved `OfficeConverter`, mapping EVERY failure mode (no converter
/// resolvable, or a runtime conversion fault) to `ContractViolation` -- "実行時の
/// 変換失敗は contract_violation ([04-pipeline.md §5.3] -- 同一入力の再試行 1 回) に
/// 合流する". Callers that must never enqueue/spend when no converter is
/// installed at all (the `kio index` enqueue gate) check
/// `resolve_office_converter().is_none()` THEMSELVES first and never call this
/// helper in that case; reaching here with no converter means the precondition
/// that led to this call (an earlier successful resolve, e.g. at enqueue time)
/// no longer holds -- a rare race this helper still turns into the same
/// well-defined retryable failure rather than a panic or a stuck task.
pub fn convert_office_to_pdf(bytes: &[u8], media_type: &str) -> Result<Vec<u8>> {
    let converter = resolve_office_converter().ok_or_else(|| {
        AdapterError::ContractViolation(format!(
            "office converter unavailable for {media_type} at execution time"
        ))
    })?;
    converter
        .convert_to_pdf(bytes, media_type)
        .map_err(|err| AdapterError::ContractViolation(format!("office conversion failed: {err}")))
}

fn supports_ocr_from_scratch(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/pdf" | "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    )
}

/// `pub` so the offline (Stage 3) route derives its effective hints the same
/// way the online one does. When a caller supplies no hints -- the
/// OCR-from-scratch case, which is every scanned PDF -- the unit identities
/// exist only in the adapter's response, and passing the empty request set
/// through to persistence makes the whole document look like zero units.
pub fn effective_prepared_unit_hints(
    requested: &[PreparedUnitHint],
    raw_hash: &str,
    response: &MarkdownizeResponse,
) -> Result<Vec<PreparedUnitHint>> {
    if !requested.is_empty() {
        return Ok(requested.to_vec());
    }
    if response.mode_used != crate::types::MarkdownizeMode::Full {
        return Err(AdapterError::ContractViolation(
            "OCR-from-scratch requires Full markdownize mode".to_owned(),
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    let hints = response
        .updated_units
        .iter()
        .chain(response.added_units.iter())
        .enumerate()
        .map(|(index, unit)| {
            if !seen.insert(unit.unit_key.clone()) {
                return Err(AdapterError::ContractViolation(
                    "OCR-from-scratch returned duplicate unit keys".to_owned(),
                ));
            }
            let order = u64::try_from(index).map_err(|_| {
                AdapterError::ContractViolation(
                    "OCR-from-scratch unit count exceeds the supported order".to_owned(),
                )
            })?;
            Ok(PreparedUnitHint {
                unit_key: unit.unit_key.clone(),
                prepared_hash: raw_hash.to_owned(),
                unit_kind: unit.unit_type,
                order,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if hints.is_empty() {
        return Err(AdapterError::ContractViolation(
            "OCR-from-scratch returned no units".to_owned(),
        ));
    }
    Ok(hints)
}

/// R13-2: the configured `[markdown] model` alias (or the built-in `mistral-ocr-latest`),
/// resolved to an immutable pin at execution. Shared by the send path and the
/// resolve-only profile path (R14-6) so both agree on the model.
fn declared_markdown_model() -> Result<String> {
    let declared = crate::tool_lock::registered_declared_adapter("markdown");
    if let Some(declared) = declared.as_ref() {
        crate::tool_lock::validate_declared_runtime_target("markdown", declared)?;
    }
    Ok(declared
        .and_then(|declared| declared.model)
        .unwrap_or_else(|| "mistral-ocr-latest".to_owned()))
}

/// R14-6: resolve the online markdownize adapter's profile (its `tool_profile_hash`)
/// WITHOUT sending an OCR request. Resolving the model pin may hit the network (a
/// `GET /v1/models` to expand a `*-latest` alias) but never uploads the document or bills
/// OCR. The incremental gate compares this *resolved* profile against the prior instance
/// BEFORE deciding to send: a changed pin is a different tool_profile, which is not an
/// eligible incremental (docs/04 §3.1 condition 2), so Kio must fall straight to a Full
/// send instead of wasting an incremental send (and, post-R14-4, a full-document upload)
/// only to discard it. Keep the seam arms in sync with `run_standard_online_markdownize`.
pub fn resolve_standard_online_markdownize_profile(scope_id: &str) -> Result<AdapterProfile> {
    resolve_standard_online_markdownize_profile_with_bbox(scope_id, true)
}

pub fn resolve_standard_online_markdownize_profile_with_bbox(
    scope_id: &str,
    bbox_annotation_enabled: bool,
) -> Result<AdapterProfile> {
    let test_mode = test_control::mistral_ocr();
    match test_mode {
        Some("auth_error") => return Err(AdapterError::Auth("mock auth failure".to_owned())),
        Some("rate_limit") => return Err(AdapterError::rate_limit("mock 429")),
        // QA3: keep the seam arms in sync with `run_standard_online_markdownize`.
        Some("rate_limit_after") => {
            return Err(AdapterError::RateLimit {
                message: "mock 429".to_owned(),
                retry_after_ms: Some(30_000),
            });
        }
        // R16-7: keep the seam arms in sync with `run_standard_online_markdownize`.
        Some("network_error") => {
            return Err(AdapterError::Network("mock network failure".to_owned()));
        }
        Some("mock")
        | Some("partial")
        | Some("partial_network")
        | Some("all_failed_network")
        | Some("all_failed_invalid_input")
        | Some("mock_link_image")
        | Some("incr_incomplete")
        | Some("pin_changed")
        | Some("no_change_no_send") => {
            let client = MockStandardOnlineMarkdownizeClient {
                test_mode,
                capture_sent_media: None,
            };
            let model_pin = client.resolve_model_pin("mistral-ocr-latest")?;
            return Ok(
                MistralOcrMarkdownizeAdapter::new(client, model_pin, scope_id)
                    .with_bbox_annotation(bbox_annotation_enabled)
                    .profile(),
            );
        }
        // QA13: keep the seam arms in sync with `run_standard_online_markdownize`.
        Some("require_idempotency_token") => {
            let client = MockStandardOnlineMarkdownizeClient {
                test_mode,
                capture_sent_media: None,
            };
            let model_pin = client.resolve_model_pin("mistral-ocr-latest")?;
            return Ok(
                MistralOcrMarkdownizeAdapter::new(client, model_pin, scope_id)
                    .with_bbox_annotation(bbox_annotation_enabled)
                    .with_provider_idempotency(ProviderIdempotency::HttpHeader(
                        "Idempotency-Key".to_owned(),
                    ))
                    .profile(),
            );
        }
        _ => {}
    }
    let client = EnvMistralOcrClient::new();
    let model_pin = client.resolve_model_pin(&declared_markdown_model()?)?;
    Ok(
        MistralOcrMarkdownizeAdapter::new(client, model_pin, scope_id)
            .with_bbox_annotation(bbox_annotation_enabled)
            .profile(),
    )
}

#[derive(Debug, Clone)]
struct MockStandardOnlineMarkdownizeClient {
    test_mode: Option<&'static str>,
    capture_sent_media: Option<std::path::PathBuf>,
}

// A decodable, deterministic 1x1 RGB PNG. Distinct unit keys retain distinct
// image content so mock OCR exercises content-addressed image deduplication.
fn mock_ocr_png(unit_key: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};

    fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], bytes: &[u8]) {
        png.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(bytes);
        let mut crc = !0u32;
        for byte in kind.iter().chain(bytes) {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        png.extend_from_slice(&(!crc).to_be_bytes());
    }

    let color = Sha256::digest(unit_key.as_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0]);
    let pixels = miniz_oxide::deflate::compress_to_vec_zlib(&[0, color[0], color[1], color[2]], 6);
    chunk(&mut png, b"IDAT", &pixels);
    chunk(&mut png, b"IEND", &[]);
    png
}

impl MistralOcrClient for MockStandardOnlineMarkdownizeClient {
    fn resolve_model_pin(&self, _configured_model: &str) -> Result<String> {
        // R14-6: the `pin_changed` seam simulates a model-pin change between runs so the
        // incremental gate sees a resolved tool_profile different from the prior instance
        // (which was created under the default `mistral-ocr-2505`).
        if self.test_mode == Some("pin_changed") {
            return Ok("mistral-ocr-2599".to_owned());
        }
        Ok("mistral-ocr-2505".to_owned())
    }

    fn ocr_markdown(
        &self,
        request: &MarkdownizeRequest,
        model_pin: &str,
        verified_raw_bytes: &[u8],
        _idempotency_header: Option<(&str, &str)>,
    ) -> Result<OcrResponse> {
        // Step4b office contract (07 §5.1 item 3): when set, the env var's
        // VALUE is a file path this test-only mock writes
        // "<media_type>\n<starts_with_%PDF magic>\n" to -- the only way a
        // black-box CLI test can observe what media_type/bytes actually
        // crossed the (mocked) OCR client boundary (proving Office media was
        // converted to `application/pdf` before this call, never the
        // original OOXML bytes). Gated behind this env var so it never
        // perturbs the exact-match markdown-TEXT assertions other existing
        // tests already make against this mock's ordinary output.
        if let Some(capture_path) = &self.capture_sent_media {
            let _ = std::fs::write(
                capture_path,
                format!(
                    "{}\n{}\n",
                    request.media_type,
                    verified_raw_bytes.starts_with(b"%PDF")
                ),
            );
        }
        // R14-6: under `pin_changed`, an INCREMENTAL send must never reach the adapter —
        // the gate resolves the changed pin first and falls back to Full. If one is
        // attempted (a regression that sends before gating), fail loudly so the test
        // catches it. A Full send under the same seam is the expected fallback and works.
        if self.test_mode == Some("pin_changed")
            && request.mode == crate::types::MarkdownizeMode::Incremental
        {
            return Err(AdapterError::ContractViolation(
                "R14-6: incremental OCR sent after a model-pin change (gate missing)".to_owned(),
            ));
        }
        // R15-6: under `no_change_no_send`, a 0-change incremental must reuse every unit
        // KIO-side WITHOUT reaching the adapter (there is no page to OCR). Unlike
        // `pin_changed`, the model pin is left stable so incremental actually FIRES (the
        // gate passes) — the only incremental send this seam sees is the regression it
        // guards. Fail loudly so the test catches any send.
        if self.test_mode == Some("no_change_no_send")
            && request.mode == crate::types::MarkdownizeMode::Incremental
        {
            return Err(AdapterError::ContractViolation(
                "R15-6: a 0-change incremental reached the adapter (should reuse without sending)"
                    .to_owned(),
            ));
        }
        let discovered = request
            .prepared_unit_hint
            .as_deref()
            .is_none_or(<[_]>::is_empty);
        let discovered_hint = crate::types::PreparedUnitHint {
            unit_key: "discovered:1".to_owned(),
            prepared_hash: request.raw.raw_hash.clone(),
            unit_kind: crate::types::UnitKind::Page,
            order: 0,
        };
        let hints = if discovered {
            std::slice::from_ref(&discovered_hint)
        } else {
            request.prepared_unit_hint.as_deref().unwrap_or(&[])
        };
        let pages: Vec<OcrPage> = hints
            .iter()
            .map(|hint| {
                // R11-6: index each returned page by the unit's DOCUMENT order
                // (`hint.order`), not its position in the hint list, so a
                // unit-scoped retry (a subset of hints) maps back correctly — the
                // adapter looks pages up by `hint.order` (mistral_ocr.rs). The real
                // OCR API likewise returns pages at their document indices. For a
                // full send `order` == list position, so this is unchanged.
                let index = hint.order as usize;
                OcrPage {
                    index,
                    markdown: if self.test_mode == Some("mock_link_image")
                    {
                        format!(
                            "[source](https://example.com/{index}) mock ocr {} ![img-{index}](img-{index}.png)\n",
                            hint.unit_key
                        )
                    } else {
                        format!(
                            "mock ocr {} ![img-{index}](img-{index}.png)\n",
                            hint.unit_key
                        )
                    },
                    images: vec![OcrImage {
                        bytes: mock_ocr_png(&hint.unit_key),
                        media_type: "image/png".to_owned(),
                        bbox: Some([index as i64, 0, index as i64 + 1, 1]),
                        confidence: Some("0.99".to_owned()),
                        annotation: request.bbox_annotation_enabled.then(|| {
                            let text = format!(
                                "Kio bbox label {} value 1000",
                                hint.unit_key
                            );
                            crate::bbox_annotation::BboxAnnotation {
                                short_description: "mock chart".to_owned(),
                                transcribed_text:
                                    crate::bbox_annotation::canonical_source_escape(&text),
                            }
                        }),
                    }],
                }
            })
            .collect();
        Ok(OcrResponse {
            pages,
            model_version_pin: model_pin.to_owned(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptedEmbeddingExecution {
    #[cfg(debug_assertions)]
    Mock,
    #[cfg(debug_assertions)]
    IncompatibleProfile,
    #[cfg(debug_assertions)]
    NonMultimodal,
    #[cfg(debug_assertions)]
    AuthError,
    #[cfg(debug_assertions)]
    RateLimit,
    /// QA3 (step4b-contract-tests-p3a.md §A, 04 §5.3): a rate limit WITH a
    /// provider `Retry-After` header (30s) — proves `retry_after_ms` wiring
    /// into `next_retry_at`, unlike `RateLimit` above (headerless, synthetic
    /// +2s backoff).
    #[cfg(debug_assertions)]
    RateLimitAfter,
    #[cfg(debug_assertions)]
    NetworkError,
    /// QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): behaves like
    /// `Mock`, but the adapter declares `ProviderIdempotency::HttpHeader` —
    /// proving the CLI threads `EmbeddingRequest.idempotency_token`
    /// end-to-end (a missing token surfaces as `ContractViolation`). Never
    /// true for the real, shipped Gemini profile.
    #[cfg(debug_assertions)]
    RequireIdempotencyToken,
    /// I12: behaves like `Mock`, but the response carries no
    /// `usageMetadata` — the one case where a settle site must fall back to
    /// the reservation estimate. Kept as its own variant because `Mock` now
    /// reports a token count like the live endpoint does, so without this the
    /// degrade path would have no coverage at all.
    #[cfg(debug_assertions)]
    NoUsageReport,
    Real,
}

/// Which embedding implementation is active, and therefore which posture the
/// caller must take toward consent, billing, and send lanes.
///
/// [`AdoptedEmbeddingExecution`] above answers a narrower question — *which
/// Gemini test seam* — and every one of its variants builds a
/// `GeminiEmbeddingAdapter`. It was standing in for "which adapter" because
/// there was only ever one. This is the selector that actually distinguishes
/// implementations; keeping them separate is what leaves the 21 existing
/// `KIO_TEST_GEMINI_EMBED` call sites untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingExecution {
    /// The adopted online adapter, under one of its seams (or `Real`).
    Online(AdoptedEmbeddingExecution),
    /// The `offline_api` adapter, reached over loopback (07 §3).
    Offline(LocalEmbeddingExecution),
    /// The Rust-only evaluator adapter, selected only by the exact evaluator
    /// environment contract.
    DeterministicEvaluator,
}

impl EmbeddingExecution {
    #[must_use]
    pub fn execution_mode(self) -> ExecutionMode {
        match self {
            Self::Online(_) => ExecutionMode::OnlineApi,
            Self::Offline(_) => ExecutionMode::OfflineApi,
            Self::DeterministicEvaluator => ExecutionMode::DeterministicLibrary,
        }
    }

    /// Whether this send leaves the machine — the question 07 §3's consent
    /// gate, the cost ledger, and the batch lane all actually mean to ask.
    /// Call sites used to spell it `execution == Some(Real)`, which conflated
    /// "is online" with "is not a test seam".
    #[must_use]
    pub fn is_online(self) -> bool {
        self.execution_mode() == ExecutionMode::OnlineApi
    }

    /// The Gemini seam, when this is the online adapter. `None` offline.
    #[must_use]
    pub fn online_seam(self) -> Option<AdoptedEmbeddingExecution> {
        match self {
            Self::Online(seam) => Some(seam),
            Self::Offline(_) | Self::DeterministicEvaluator => None,
        }
    }
}

/// Resolves the active embedding implementation, or `None` when none is
/// configured — in which case the existing degradations apply unchanged
/// (search falls back to text, no embedding tasks are generated).
///
/// Shaped after [`crate::gemini_batch_client::resolve_gemini_batch_client`]:
/// resolve to `Option`, let `None` mean "this lane is unavailable, degrade".
#[must_use]
pub fn active_embedding_execution() -> Option<EmbeddingExecution> {
    match std::env::var_os(DETERMINISTIC_EVAL_EMBEDDING_ENV) {
        Some(value) if value == DETERMINISTIC_EVAL_EMBEDDING_PROFILE => {
            return Some(EmbeddingExecution::DeterministicEvaluator);
        }
        // A supplied but unknown evaluator selector must not silently select a
        // live or declared backend. The public resolver currently represents
        // unavailability as `None`, so this is its fail-closed outcome.
        Some(_) => return None,
        None => {}
    }
    #[cfg(debug_assertions)]
    let adapters = crate::debug_test_control().adapters;
    if let Some(local) = active_local_embedding_execution() {
        return Some(EmbeddingExecution::Offline(local));
    }
    active_adopted_embedding_execution_from(
        #[cfg(debug_assertions)]
        Some(&adapters),
    )
    .map(EmbeddingExecution::Online)
}

fn active_local_embedding_execution() -> Option<LocalEmbeddingExecution> {
    declared_offline_embedding().then_some(LocalEmbeddingExecution::Real)
}

/// Whether `tools.toml` declares the embedding role as `offline_api`.
fn declared_offline_embedding() -> bool {
    crate::tool_lock::registered_declared_adapter("embedding")
        .and_then(|declared| declared.kind)
        .is_some_and(|kind| kind == "offline_api")
}

/// Builds the adapter for a resolved execution.
///
/// The `EmbeddingAdapter` trait was already the right abstraction — it carries
/// `profile()` (hence `execution_mode`, `allow_network`, `billable_kinds`) and
/// `preferred_request_kind()`, which is every input the offline forks need. It
/// simply had one implementor. Note the seam is at the *adapter* level and not
/// at `GeminiEmbeddingClient`, whose `resolve_model_pin` and
/// `(model_pin, dimensions, idempotency_header)` signature encode Gemini's
/// model catalog and `outputDimensionality` and describe no other provider.
pub fn embedding_adapter_for(execution: EmbeddingExecution) -> Result<Box<dyn EmbeddingAdapter>> {
    match execution {
        EmbeddingExecution::DeterministicEvaluator => Ok(Box::new(DeterministicEmbeddingAdapter)),
        // Same shape as the online `Real` arm below: the declaration is
        // revalidated here rather than trusted from load time, and the parts
        // only the real backend needs (url, model) are read at the point of
        // construction so the execution enum can stay a `Copy` unit.
        EmbeddingExecution::Offline(LocalEmbeddingExecution::Real) => {
            let declared =
                crate::tool_lock::registered_declared_adapter("embedding").ok_or_else(|| {
                    AdapterError::ConfigSchema(
                        "the offline embedding adapter resolved without a declaration".to_owned(),
                    )
                })?;
            // Re-runs D1's literal-loopback check. An `offline_api` entry that
            // passed at load time and points somewhere else now must not send.
            crate::tool_lock::validate_declared_runtime_target("embedding", &declared)?;
            let base_url = declared.url.clone().ok_or_else(|| {
                AdapterError::ConfigSchema(
                    "an offline_api embedding adapter must declare `url`".to_owned(),
                )
            })?;
            let endpoint = crate::tool_lock::authenticated_local_endpoint("embedding", &base_url)?;
            let model = declared
                .model
                .clone()
                .unwrap_or_else(|| LOCAL_EMBEDDING_DEFAULT_MODEL.to_owned());
            // D7: `[adapter.policy.offline_api].timeout_seconds` (07 §7). Read
            // at construction like `url` and `model` above, and absent means
            // inherit the parent's documented 300 rather than a value invented
            // here -- 07 §7 defers the offline default to Stage 3's measurement.
            let timeout_seconds = crate::tool_lock::registered_execution_timeout("offline_api");
            Ok(Box::new(LocalEmbeddingAdapter::with_client(
                crate::local_embedding::EnvLocalEmbeddingClient::new(
                    endpoint,
                    model,
                    timeout_seconds,
                ),
            )))
        }
        EmbeddingExecution::Online(AdoptedEmbeddingExecution::Real) => {
            let declared = crate::tool_lock::registered_declared_adapter("embedding");
            if let Some(declared) = declared.as_ref() {
                crate::tool_lock::validate_declared_runtime_target("embedding", declared)?;
            }
            let configured_model = declared
                .and_then(|declared| declared.model)
                .unwrap_or_else(|| ADOPTED_MODEL_PIN.to_owned());
            Ok(Box::new(GeminiEmbeddingAdapter::new(
                crate::gemini_embedding::EnvGeminiEmbeddingClient::new(),
                configured_model,
                ADOPTED_DIMENSIONS,
            )))
        }
        #[cfg(debug_assertions)]
        EmbeddingExecution::Online(AdoptedEmbeddingExecution::RequireIdempotencyToken) => {
            Ok(Box::new(
                GeminiEmbeddingAdapter::new(
                    MockAdoptedEmbeddingClient {
                        execution: AdoptedEmbeddingExecution::RequireIdempotencyToken,
                    },
                    ADOPTED_MODEL_PIN,
                    ADOPTED_DIMENSIONS,
                )
                .with_provider_idempotency(ProviderIdempotency::HttpHeader(
                    "Idempotency-Key".to_owned(),
                )),
            ))
        }
        #[cfg(debug_assertions)]
        EmbeddingExecution::Online(other) => Ok(Box::new(GeminiEmbeddingAdapter::new(
            MockAdoptedEmbeddingClient { execution: other },
            ADOPTED_MODEL_PIN,
            ADOPTED_DIMENSIONS,
        ))),
    }
}

#[must_use]
pub fn active_adopted_embedding_execution() -> Option<AdoptedEmbeddingExecution> {
    #[cfg(debug_assertions)]
    let adapters = crate::debug_test_control().adapters;
    active_adopted_embedding_execution_from(
        #[cfg(debug_assertions)]
        Some(&adapters),
    )
}

fn active_adopted_embedding_execution_from(
    #[cfg(debug_assertions)] adapters: Option<&kio_core::test_control::AdapterTestControl>,
) -> Option<AdoptedEmbeddingExecution> {
    #[cfg(debug_assertions)]
    match adapters.and_then(|adapters| match &adapters.gemini_embed {
        kio_core::test_control::Selector::Known(mode) => Some(*mode),
        kio_core::test_control::Selector::Unset | kio_core::test_control::Selector::Unknown(_) => {
            None
        }
    }) {
        Some(kio_core::test_control::GeminiEmbedMode::Mock) => {
            Some(AdoptedEmbeddingExecution::Mock)
        }
        Some(kio_core::test_control::GeminiEmbedMode::IncompatibleProfile) => {
            Some(AdoptedEmbeddingExecution::IncompatibleProfile)
        }
        Some(kio_core::test_control::GeminiEmbedMode::NonMultimodal) => {
            Some(AdoptedEmbeddingExecution::NonMultimodal)
        }
        Some(kio_core::test_control::GeminiEmbedMode::AuthError) => {
            Some(AdoptedEmbeddingExecution::AuthError)
        }
        Some(kio_core::test_control::GeminiEmbedMode::RateLimit) => {
            Some(AdoptedEmbeddingExecution::RateLimit)
        }
        Some(kio_core::test_control::GeminiEmbedMode::RateLimitAfter) => {
            Some(AdoptedEmbeddingExecution::RateLimitAfter)
        }
        Some(kio_core::test_control::GeminiEmbedMode::NetworkError) => {
            Some(AdoptedEmbeddingExecution::NetworkError)
        }
        Some(kio_core::test_control::GeminiEmbedMode::RequireIdempotencyToken) => {
            Some(AdoptedEmbeddingExecution::RequireIdempotencyToken)
        }
        Some(kio_core::test_control::GeminiEmbedMode::NoUsageReport) => {
            Some(AdoptedEmbeddingExecution::NoUsageReport)
        }
        // The declared tools.toml credential is the sole activation authority.
        None => real_embedding_activation(
            crate::tool_lock::registered_declared_adapter("embedding")
                .and_then(|declared| declared.auth)
                .is_some(),
        ),
    }
    #[cfg(not(debug_assertions))]
    {
        real_embedding_activation(
            crate::tool_lock::registered_declared_adapter("embedding")
                .and_then(|declared| declared.auth)
                .is_some(),
        )
    }
}

/// R13-2: pure activation rule for the adopted embedding adapter (unit-testable).
/// Real when the adapter declares credentials in `tools.toml`.
#[must_use]
pub fn real_embedding_activation(declared: bool) -> Option<AdoptedEmbeddingExecution> {
    declared.then_some(AdoptedEmbeddingExecution::Real)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredEmbeddingProfile {
    pub tool_id: String,
    pub dimensions: u64,
    pub distance: String,
    pub modality: String,
    pub profile_hash: String,
}

/// The lock-facing profile for whichever implementation is active.
///
/// The offline adapter reports its own identity — a different
/// `profile_hash` from the online one, which is exactly what 03 §7's
/// compatibility gate needs in order to refuse to mix the two vector spaces.
#[must_use]
pub fn declared_embedding_profile_for(execution: EmbeddingExecution) -> DeclaredEmbeddingProfile {
    match execution {
        EmbeddingExecution::Online(seam) => declared_adopted_embedding_profile(seam),
        EmbeddingExecution::Offline(local) => {
            let profile = crate::local_embedding::profile_for(local);
            DeclaredEmbeddingProfile {
                tool_id: profile.adapter_id,
                dimensions: u64::from(crate::local_embedding::LOCAL_EMBEDDING_DIMENSIONS),
                distance: "cosine".to_owned(),
                modality: "multimodal".to_owned(),
                profile_hash: profile.tool_profile_hash,
            }
        }
        EmbeddingExecution::DeterministicEvaluator => {
            let profile = DeterministicEmbeddingAdapter.profile();
            DeclaredEmbeddingProfile {
                tool_id: profile.adapter_id,
                dimensions: u64::from(
                    crate::deterministic_embedding::DETERMINISTIC_EVAL_EMBEDDING_DIMENSIONS,
                ),
                distance: "cosine".to_owned(),
                modality: "multimodal".to_owned(),
                profile_hash: profile.tool_profile_hash,
            }
        }
    }
}

#[must_use]
pub fn declared_adopted_embedding_profile(
    execution: AdoptedEmbeddingExecution,
) -> DeclaredEmbeddingProfile {
    let adopted = adopted_embedding_profile();
    match execution {
        #[cfg(debug_assertions)]
        AdoptedEmbeddingExecution::IncompatibleProfile => DeclaredEmbeddingProfile {
            tool_id: "test_embedding_incompatible".to_owned(),
            dimensions: ADOPTED_DIMENSIONS as u64,
            distance: "cosine".to_owned(),
            modality: "multimodal".to_owned(),
            profile_hash: "sha256:00000000000000000000000000000000000000000000000000000000incompat"
                .to_owned(),
        },
        #[cfg(debug_assertions)]
        AdoptedEmbeddingExecution::NonMultimodal => DeclaredEmbeddingProfile {
            tool_id: "test_text_embedding".to_owned(),
            dimensions: ADOPTED_DIMENSIONS as u64,
            distance: "cosine".to_owned(),
            modality: "text".to_owned(),
            profile_hash: adopted.tool_profile_hash,
        },
        _ => DeclaredEmbeddingProfile {
            tool_id: adopted.adapter_id,
            dimensions: ADOPTED_DIMENSIONS as u64,
            distance: "cosine".to_owned(),
            modality: "multimodal".to_owned(),
            profile_hash: adopted.tool_profile_hash,
        },
    }
}

/// What one adopted-embedding send produced: the vectors, plus the usage the
/// adapter self-reported for THIS call.
///
/// I12: this used to be a bare `Vec<EmbeddingVector>`, so `response.usage` was
/// discarded at the crate boundary. The Batch lane grew its own token-count
/// path and left both sync settle sites pricing themselves from the
/// reservation estimate — while four doc comments went on asserting the
/// endpoint reports no token count, which stopped being true the moment the
/// adapter started populating `usage`. Carrying it here means a caller can no
/// longer settle a sync send without at least deciding what to do with the
/// real number.
pub struct AdoptedEmbeddingOutcome {
    pub vectors: Vec<EmbeddingVector>,
    pub usage: Option<AdapterUsage>,
}

/// Runs whichever embedding implementation is active over one batch.
///
/// Replaces the per-seam `match` that used to build a `GeminiEmbeddingAdapter`
/// in every arm; construction now lives in [`embedding_adapter_for`] and this
/// is the trait call plus the shared acceptance checks.
pub fn run_embedding(
    execution: EmbeddingExecution,
    items: Vec<EmbeddingItem>,
    input_type: EmbeddingInputType,
    idempotency_token: Option<String>,
) -> Result<AdoptedEmbeddingOutcome> {
    let adapter = embedding_adapter_for(execution)?;
    let response = adapter.embed(EmbeddingRequest {
        input_type,
        items,
        idempotency_token,
    })?;
    Ok(AdoptedEmbeddingOutcome {
        vectors: response.vectors,
        usage: response.usage,
    })
}

/// The send lane the active implementation prefers (07 §5.5). The offline
/// adapter has no batch lane to prefer — there is no provider job queue — so
/// this is what keeps `poll_batch_embedding_jobs` from running for it.
pub fn embedding_preferred_request_kind(
    execution: EmbeddingExecution,
) -> Result<crate::traits::PreferredRequestKind> {
    Ok(embedding_adapter_for(execution)?.preferred_request_kind())
}

/// The active implementation's `AdapterProfile` — the source for
/// `execution_mode`, `allow_network`, and `billable_kinds`, which the consent,
/// ledger, and lane decisions all key off.
pub fn embedding_adapter_profile(execution: EmbeddingExecution) -> Result<AdapterProfile> {
    Ok(embedding_adapter_for(execution)?.profile())
}

#[cfg(debug_assertions)]
pub fn run_adopted_embedding(
    execution: AdoptedEmbeddingExecution,
    items: Vec<EmbeddingItem>,
    input_type: EmbeddingInputType,
    // QA13 (step4b-contract-tests-p3a.md §E, 04 §5.5 L880): the caller's
    // ledger `intent_token` for this send — see
    // `EmbeddingRequest::idempotency_token`'s doc. `None` for a call with no
    // ledger charge.
    idempotency_token: Option<String>,
) -> Result<AdoptedEmbeddingOutcome> {
    let request = EmbeddingRequest {
        input_type,
        items,
        idempotency_token,
    };
    let response = match execution {
        AdoptedEmbeddingExecution::Real => {
            let declared = crate::tool_lock::registered_declared_adapter("embedding");
            if let Some(declared) = declared.as_ref() {
                crate::tool_lock::validate_declared_runtime_target("embedding", declared)?;
            }
            let configured_model = declared
                .and_then(|declared| declared.model)
                .unwrap_or_else(|| ADOPTED_MODEL_PIN.to_owned());
            GeminiEmbeddingAdapter::new(
                crate::gemini_embedding::EnvGeminiEmbeddingClient::new(),
                configured_model,
                ADOPTED_DIMENSIONS,
            )
            .embed(request)
        }
        // QA13: behaves like the generic mock arm below, but the adapter
        // declares a provider idempotency header so a missing
        // `idempotency_token` surfaces as `ContractViolation` — proving the
        // CLI threads it end-to-end.
        AdoptedEmbeddingExecution::RequireIdempotencyToken => GeminiEmbeddingAdapter::new(
            MockAdoptedEmbeddingClient { execution },
            ADOPTED_MODEL_PIN,
            ADOPTED_DIMENSIONS,
        )
        .with_provider_idempotency(ProviderIdempotency::HttpHeader(
            "Idempotency-Key".to_owned(),
        ))
        .embed(request),
        other => GeminiEmbeddingAdapter::new(
            MockAdoptedEmbeddingClient { execution: other },
            ADOPTED_MODEL_PIN,
            ADOPTED_DIMENSIONS,
        )
        .embed(request),
    }?;
    Ok(AdoptedEmbeddingOutcome {
        vectors: response.vectors,
        usage: response.usage,
    })
}

#[cfg(not(debug_assertions))]
pub fn run_adopted_embedding(
    execution: AdoptedEmbeddingExecution,
    items: Vec<EmbeddingItem>,
    input_type: EmbeddingInputType,
    idempotency_token: Option<String>,
) -> Result<AdoptedEmbeddingOutcome> {
    debug_assert_eq!(execution, AdoptedEmbeddingExecution::Real);
    run_embedding(
        EmbeddingExecution::Online(AdoptedEmbeddingExecution::Real),
        items,
        input_type,
        idempotency_token,
    )
}

#[must_use]
pub fn deterministic_embedding_vector(seed: &str, dimensions: usize) -> Vec<f32> {
    let mut values = Vec::with_capacity(dimensions);
    let mut counter = 0u32;
    while values.len() < dimensions {
        let mut hasher = Sha256::new();
        hasher.update(seed.as_bytes());
        hasher.update(counter.to_le_bytes());
        let digest = hasher.finalize();
        for chunk in digest.as_chunks::<4>().0 {
            if values.len() >= dimensions {
                break;
            }
            let bits = u32::from_le_bytes(*chunk);
            values.push((bits as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32);
        }
        counter += 1;
    }
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in &mut values {
            *value /= norm;
        }
    }
    values
}

#[derive(Debug, Clone, Copy)]
#[cfg(debug_assertions)]
struct MockAdoptedEmbeddingClient {
    execution: AdoptedEmbeddingExecution,
}

#[cfg(debug_assertions)]
impl GeminiEmbeddingClient for MockAdoptedEmbeddingClient {
    fn resolve_model_pin(&self, _configured_model: &str) -> Result<String> {
        Ok(ADOPTED_MODEL_PIN.to_owned())
    }

    fn embed(
        &self,
        items: &[EmbeddingItem],
        _model_pin: &str,
        dimensions: u32,
        _idempotency_header: Option<(&str, &str)>,
    ) -> Result<crate::gemini_embedding::EmbedBatchOutput> {
        match self.execution {
            AdoptedEmbeddingExecution::AuthError => {
                return Err(AdapterError::Auth("mock auth failure".to_owned()));
            }
            AdoptedEmbeddingExecution::RateLimit => {
                return Err(AdapterError::rate_limit("mock 429"));
            }
            AdoptedEmbeddingExecution::RateLimitAfter => {
                return Err(AdapterError::RateLimit {
                    message: "mock 429".to_owned(),
                    retry_after_ms: Some(30_000),
                });
            }
            AdoptedEmbeddingExecution::NetworkError => {
                return Err(AdapterError::Network("mock network failure".to_owned()));
            }
            _ => {}
        }
        Ok(crate::gemini_embedding::EmbedBatchOutput {
            vectors: items
                .iter()
                .map(|item| EmbeddingVector {
                    id: item.id.clone(),
                    vector: match &item.content {
                        crate::types::EmbeddingContent::Text { text } => {
                            deterministic_embedding_vector(text, dimensions as usize)
                        }
                        crate::types::EmbeddingContent::Image { bytes, .. } => {
                            // The test seam cannot issue an online multimodal request,
                            // but image bytes still determine its synthetic vector.
                            let seed =
                                format!("image:sha256:{}", crate::identity::hash_bytes(bytes));
                            deterministic_embedding_vector(&seed, dimensions as usize)
                        }
                    },
                })
                .collect(),
            // I12: this used to be `None` on the reasoning that "a mock cannot
            // invent a provider's token count". The live endpoint always
            // returns `usageMetadata.promptTokenCount`, so a seam that reports
            // nothing is not the honest case — it is a shape the provider never
            // sends, and it silently pins every mock-driven settlement to the
            // degrade-to-estimate branch. That is the I3 failure again: a mock
            // that disagrees with the wire makes its own tests vacuous. The
            // count is derived from the input so it is deterministic and moves
            // when the input does; `AdoptedEmbeddingExecution::NoUsageReport`
            // still exercises the degrade path deliberately.
            prompt_tokens: (self.execution != AdoptedEmbeddingExecution::NoUsageReport).then(
                || {
                    items
                        .iter()
                        .map(|item| match &item.content {
                            crate::types::EmbeddingContent::Text { text } => {
                                text.chars().count() as u64
                            }
                            crate::types::EmbeddingContent::Image { bytes, .. } => {
                                bytes.len() as u64
                            }
                        })
                        .sum()
                },
            ),
        })
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::types::AdapterKind;

    #[test]
    fn adopted_profiles_are_catalog_owned() {
        assert_eq!(builtin_prepare_profile().adapter_id, "prepare_default");
        assert_eq!(
            standard_online_markdownize_profile().adapter_id,
            "mistral_ocr_markdownize"
        );
        assert_eq!(adopted_embedding_profile().adapter_id, "gemini_embedding_2");
    }

    // A declared `[embedding]` credential activates the Real path.
    #[test]
    fn real_embedding_activation_requires_declaration() {
        assert_eq!(
            real_embedding_activation(true),
            Some(AdoptedEmbeddingExecution::Real),
            "a declared adapter activates"
        );
        assert_eq!(
            real_embedding_activation(false),
            None,
            "an undeclared adapter remains inactive"
        );
    }

    #[test]
    fn deterministic_embedding_vector_is_stable_and_normalized() {
        let a = deterministic_embedding_vector("認証仕様 トークン", 768);
        let b = deterministic_embedding_vector("認証仕様 トークン", 768);
        assert_eq!(a, b);
        let norm = a.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.0001);

        let other = deterministic_embedding_vector("別のクエリ", 768);
        assert_ne!(a, other);
    }

    #[test]
    fn deterministic_evaluator_activation_is_exact_and_fail_closed() {
        let _env_lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let _selector = kio_core::test_control::TestEnvGuard::set(
            DETERMINISTIC_EVAL_EMBEDDING_ENV,
            DETERMINISTIC_EVAL_EMBEDDING_PROFILE,
        );
        assert_eq!(
            active_embedding_execution(),
            Some(EmbeddingExecution::DeterministicEvaluator)
        );
        let adapter = embedding_adapter_for(EmbeddingExecution::DeterministicEvaluator).unwrap();
        assert_eq!(
            adapter.profile().execution_mode,
            ExecutionMode::DeterministicLibrary
        );
        drop(_selector);

        let _invalid = kio_core::test_control::TestEnvGuard::set(
            DETERMINISTIC_EVAL_EMBEDDING_ENV,
            "not-a-profile",
        );
        assert_eq!(active_embedding_execution(), None);
    }

    #[test]
    fn declared_adopted_embedding_profile_uses_adopted_profile_by_default() {
        let declared = declared_adopted_embedding_profile(AdoptedEmbeddingExecution::Mock);
        let adopted = adopted_embedding_profile();
        assert_eq!(declared.tool_id, adopted.adapter_id);
        assert_eq!(declared.profile_hash, adopted.tool_profile_hash);
        assert_eq!(declared.modality, "multimodal");
        assert_eq!(declared.dimensions, 768);
    }

    #[test]
    fn run_adopted_embedding_mock_returns_one_vector_per_item() {
        let outcome = run_adopted_embedding(
            AdoptedEmbeddingExecution::Mock,
            vec![
                EmbeddingItem::text("a", "hello"),
                EmbeddingItem::text("b", "world"),
            ],
            EmbeddingInputType::MarkdownChunk,
            None,
        )
        .unwrap();
        assert_eq!(outcome.vectors.len(), 2);
        assert_eq!(outcome.vectors[0].id, "a");
        assert_eq!(outcome.vectors[1].id, "b");
    }

    #[test]
    fn run_adopted_embedding_mock_errors_are_classified() {
        assert!(matches!(
            run_adopted_embedding(
                AdoptedEmbeddingExecution::AuthError,
                Vec::new(),
                EmbeddingInputType::Query,
                None,
            ),
            Err(AdapterError::Auth(_))
        ));
        assert!(matches!(
            run_adopted_embedding(
                AdoptedEmbeddingExecution::RateLimit,
                Vec::new(),
                EmbeddingInputType::Query,
                None,
            ),
            Err(AdapterError::RateLimit { .. })
        ));
    }

    #[test]
    fn standard_online_markdownize_mock_runs() {
        let _env_lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input.pdf");
        let input_bytes = b"%PDF mock";
        std::fs::write(&input, input_bytes).unwrap();
        let input_hash = crate::identity::hash_bytes(input_bytes);
        let outcome = {
            let _mode = kio_core::test_control::TestEnvGuard::set(
                TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV,
                "mock",
            );
            run_standard_online_markdownize(StandardOnlineMarkdownizeRequest {
                scope_id: "01H00000000000000000000000",
                kio_dir: temp.path(),
                raw_hash: &input_hash,
                path: &input,
                media_type: "application/pdf",
                prepared_unit_hints: vec![PreparedUnitHint {
                    unit_key: "page:1".to_owned(),
                    prepared_hash:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .to_owned(),
                    unit_kind: crate::types::UnitKind::Page,
                    order: 0,
                }],
                mode: MarkdownizeMode::Full,
                previous: None,
                hints: None,
                restrict_to_hint_pages: false,
                bbox_annotation_enabled: false,
                idempotency_token: None,
            })
            .unwrap()
        };
        assert_eq!(outcome.profile.adapter_kind, AdapterKind::Markdownize);
        assert_eq!(outcome.response.updated_units.len(), 1);

        // A test-only missing output must not shrink the Prepared manifest. Capture
        // the complete provider-discovered set before the seam removes page:1.
        let partial = {
            let _mode = kio_core::test_control::TestEnvGuard::set(
                TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV,
                "partial",
            );
            run_standard_online_markdownize(StandardOnlineMarkdownizeRequest {
                scope_id: "01H00000000000000000000000",
                kio_dir: temp.path(),
                raw_hash: &input_hash,
                path: &input,
                media_type: "application/pdf",
                prepared_unit_hints: Vec::new(),
                mode: MarkdownizeMode::Full,
                previous: None,
                hints: None,
                restrict_to_hint_pages: false,
                bbox_annotation_enabled: false,
                idempotency_token: None,
            })
            .unwrap()
        };
        assert!(partial.response.updated_units.is_empty());
        assert_eq!(partial.effective_prepared_unit_hints.len(), 1);
        assert_eq!(partial.effective_prepared_unit_hints[0].unit_key, "page:1");
        assert_eq!(
            partial.effective_prepared_unit_hints[0].prepared_hash,
            input_hash
        );
    }

    #[test]
    fn ct4_bbox_003_and_006_catalog_mock_persists_profile_metadata_and_search_text() {
        let _env_lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("chart.pdf");
        let input_bytes = b"%PDF bbox mock";
        std::fs::write(&input, input_bytes).unwrap();
        let input_hash = crate::identity::hash_bytes(input_bytes);
        let outcome = {
            let _mode = kio_core::test_control::TestEnvGuard::set(
                TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV,
                "mock",
            );
            run_standard_online_markdownize(StandardOnlineMarkdownizeRequest {
                scope_id: "01H00000000000000000000000",
                kio_dir: temp.path(),
                raw_hash: &input_hash,
                path: &input,
                media_type: "application/pdf",
                prepared_unit_hints: vec![PreparedUnitHint {
                    unit_key: "page:1".to_owned(),
                    prepared_hash:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .to_owned(),
                    unit_kind: crate::types::UnitKind::Page,
                    order: 0,
                }],
                mode: MarkdownizeMode::Full,
                previous: None,
                hints: None,
                restrict_to_hint_pages: false,
                bbox_annotation_enabled: true,
                idempotency_token: None,
            })
            .unwrap()
        };

        assert_eq!(
            outcome.profile.tool_profile_hash,
            "sha256:9b2490fd9b25c25db6e83baccd86552c038679605f04af4f7f47000391b0d289"
        );
        let unit = &outcome.response.updated_units[0];
        assert!(unit.markdown.contains(r"Kio bbox label page\:1 value 1000"));
        assert_eq!(
            unit.metadata["bbox_annotations"][0]["transcribed_text"],
            "Kio bbox label page\\:1 value 1000"
        );
        let images = temp.path().join("objects/image");
        assert!(
            images.exists(),
            "mock image bytes remain persisted in image CAS"
        );
    }

    #[test]
    fn ct4_bbox_001_catalog_default_is_enabled_and_disabled_preserves_old_identity() {
        assert_eq!(
            standard_online_markdownize_profile().tool_profile_hash,
            standard_online_markdownize_profile_with_bbox(true).tool_profile_hash
        );
        assert_ne!(
            standard_online_markdownize_profile().tool_profile_hash,
            standard_online_markdownize_profile_with_bbox(false).tool_profile_hash
        );
    }
}
