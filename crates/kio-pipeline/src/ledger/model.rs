//! Row and enum types for `cost-ledger.sqlite` (04-pipeline.md §5.4 DDL / §5.8).

/// `batch_requests.state` (04 §5.4 DDL comment: `0=投入前/中 1=job 作成済み
/// 2=完了 3=terminal error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BatchState {
    /// Intent recorded; upload/job creation in flight.
    Intent = 0,
    /// Provider job created (batch rows only — sync rows never reach this state).
    JobCreated = 1,
    /// Terminal success.
    Completed = 2,
    /// Terminal error (reject / expired / abandoned / unknown_settled / purged /
    /// submit_rejected / fallback_to_full).
    Terminal = 3,
}

impl BatchState {
    #[must_use]
    pub fn from_i64(value: i64) -> Option<Self> {
        match value {
            0 => Some(Self::Intent),
            1 => Some(Self::JobCreated),
            2 => Some(Self::Completed),
            3 => Some(Self::Terminal),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_i64(self) -> i64 {
        self as i64
    }

    #[must_use]
    pub const fn is_inflight(self) -> bool {
        matches!(self, Self::Intent | Self::JobCreated)
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Terminal)
    }
}

/// `batch_requests.request_kind` (04 §5.4 DDL): `'batch'` follows the full §5.8
/// upload/job/collect protocol; `'sync'` follows the degenerate 2-phase (§5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Batch,
    Sync,
}

impl RequestKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Batch => "batch",
            Self::Sync => "sync",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "batch" => Some(Self::Batch),
            "sync" => Some(Self::Sync),
            _ => None,
        }
    }
}

/// `cost_ledger.outcome` closed enum (04 §5.8 "outcome の対応" table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    ContractViolation,
    Expired,
    Abandoned,
    SubmitRejected,
    Purged,
    UnknownSettled,
    FallbackToFull,
}

impl Outcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::ContractViolation => "contract_violation",
            Self::Expired => "expired",
            Self::Abandoned => "abandoned",
            Self::SubmitRejected => "submit_rejected",
            Self::Purged => "purged",
            Self::UnknownSettled => "unknown_settled",
            Self::FallbackToFull => "fallback_to_full",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "succeeded" => Some(Self::Succeeded),
            "contract_violation" => Some(Self::ContractViolation),
            "expired" => Some(Self::Expired),
            "abandoned" => Some(Self::Abandoned),
            "submit_rejected" => Some(Self::SubmitRejected),
            "purged" => Some(Self::Purged),
            "unknown_settled" => Some(Self::UnknownSettled),
            "fallback_to_full" => Some(Self::FallbackToFull),
            _ => None,
        }
    }
}

/// The 4-tuple task identity shared by `cost_ledger` and `batch_requests` (§5.5
/// task identity key, reused verbatim as the ledger row key — 04 §5.4).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskKey {
    pub scope_id: String,
    pub adapter_kind: String,
    pub input_hash: String,
    pub tool_profile_hash: String,
}

impl TaskKey {
    #[must_use]
    pub fn new(
        scope_id: impl Into<String>,
        adapter_kind: impl Into<String>,
        input_hash: impl Into<String>,
        tool_profile_hash: impl Into<String>,
    ) -> Self {
        Self {
            scope_id: scope_id.into(),
            adapter_kind: adapter_kind.into(),
            input_hash: input_hash.into(),
            tool_profile_hash: tool_profile_hash.into(),
        }
    }

    /// The reserved `scope_id` for query-embedding device rows (04 §5.4: "予約値
    /// — scope_id は ULID のため実 scope と衝突しない").
    pub const DEVICE_SCOPE_ID: &'static str = "device";

    #[must_use]
    pub fn is_device(&self) -> bool {
        self.scope_id == Self::DEVICE_SCOPE_ID
    }
}

/// A full `batch_requests` row (04 §5.4 DDL, 19 columns).
#[derive(Debug, Clone, PartialEq)]
pub struct BatchRequestRow {
    pub key: TaskKey,
    pub state: BatchState,
    pub request_kind: RequestKind,
    pub intent_token: Option<String>,
    pub upload_id: Option<String>,
    pub batch_job_id: Option<String>,
    pub provider_scope_id: Option<String>,
    pub job_create_started_at: Option<i64>,
    pub stale_after_at: Option<i64>,
    pub submission_seq: i64,
    pub attempts: i64,
    pub contract_violation_count: i64,
    pub estimated_usd: f64,
    pub error: Option<String>,
    pub completed_at: Option<i64>,
    pub created_at: i64,
}

impl BatchRequestRow {
    /// Whether the row's residual (upload / job) cleanup is outstanding — the
    /// gate `04 §5.8` uses for "no reissued phase 1 until the old attempt's
    /// residue is confirmed cleaned up" (CL21/CL39): a non-NULL `intent_token`.
    #[must_use]
    pub fn cleanup_pending(&self) -> bool {
        self.intent_token.is_some()
    }
}

/// A full `cost_ledger` row (04 §5.4 DDL, 11 columns).
#[derive(Debug, Clone, PartialEq)]
pub struct CostLedgerRow {
    pub key: TaskKey,
    pub submission_seq: i64,
    pub batch_job_id: String,
    pub usd: f64,
    pub estimated: bool,
    pub outcome: Outcome,
    pub month: String,
    pub recorded_at: i64,
}
