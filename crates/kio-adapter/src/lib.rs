//! Adapter trait, identity helpers, and built-in Step 2 adapters.

pub mod authority;
pub mod batch_client;
pub mod batch_inventory;
pub mod batch_recovery;
pub mod bbox_annotation;
pub mod catalog;
pub mod deterministic;
pub mod deterministic_embedding;
pub mod gemini_batch_client;
mod gemini_embedding;
mod http_policy;
pub mod identity;
mod local_embedding;
pub mod local_ocr_markdownize;
pub mod local_peer;
pub mod local_rerank;
mod mistral_ocr;
pub mod office_convert;
mod ooxml_package;
pub mod pdf_decode;
pub mod tool_lock;
pub mod traits;
pub mod types;
pub mod xlsx_extract;

use thiserror::Error;

/// Return the debug control captured by the composition root for this adapter
/// operation. Production callers must never re-read the ambient environment:
/// the CLI installs one snapshot before dispatch. Crate unit tests are library
/// operation roots, so they capture once when no caller installed a snapshot.
#[cfg(debug_assertions)]
pub(crate) fn debug_test_control() -> kio_core::test_control::DebugTestControl {
    #[cfg(test)]
    {
        kio_core::test_control::capture_for_operation()
    }
    #[cfg(not(test))]
    {
        kio_core::test_control::current_or_default()
    }
}

