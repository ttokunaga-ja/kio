//! Typed durable operations for the device cost ledger.
//!
//! SQL is deliberately confined to `ops_sql`.  Callers hold a [`LedgerDb`],
//! never a writable SQLite connection; lifecycle owns locking, the outer
//! immediate transaction, commit/rollback, and write-sequence checkpointing.

use super::{LedgerDb, lifecycle::LedgerWriteTxn};
use crate::Result;
use crate::ledger::model::{BatchRequestRow, RequestKind, TaskKey};

pub use super::ops_sql::{
    AbandonExecution, AbandonResolution, AbandonSelector, BILLABLE_UNIT_KINDS, BilledAmount,
    BudgetCapConfig, CapCheckResult, CapLayer, ClaimOutcome, DEFAULT_RECOVERY_DEADLINE_MS,
    DEFAULT_VISIBILITY_GRACE_PERIOD_MS, ExtendOutcome, PER_ADAPTER_KIND_ENUM, Phase1Outcome,
    ReportedBillableUnit, SweepPlan, SweepReport, TerminalReceipt, TerminalWrite,
};
pub use super::ops_sql::{
    allocate_sweep_capacity, compute_stale_after_at, contract_violation_retry_allowed,
    device_input_hash, is_uuid_v7, is_valid_per_adapter_key, new_intent_token, nonbillable_charge,
    phase2b_scope_matches, recovery_deadline_passed, resolve_billing_from_billable_units,
    resolve_billing_from_reported_usage, resolve_billing_from_usd_field, uuid_v7_timestamp_millis,
    visibility_grace_period_elapsed,
};

/// Look up a request row through the ledger's read lifecycle.
pub fn get_batch_request(db: &LedgerDb, key: &TaskKey) -> Result<Option<BatchRequestRow>> {
    db.read(|conn| super::ops_sql::get_batch_request(conn, key))
}

pub fn requests_with_intent_for_scope(
    db: &LedgerDb,
    scope_id: &str,
) -> Result<Vec<BatchRequestRow>> {
    db.read(|conn| super::ops_sql::requests_with_intent_for_scope(conn, scope_id))
}

/// Read every state 0/1 request for a scope through the existing ledger
/// lifecycle. Request kind and presence of an intent token do not limit this
/// repair blocker query; no database is initialized or recovered here.
pub fn inflight_requests_for_scope(db: &LedgerDb, scope_id: &str) -> Result<Vec<BatchRequestRow>> {
    db.read(|conn| super::ops_sql::inflight_requests_for_scope(conn, scope_id))
}

pub fn cost_ledger_rows_for_key(
    db: &LedgerDb,
    key: &TaskKey,
) -> Result<Vec<super::model::CostLedgerRow>> {
    db.read(|conn| super::ops_sql::cost_ledger_rows_for_key(conn, key))
}

macro_rules! typed_write {
    ($name:ident($($arg:ident : $ty:ty),* $(,)?) -> $ret:ty) => {
        pub fn $name(db: &LedgerDb, $($arg: $ty),*) -> Result<$ret> {
            db.write(|tx: &mut LedgerWriteTxn| super::ops_sql::$name(tx.connection(), $($arg),*))
        }
    };
}
macro_rules! typed_read {
    ($name:ident($($arg:ident : $ty:ty),* $(,)?) -> $ret:ty) => {
        pub fn $name(db: &LedgerDb, $($arg: $ty),*) -> Result<$ret> {
            db.read(|conn| super::ops_sql::$name(conn, $($arg),*))
        }
    };
}

typed_write!(phase1_intent(key: &TaskKey, request_kind: RequestKind, estimated_usd: f64,
    sync_effective_timeout_seconds: Option<i64>) -> Phase1Outcome);
typed_write!(phase2a_record_provider_scope(key: &TaskKey, intent_token: &str,
    provider_scope_id: &str) -> bool);
typed_write!(phase2a_record_upload_id(key: &TaskKey, intent_token: &str, upload_id: &str) -> bool);
typed_write!(phase2a_restart_after_scope_mismatch(key: &TaskKey, intent_token: &str,
    old_upload_deletion_confirmed: bool) -> bool);
typed_write!(phase2b_record_job_create_started(key: &TaskKey, intent_token: &str) -> bool);
typed_write!(phase2b_record_job_created(key: &TaskKey, intent_token: &str, batch_job_id: &str) -> bool);
typed_write!(sync_record_provider_request_id(key: &TaskKey, intent_token: &str,
    provider_request_id: &str) -> bool);
