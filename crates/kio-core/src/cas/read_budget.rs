//! Explicit operation accounting for verified physical CAS reads.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use super::ObjectStore;
use crate::error::{KioError, Result};

#[derive(Debug, Default)]
pub struct VerifiedCasReadBudget {
    active: Cell<bool>,
}

#[derive(Debug)]
struct Accounting {
    max_bytes: u64,
    consumed_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct VerifiedCasReadCapability(Arc<Mutex<Option<Accounting>>>);

struct Guard<'a> {
    owner: &'a VerifiedCasReadBudget,
    reads: VerifiedCasReadCapability,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        *self
            .reads
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.owner.active.set(false);
    }
}

impl VerifiedCasReadBudget {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn run<T>(
        &self,
        max_bytes: u64,
        operation: impl FnOnce(&VerifiedCasReadCapability) -> Result<T>,
    ) -> Result<(T, u64)> {
        if self.active.replace(true) {
            return Err(KioError::new(
                "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001",
                "verified CAS read budget cannot be nested",
                serde_json::json!({"max_bytes": max_bytes}),
                crate::ExitCode::PermanentFailure,
            ));
        }
        let guard = Guard {
            owner: self,
            reads: VerifiedCasReadCapability(Arc::new(Mutex::new(Some(Accounting {
                max_bytes,
                consumed_bytes: 0,
            })))),
        };
        let value = operation(&guard.reads)?;
        let consumed = guard
            .reads
            .0
            .lock()
            .map_err(|_| unavailable("read accounting lock poisoned"))?
            .as_ref()
            .ok_or_else(|| unavailable("verified CAS read budget expired"))?
            .consumed_bytes;
        Ok((value, consumed))
    }
}

impl VerifiedCasReadCapability {
    pub fn ensure_active(&self) -> Result<()> {
        self.charge(0)
    }

    pub(crate) fn charge(&self, bytes: u64) -> Result<()> {
        let mut accounting = self
            .0
            .lock()
            .map_err(|_| unavailable("read accounting lock poisoned"))?;
        let budget = accounting
            .as_mut()
            .ok_or_else(|| unavailable("verified CAS read budget expired"))?;
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
    }
}

#[derive(Debug, Clone)]
pub(crate) enum CasReadAccess {
    Unmetered,
    Metered(VerifiedCasReadCapability),
}

impl CasReadAccess {
    pub(crate) fn charge(&self, bytes: u64) -> Result<()> {
        match self {
            Self::Unmetered => Ok(()),
            Self::Metered(reads) => reads.charge(bytes),
        }
    }

    pub(crate) fn bind(&self, reads: &VerifiedCasReadCapability) -> Result<Self> {
        reads.ensure_active()?;
        if let Self::Metered(current) = self {
            current.ensure_active()?;
            if !Arc::ptr_eq(&current.0, &reads.0) {
                return Err(unavailable(
                    "active verified CAS read budget cannot be replaced",
                ));
            }
        }
        Ok(Self::Metered(reads.clone()))
    }

    pub(crate) fn capability(&self) -> Result<Option<VerifiedCasReadCapability>> {
        self.charge(0)?;
        Ok(match self {
            Self::Unmetered => None,
            Self::Metered(reads) => Some(reads.clone()),
        })
    }
}

impl ObjectStore {
    pub fn with_verified_reads(&self, reads: &VerifiedCasReadCapability) -> Result<Self> {
        let mut store = self.clone();
        store.reads = self.reads.bind(reads)?;
        Ok(store)
    }

    pub fn verified_reads(&self) -> Result<Option<VerifiedCasReadCapability>> {
        self.reads.capability()
    }

    pub(crate) fn read_access(&self) -> &CasReadAccess {
        &self.reads
    }
}

fn unavailable(message: &str) -> KioError {
    KioError::new(
        "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001",
        message,
        serde_json::json!({}),
        crate::ExitCode::PermanentFailure,
    )
}

fn verified_cas_read_budget_error(max_bytes: u64, attempted: u64, consumed: u64) -> KioError {
    KioError::new(
        "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001",
        "verified CAS reads exceed the aggregate byte limit",
        serde_json::json!({ "max_bytes": max_bytes, "attempted_bytes": attempted, "consumed_bytes": consumed }),
        crate::ExitCode::PermanentFailure,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planned_content_capture_charges_physical_reads_without_retaining_the_token() {
        use crate::cas::ContentObjectKind;
        use crate::store_dir::RemovalBudget;

        let temp = tempfile::tempdir().unwrap();
        let repo = crate::scope::Repository::init(temp.path()).unwrap();
        let bytes = b"orphan content capture";
        let hash = repo
            .object_store()
            .write_content_object(ContentObjectKind::Prepared, bytes)
            .unwrap();
        let root = repo.store_directory().unwrap().root_handle();
        let store = ObjectStore::from_bound_kio(&root).unwrap();
        let owner = VerifiedCasReadBudget::new();
        let error = owner
            .run(bytes.len() as u64 - 1, |reads| {
                store.with_verified_reads(reads)?.plan_content_removal(
                    ContentObjectKind::Prepared,
                    &hash,
                    &mut RemovalBudget::new(),
                )
            })
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001");
        let (pin, consumed) = owner
            .run(bytes.len() as u64, |reads| {
                store.with_verified_reads(reads)?.plan_content_removal(
                    ContentObjectKind::Prepared,
                    &hash,
                    &mut RemovalBudget::new(),
                )
            })
            .unwrap();
        assert_eq!(consumed, bytes.len() as u64);
        assert!(pin.unwrap().revalidate().unwrap());
    }

    #[test]
    fn verified_cas_budget_overflow_is_checked_without_resetting_consumed_bytes() {
        let owner = VerifiedCasReadBudget::new();
        let ((), consumed) = owner
            .run(u64::MAX, |reads| {
                reads.charge(u64::MAX)?;
                let error = reads.charge(1).unwrap_err();
                assert_eq!(error.error_code(), "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001");
                assert_eq!(error.context()["max_bytes"], u64::MAX);
                assert_eq!(error.context()["attempted_bytes"], u64::MAX);
                assert_eq!(error.context()["consumed_bytes"], u64::MAX);
                Ok(())
            })
            .unwrap();
        assert_eq!(consumed, u64::MAX);
    }
}