pub type Result<T> = std::result::Result<T, AdapterError>;

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("adapter contract violation: {0}")]
    ContractViolation(String),
    #[error("adapter auth error: {0}")]
    Auth(String),
    /// The configured local endpoint did not authenticate as the peer bound to
    /// the device CA.  This is permanent: retrying would only resend private
    /// input to a listener whose identity remains unproven.
    #[error("local peer authentication failed: {0}")]
    LocalPeerAuth(String),
    /// A local-peer destination or trust-anchor configuration failure.  The
    /// code is intentionally separate from generic schema errors so callers
    /// never retry a missing, unsafe, or revoked device trust anchor.
    #[error("{code}: {message}")]
    LocalPeerConfig { code: &'static str, message: String },
    /// QA16 (step4b-contract-tests-p3a.md §F, 07 §4 L290): carries the
    /// provider's `Retry-After` in milliseconds when the HTTP response
    /// included one (`http_policy::parse_retry_after_ms`). `None` when the
    /// header was absent or unparseable — never a fabricated value. Use
    /// [`AdapterError::rate_limit`] for the common no-header case (mock/test
    /// seams).
    #[error("adapter rate limited: {message}")]
    RateLimit {
        message: String,
        retry_after_ms: Option<u64>,
    },
    #[error("adapter quota exceeded: {0}")]
    QuotaExceeded(String),
    #[error("adapter network error: {0}")]
    Network(String),
    #[error("schema validation failed: {0}")]
    ConfigSchema(String),
    /// A config-schema violation that owns a user-facing error code.
    ///
    /// `error_code()` below is pinned to `retry_policy`'s table (see its doc
    /// comment), so a bespoke code cannot be returned from there — a config
    /// violation is a `ContractViolation` on the AdapterRun path no matter what
    /// the operator is shown. This variant carries the operator-facing code
    /// alongside, letting `adapter_to_kio` surface it structurally instead of
    /// re-reading it out of the message text.
    ///
    /// Codes are declared in [06-cli-spec.md §8] / [10-operations.md §11.1] and
    /// all resolve to exit 2.
    #[error("{code}: {message}")]
    ConfigSchemaCoded { code: &'static str, message: String },
    #[error("io error at {path}: {message}")]
    Io { path: String, message: String },
}

impl AdapterError {
    /// Construct a `RateLimit` with no known `Retry-After` — the common case
    /// for a mock/test seam or a provider 429 response that omitted the
    /// header. Real HTTP call sites that DO have a parsed header value
    /// (`mistral_ocr`/`gemini_embedding`'s `http_error`) build the variant
    /// directly instead.
    #[must_use]
    pub fn rate_limit(message: impl Into<String>) -> Self {
        Self::RateLimit {
            message: message.into(),
            retry_after_ms: None,
        }
    }

    /// QA16 (step4b-contract-tests-p3a.md §F, 07 §4 L286 / 06 §8): the
    /// machine-judgeable error code this error maps to. Mirrors exactly the
    /// code `kio_pipeline::task::retry_policy` assigns to the corresponding
    /// `RetryErrorKind` (`kio-adapter` cannot depend on `kio-pipeline`, so the
    /// two tables are independently maintained and cross-checked by
    /// `kio-pipeline`'s `qa16_adapter_error_code_matches_retry_policy` test —
    /// see that test before editing either table).
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::Auth(_) => "KIO-E-BATCH-AUTH-001",
            Self::LocalPeerAuth(_) => "KIO-E-LOCAL-PEER-AUTH-001",
            Self::LocalPeerConfig { .. } => "KIO-E-LOCAL-PEER-CONFIG-001",
            Self::RateLimit { .. } => "KIO-E-BATCH-RATE-001",
            Self::QuotaExceeded(_) => "KIO-E-BATCH-QUOTA-001",
            Self::Network(_) | Self::Io { .. } => "KIO-E-BATCH-NET-001",
            Self::ContractViolation(_) | Self::ConfigSchema(_) | Self::ConfigSchemaCoded { .. } => {
                "KIO-E-ADAPTER-CONTRACT-001"
            }
        }
    }

    /// QA16: `transient | permanent | rate_limit` (07 §4 L287) — the coarse
    /// bucket 04 §5.3's table rolls up to for aggregation/reporting. Retry
    /// DECISIONS stay keyed by `error_code`/`RetryErrorKind` (07 §4: "retry
    /// 対応は 04 §5.3 の表が error_code 基準で優先する") — this classification
    /// mirrors `retry_policy(...).retryable` (permanent = non-retryable)
    /// without changing which table drives scheduling.
    #[must_use]
    pub fn error_category(&self) -> crate::types::ErrorCategory {
        use crate::types::ErrorCategory;
        match self {
            Self::RateLimit { .. } => ErrorCategory::RateLimit,
            // Non-retryable in 04 §5.3's table (auth_error has max_attempts=0).
            Self::Auth(_) => ErrorCategory::Permanent,
            Self::LocalPeerAuth(_) | Self::LocalPeerConfig { .. } => ErrorCategory::Permanent,
            // Retryable in 04 §5.3's table (network_error/quota_exceeded/
            // contract_violation all have max_attempts >= 1).
            Self::QuotaExceeded(_)
            | Self::Network(_)
            | Self::Io { .. }
            | Self::ContractViolation(_)
            | Self::ConfigSchema(_)
            | Self::ConfigSchemaCoded { .. } => ErrorCategory::Transient,
        }
    }

    /// QA16: provider `Retry-After`, in milliseconds, when this is a
    /// rate-limit error and the header was present/parseable (07 §4 L290).
    #[must_use]
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            Self::RateLimit { retry_after_ms, .. } => *retry_after_ms,
            _ => None,
        }
    }

    /// QA16: render this error as the terminal `AdapterRun` a real Adapter
    /// trait boundary would report for it (07 §4 L278-307). `error_code`/
    /// `error_category`/`retry_after_ms` are individually queryable instead of
    /// folded into one free-text string; the free text itself lives only in
    /// [`Display`], which is a presentation concern and not part of the record.
    #[must_use]
    pub fn as_adapter_run(&self, task_id: impl Into<String>) -> crate::types::AdapterRun {
        crate::types::AdapterRun {
            task_id: task_id.into(),
            input_hashes: Vec::new(),
            output_hashes: Vec::new(),
            status: crate::types::AdapterRunStatus::Failed,
            error_code: Some(self.error_code().to_owned()),
            error_category: Some(self.error_category()),
            retry_after_ms: self.retry_after_ms(),
            usage: None,
        }
    }
}

pub use mistral_ocr::{BatchOcrMaterializationError, materialize_mistral_batch_ocr_body};
pub use traits::{EmbeddingAdapter, MarkdownizeAdapter, PrepareAdapter};

#[cfg(test)]
mod adapter_error_tests {
    use super::*;
    use crate::types::{AdapterRunStatus, ErrorCategory};