typed_write!(terminal_transaction(write: &TerminalWrite<'_>) -> TerminalReceipt);
typed_read!(recovery_candidates() -> Vec<BatchRequestRow>);
// Unreset Batch contract failures for the exact current attempt, including cleaned rows.
typed_read!(unreset_contract_violations_for_scope_adapter(scope_id: &str, adapter_kind: &str) -> Vec<BatchRequestRow>);
typed_read!(batch_poll_candidates(scope_id: &str, adapter_kind: &str) -> Vec<BatchRequestRow>);
typed_read!(sync_recovery_candidates(scope_id: &str, now_ms: i64) -> Vec<BatchRequestRow>);
typed_read!(distinct_scope_ids_with_sync_rows() -> Vec<String>);
typed_write!(recovery_mark_found(key: &TaskKey, batch_job_id: &str) -> bool);
typed_write!(recovery_settle_unknown(key: &TaskKey, intent_token: &str, estimated_usd: f64,
    cleanup_complete: bool) -> TerminalReceipt);
typed_write!(authorize_unknown_sync_resend(key: &TaskKey) -> bool);
typed_write!(recovery_finish_cleanup(key: &TaskKey, intent_token: &str) -> bool);
typed_read!(stalled_rows() -> Vec<BatchRequestRow>);
typed_write!(device_claim(key: &TaskKey, candidate_usd: f64, effective_timeout_seconds: i64,
    device_cap: f64, per_adapter_cap: Option<f64>) -> ClaimOutcome);
typed_write!(device_extend_stale_after(key: &TaskKey, intent_token: &str, retry_after_seconds: f64,
    effective_timeout_seconds: i64) -> ExtendOutcome);
typed_read!(plan_bounded_sweep(own_key: Option<&TaskKey>, now_ms: i64) -> SweepPlan);
typed_write!(execute_bounded_sweep(plan: &SweepPlan, now_ms: i64) -> SweepReport);
pub fn ledger_month_total(
    db: &LedgerDb,
    scope_id: Option<&str>,
    adapter_kind: Option<&str>,
    month: &str,
) -> Result<f64> {
    db.read(|conn| super::ops_sql::ledger_month_total(conn, scope_id, adapter_kind, month))
}
typed_write!(check_then_reserve(key: &TaskKey, candidate_usd: f64, caps: &BudgetCapConfig,
    request_kind: RequestKind, sync_effective_timeout_seconds: Option<i64>) -> CapCheckResult);
typed_read!(resolve_abandon_selector(selector: &AbandonSelector) -> AbandonResolution);
typed_write!(execute_abandon(key: &TaskKey) -> AbandonExecution);
typed_write!(reset_contract_violations(key: &TaskKey) -> bool);

/// Atomic result for the application-level ``reuse open reservation or make a
/// fresh cap-checked reservation`` decision.
#[derive(Debug, Clone, PartialEq)]
pub enum ReserveOrReuseOutcome {
    Reused {
        intent_token: String,
        estimated_usd: f64,
    },
    Reserved {
        intent_token: String,
        estimated_usd: f64,
    },
    BudgetExceeded,
}

/// Preserve the check/read/reserve relationship under one ledger write
/// transaction.  `bypass_cap_denial` retains the documented override and
/// soft-stop behaviour while still recording a phase-1 intent.
pub fn reserve_or_reuse(
    db: &LedgerDb,
    key: &TaskKey,
    candidate_usd: f64,
    caps: &BudgetCapConfig,
    request_kind: RequestKind,
    sync_effective_timeout_seconds: Option<i64>,
    bypass_cap_denial: bool,
) -> Result<ReserveOrReuseOutcome> {
    db.write(|tx: &mut LedgerWriteTxn| {
        let conn = tx.connection();
        super::ops_sql::guard_unauthorized_unknown_sync_resend(conn, key)?;
        if let Some(existing) = super::ops_sql::get_batch_request(conn, key)?
            && existing.state.is_inflight()
        {
            if !key.is_device() && existing.request_kind == RequestKind::Sync {
                return Err(crate::PipelineError::contract(
                    "KIO-E-SYNC-RESULT-UNKNOWN-001",
                    "an in-flight sync attempt cannot be resent; recover or settle its result first",
                ));
            }
            let intent_token = existing.intent_token.ok_or_else(|| {
                crate::PipelineError::contract(
                    "KIO-E-LEDGER-INFLIGHT-INTENT-001",
                    "in-flight ledger row is missing its required intent token",
                )
            })?;
            return Ok(ReserveOrReuseOutcome::Reused {
                intent_token,
                estimated_usd: existing.estimated_usd,
            });
        }
        match super::ops_sql::check_then_reserve(
            conn,
            key,
            candidate_usd,
            caps,
            request_kind,
            sync_effective_timeout_seconds,
        )? {
            CapCheckResult::Allowed(outcome) | CapCheckResult::ExemptZeroCost(outcome) => {
                Ok(ReserveOrReuseOutcome::Reserved {
                    intent_token: outcome.intent_token,
                    estimated_usd: candidate_usd,
                })
            }
            CapCheckResult::Denied(_) if bypass_cap_denial => {
                let outcome = super::ops_sql::phase1_intent(
                    conn,
                    key,
                    request_kind,
                    candidate_usd,
                    sync_effective_timeout_seconds,
                )?;
                Ok(ReserveOrReuseOutcome::Reserved {
                    intent_token: outcome.intent_token,
                    estimated_usd: candidate_usd,
                })
            }
            CapCheckResult::Denied(_) => Ok(ReserveOrReuseOutcome::BudgetExceeded),
        }
    })
}

/// Sweep stale device rows and issue a new claim in one immediate transaction.
pub fn sweep_then_device_claim(
    db: &LedgerDb,
    key: &TaskKey,
    now_ms: i64,
    candidate_usd: f64,
    effective_timeout_seconds: i64,
    device_cap: f64,
    per_adapter_cap: Option<f64>,
) -> Result<ClaimOutcome> {
    db.write(|tx: &mut LedgerWriteTxn| {
        let conn = tx.connection();
        let plan = super::ops_sql::plan_bounded_sweep(conn, Some(key), now_ms)?;
        super::ops_sql::execute_bounded_sweep(conn, &plan, now_ms)?;
        super::ops_sql::device_claim(
            conn,
            key,
            candidate_usd,
            effective_timeout_seconds,
            device_cap,
            per_adapter_cap,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inflight_scope_query_uses_the_existing_ledger_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let db = LedgerDb::initialize(dir.path().join("device/cost-ledger.sqlite")).unwrap();
        let batch = TaskKey::new("scope-a", "embedding", "batch", "profile");
        let realtime = TaskKey::new("scope-a", "markdownize", "realtime", "profile");
        let other = TaskKey::new("scope-b", "embedding", "other", "profile");
        phase1_intent(&db, &batch, RequestKind::Batch, 0.5, None).unwrap();
        phase1_intent(&db, &realtime, RequestKind::Sync, 0.5, Some(300)).unwrap();
        phase1_intent(&db, &other, RequestKind::Batch, 0.5, None).unwrap();
        let rows = inflight_requests_for_scope(&db, "scope-a").unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.key == batch));
        assert!(rows.iter().any(|row| row.key == realtime));
        assert!(
            inflight_requests_for_scope(&db, "missing-scope")
                .unwrap()
                .is_empty()
        );
        // LedgerDb retains its authority/locator, not a live connection. A
        // disappearance must fail closed rather than bootstrap a new ledger.
        std::fs::remove_file(db.path()).unwrap();
        assert!(inflight_requests_for_scope(&db, "scope-a").is_err());
        assert!(!db.path().exists());
    }

    #[test]
    fn reserve_or_reuse_refuses_an_inflight_task_sync_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let db = LedgerDb::initialize(dir.path().join("device/cost-ledger.sqlite")).unwrap();
        let key = TaskKey::new("scope-a", "markdownize", "hash-a", "profile-a");
        let first = phase1_intent(&db, &key, RequestKind::Sync, 1.25, Some(300)).unwrap();
        let caps = BudgetCapConfig {
            device_cap: 1_000_000.0,
            folder_cap: None,
            device_per_adapter_cap: None,
        };

        let err = reserve_or_reuse(&db, &key, 1.25, &caps, RequestKind::Sync, Some(300), false)
            .expect_err("a task sync retry must recover the existing attempt, never resend it");
        assert!(
            err.to_string().contains("KIO-E-SYNC-RESULT-UNKNOWN-001"),
            "got {err}"
        );
        let row = get_batch_request(&db, &key).unwrap().unwrap();
        assert_eq!(
            row.intent_token.as_deref(),
            Some(first.intent_token.as_str())
        );
        assert!(cost_ledger_rows_for_key(&db, &key).unwrap().is_empty());
    }
}
