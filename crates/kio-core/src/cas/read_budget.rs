//! Aggregate accounting for synchronous verified CAS reads.

use std::cell::RefCell;

use crate::error::{KioError, Result};

#[derive(Debug, Clone, Copy)]
struct VerifiedCasReadBudget {
    max_bytes: u64,
    consumed_bytes: u64,
}

thread_local! {
    /// Evidence verification is synchronous, so a thread-scoped budget binds all
    /// of its canonical CAS reads without changing every store API. It is only
    /// active inside [`with_verified_cas_read_budget`].
    static VERIFIED_CAS_READ_BUDGET: RefCell<Option<VerifiedCasReadBudget>> = const { RefCell::new(None) };
}

struct VerifiedCasReadBudgetGuard;

impl Drop for VerifiedCasReadBudgetGuard {
    fn drop(&mut self) {
        VERIFIED_CAS_READ_BUDGET.with(|budget| *budget.borrow_mut() = None);
    }
}

/// Bound aggregate bytes physically read from CAS during one synchronous
/// verification operation. Repeated reads are charged repeatedly.
///
/// Nesting is rejected so an inner caller cannot obscure the outer authority's
/// accounting. The scope is removed even when `operation` returns an error or
/// unwinds.
pub fn with_verified_cas_read_budget<T>(
    max_bytes: u64,
    operation: impl FnOnce() -> Result<T>,
) -> Result<(T, u64)> {
    VERIFIED_CAS_READ_BUDGET.with(|budget| {
        let mut budget = budget.borrow_mut();
        if budget.is_some() {
            return Err(KioError::new(
                "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001",
                "verified CAS read budget cannot be nested",
                serde_json::json!({ "max_bytes": max_bytes }),
                crate::ExitCode::PermanentFailure,
            ));
        }
        *budget = Some(VerifiedCasReadBudget {
            max_bytes,
            consumed_bytes: 0,
        });
        Ok(())
    })?;
    let _guard = VerifiedCasReadBudgetGuard;
    let value = operation()?;
    let consumed_bytes = VERIFIED_CAS_READ_BUDGET.with(|budget| {
        budget
            .borrow()
            .as_ref()
            .map(|budget| budget.consumed_bytes)
            .ok_or_else(|| {
                KioError::new(
                    "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001",
                    "verified CAS read budget unexpectedly unavailable",
                    serde_json::json!({ "max_bytes": max_bytes }),
                    crate::ExitCode::PermanentFailure,
                )
            })
    })?;
    Ok((value, consumed_bytes))
}

pub(super) fn charge_verified_cas_read(bytes: u64) -> Result<()> {
    VERIFIED_CAS_READ_BUDGET.with(|budget| {
        let mut budget = budget.borrow_mut();
        let Some(budget) = budget.as_mut() else {
            return Ok(());
        };
        let attempted = budget.consumed_bytes.checked_add(bytes).ok_or_else(|| {
            verified_cas_read_budget_error(budget.max_bytes, u64::MAX, budget.consumed_bytes)
        })?;
        if attempted > budget.max_bytes {
            return Err(verified_cas_read_budget_error(
                budget.max_bytes,
                attempted,
                budget.consumed_bytes,
            ));
        }
        budget.consumed_bytes = attempted;
        Ok(())
    })
}

fn verified_cas_read_budget_error(max_bytes: u64, attempted: u64, consumed: u64) -> KioError {
    KioError::new(
        "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001",
        "verified CAS reads exceed the aggregate byte limit",
        serde_json::json!({
            "max_bytes": max_bytes,
            "attempted_bytes": attempted,
            "consumed_bytes": consumed,
        }),
        crate::ExitCode::PermanentFailure,
    )
}