    /// QA16: every variant's `error_code`/`error_category` pairing matches the
    /// 07 §4 L287 contract (`error_category` is the coarse rollup of the fine
    /// `error_code`) and stays internally consistent (RateLimit <-> RateLimit
    /// category exclusively).
    #[test]
    fn error_code_and_category_cover_every_variant() {
        let cases: &[(AdapterError, &str, ErrorCategory)] = &[
            (
                AdapterError::Auth("x".to_owned()),
                "KIO-E-BATCH-AUTH-001",
                ErrorCategory::Permanent,
            ),
            (
                AdapterError::LocalPeerAuth("x".to_owned()),
                "KIO-E-LOCAL-PEER-AUTH-001",
                ErrorCategory::Permanent,
            ),
            (
                AdapterError::LocalPeerConfig {
                    code: "KIO-E-LOCAL-PEER-CA-PEM-001",
                    message: "x".to_owned(),
                },
                "KIO-E-LOCAL-PEER-CONFIG-001",
                ErrorCategory::Permanent,
            ),
            (
                AdapterError::rate_limit("x"),
                "KIO-E-BATCH-RATE-001",
                ErrorCategory::RateLimit,
            ),
            (
                AdapterError::QuotaExceeded("x".to_owned()),
                "KIO-E-BATCH-QUOTA-001",
                ErrorCategory::Transient,
            ),
            (
                AdapterError::Network("x".to_owned()),
                "KIO-E-BATCH-NET-001",
                ErrorCategory::Transient,
            ),
            (
                AdapterError::Io {
                    path: "p".to_owned(),
                    message: "m".to_owned(),
                },
                "KIO-E-BATCH-NET-001",
                ErrorCategory::Transient,
            ),
            (
                AdapterError::ContractViolation("x".to_owned()),
                "KIO-E-ADAPTER-CONTRACT-001",
                ErrorCategory::Transient,
            ),
            (
                AdapterError::ConfigSchema("x".to_owned()),
                "KIO-E-ADAPTER-CONTRACT-001",
                ErrorCategory::Transient,
            ),
        ];
        for (error, expected_code, expected_category) in cases {
            assert_eq!(error.error_code(), *expected_code, "{error:?}");
            assert_eq!(error.error_category(), *expected_category, "{error:?}");
        }
    }

    /// QA16: `retry_after_ms` is `Some` only for `RateLimit`, and only when a
    /// value was actually supplied — never fabricated for other variants or
    /// for a `RateLimit` built via [`AdapterError::rate_limit`] (no header).
    #[test]
    fn retry_after_ms_is_rate_limit_exclusive() {
        assert_eq!(AdapterError::rate_limit("no header").retry_after_ms(), None);
        assert_eq!(
            AdapterError::RateLimit {
                message: "with header".to_owned(),
                retry_after_ms: Some(30_000),
            }
            .retry_after_ms(),
            Some(30_000)
        );
        assert_eq!(
            AdapterError::Auth("x".to_owned()).retry_after_ms(),
            None,
            "retry_after_ms must not leak onto unrelated variants"
        );
    }

    /// QA16 operation scenario (step4b-contract-tests-p3a.md §F): "online
    /// Adapter 呼出が transient エラー (429相当、Retry-After ヘッダ付き) で失敗する"
    /// — the resulting `AdapterRun` carries `error_code`, `error_category` =
    /// `rate_limit`, and `retry_after_ms` as individually-queryable fields
    /// (not folded into one free-text string).
    #[test]
    fn rate_limit_with_retry_after_becomes_a_failed_adapter_run_with_all_three_fields() {
        let error = AdapterError::RateLimit {
            message: "Mistral OCR HTTP 429: Too Many Requests".to_owned(),
            retry_after_ms: Some(30_000),
        };
        let run = error.as_adapter_run("task_01H_example");
        assert_eq!(run.task_id, "task_01H_example");
        assert_eq!(run.status, AdapterRunStatus::Failed);
        assert_eq!(run.error_code.as_deref(), Some("KIO-E-BATCH-RATE-001"));
        assert_eq!(run.error_category, Some(ErrorCategory::RateLimit));
        assert_eq!(run.retry_after_ms, Some(30_000));
        assert_eq!(run.usage, None);
    }
}
