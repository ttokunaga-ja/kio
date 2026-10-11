use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use super::{
    BoundStoreLock, RetainedStoreLock, validate_bound_store_lock_authority,
    validate_store_lock_authority,
};
use crate::error::{IoResultExt, KioError, Result};
use crate::store_dir::StoreDirectory;

/// Local synchronous-operation lock authority. Independent sessions always
/// contend through the OS; clones explicitly share only this operation.
#[derive(Clone, Default)]
pub struct LockSession(Rc<RefCell<LockState>>);

impl std::fmt::Debug for LockSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LockSession")
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct LockState {
    paths: HashMap<(PathBuf, bool), Weak<PathOwner>>,
    gates: HashMap<PathBuf, Weak<GateOwner>>,
    retained: Vec<(BoundLeaseIdentity, Weak<BoundStoreLock>)>,
}

struct PathOwner {
    depth: Cell<u32>,
    native: Option<Weak<crate::platform::PathLock>>,
}

struct GateOwner {
    file: File,
    unlock: fn(&File),
}

impl Drop for GateOwner {
    fn drop(&mut self) {
        (self.unlock)(&self.file);
    }
}

pub(crate) struct LockGate {
    pub(super) _file: File,
    _owner: Rc<GateOwner>,
    _session: LockSession,
}

/// RAII path lock. Reentrancy requires explicitly sharing a LockSession.
pub struct StoreLock {
    reentrant: bool,
    owner: Rc<PathOwner>,
    _session: LockSession,
    pub(super) native: Option<crate::platform::PathLockLease>,
}

impl StoreLock {
    pub fn acquire(kio_dir: &Path) -> Result<Self> {
        Self::acquire_with_control(kio_dir, &crate::control::ControlHandle::default())
    }

    pub fn acquire_with_control(
        kio_dir: &Path,
        control: &crate::control::ControlHandle,
    ) -> Result<Self> {
        Self::acquire_in_session(&LockSession::default(), kio_dir, control)
    }

    pub fn acquire_in_session(
        session: &LockSession,
        kio_dir: &Path,
        control: &crate::control::ControlHandle,
    ) -> Result<Self> {
        validate_store_lock_authority(&StoreDirectory::open_with_control(
            kio_dir,
            control.clone(),
        )?)?;
        let lock = Self::acquire_path_kind(session, kio_dir.join(".lock"), true, control)?;
        if lock.reentrant {
            return Ok(lock);
        }
        if let Err(error) = crate::gc::ensure_no_active_sweep_with_control(kio_dir, control) {
            drop(lock);
            return Err(error);
        }
        Ok(lock)
    }

    /// Standalone device/publication lock root with fresh operation authority.
    pub fn acquire_path(path: PathBuf) -> Result<Self> {
        Self::acquire_path_with_control(path, &crate::control::ControlHandle::default())
    }

    pub fn acquire_path_with_control(
        path: PathBuf,
        control: &crate::control::ControlHandle,
    ) -> Result<Self> {
        Self::acquire_path_in_session(&LockSession::default(), path, control)
    }

    pub fn acquire_path_in_session(
        session: &LockSession,
        path: PathBuf,
        control: &crate::control::ControlHandle,
    ) -> Result<Self> {
        Self::acquire_path_kind(session, path, false, control)
    }

    fn acquire_path_kind(
        session: &LockSession,
        path: PathBuf,
        store_gate: bool,
        control: &crate::control::ControlHandle,
    ) -> Result<Self> {
        let key = (path.clone(), store_gate);
        let owner = session.0.borrow().paths.get(&key).and_then(Weak::upgrade);
        if let Some(owner) = owner {
            let native = crate::platform::scope_semantics::reentrant_path_lock_lease(
                owner.native.as_ref().and_then(Weak::upgrade),
            )?;
            owner.depth.set(owner.depth.get() + 1);
            return Ok(Self {
                reentrant: true,
                owner,
                _session: session.clone(),
                native,
            });
        }
        let native = crate::platform::acquire_path_lock(
            session,
            path,
            control,
            std::process::id(),
            store_gate,
            validate_bound_store_lock_authority,
            acquire_lock_gate,
        )?;
        let (native, native_owner) =
            crate::platform::scope_semantics::prepare_path_lock_lease(native);
        let owner = Rc::new(PathOwner {
            depth: Cell::new(1),
            native: native_owner.as_ref().map(Rc::downgrade),
        });
        let mut state = session.0.borrow_mut();
        state.paths.retain(|_, owner| owner.strong_count() != 0);
        state.paths.insert(key, Rc::downgrade(&owner));
        Ok(Self {
            reentrant: false,
            owner,
            _session: session.clone(),
            native: Some(native),
        })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let depth = self.owner.depth.get() - 1;
        self.owner.depth.set(depth);
        // Unix preserves its existing outer-owner final-release rule; Windows
        // drops the shared native lease only after the final guard.
        crate::platform::finish_path_lock(&mut self.native, depth == 0 && !self.reentrant);
    }
}

pub(crate) type AcquireLockGate =
    fn(&LockSession, &Path, fn(&Path, &Path) -> Result<File>, fn(&File)) -> Result<LockGate>;

pub(crate) fn acquire_lock_gate(
    session: &LockSession,
    lock_path: &Path,
    acquire: fn(&Path, &Path) -> Result<File>,
    unlock: fn(&File),
) -> Result<LockGate> {
    let parent = lock_path
        .parent()
        .ok_or_else(|| KioError::io("lock path has no parent", lock_path.display().to_string()))?;
    let existing = session.0.borrow().gates.get(parent).and_then(Weak::upgrade);
    let owner = match existing {
        Some(owner) => owner,
        None => {
            let owner = Rc::new(GateOwner {
                file: acquire(parent, lock_path)?,
                unlock,
            });
            let mut state = session.0.borrow_mut();
            state.gates.retain(|_, owner| owner.strong_count() != 0);
            state
                .gates
                .insert(parent.to_path_buf(), Rc::downgrade(&owner));
            owner
        }
    };
    Ok(LockGate {
        _file: owner.file.try_clone().kio_io(parent)?,
        _owner: owner,
        _session: session.clone(),
    })
}

#[derive(Clone, PartialEq, Eq)]
struct BoundLeaseIdentity {
    store_gate: bool,
    identity: crate::platform::DirectoryHandleIdentity,
}

pub(crate) fn acquire_retained_store_lock(kio: &File) -> Result<RetainedStoreLock> {
    acquire_retained_store_lock_with_control(
        &LockSession::default(),
        kio,
        &crate::control::ControlHandle::default(),
    )
}

pub(crate) fn acquire_retained_store_lock_with_control(
    session: &LockSession,
    kio: &File,
    control: &crate::control::ControlHandle,
) -> Result<RetainedStoreLock> {
    validate_bound_store_lock_authority(kio, control)?;
    acquire_retained_lock_kind_with_control(session, kio, true, control)
}

pub(crate) fn acquire_retained_lock_kind_with_control(
    session: &LockSession,
    kio: &File,
    store_gate: bool,
    control: &crate::control::ControlHandle,
) -> Result<RetainedStoreLock> {
    let identity = BoundLeaseIdentity {
        store_gate,
        identity: crate::platform::directory_lease_identity(kio)?,
    };
    let owner = {
        let mut state = session.0.borrow_mut();
        state
            .retained
            .retain(|(_, owner)| owner.strong_count() != 0);
        state
            .retained
            .iter()
            .find_map(|(known, owner)| (known == &identity).then(|| owner.upgrade()).flatten())
    };
    if let Some(owner) = owner {
        return Ok(RetainedStoreLock {
            _owner: owner,
            _session: session.clone(),
        });
    }
    if store_gate {
        validate_bound_store_lock_authority(kio, control)?;
    }
    let owner = Rc::new(BoundStoreLock {
        _native: crate::platform::acquire_bound_lock(kio, store_gate, control)?,
    });
    session
        .0
        .borrow_mut()
        .retained
        .push((identity, Rc::downgrade(&owner)));
    Ok(RetainedStoreLock {
        _owner: owner,
        _session: session.clone(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_lock_roots_contend_on_the_same_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operation.lock");
        let outer = StoreLock::acquire_path(path.clone()).unwrap();
        let error = StoreLock::acquire_path(path.clone())
            .err()
            .expect("independent roots must contend");
        assert_eq!(error.error_code(), "KIO-E-STORE-LOCKED-001");
        drop(outer);
        assert!(StoreLock::acquire_path(path).is_ok());
    }

    #[test]
    fn operation_log_append_reuses_the_scrub_lock() {
        let dir = tempfile::tempdir().unwrap();
        let session = LockSession::default();
        let control = crate::control::ControlHandle::default();
        let _scrub =
            StoreLock::acquire_path_in_session(&session, dir.path().join("scrub.lock"), &control)
                .unwrap();
        let log = dir.path().join("events.jsonl");
        let value = serde_json::json!({"phase":"completed"});
        assert!(control.append_jsonl_rotating(&log, &value, 30).is_err());
        control
            .append_jsonl_rotating_in_session(&session, &log, &value, 30)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            "{\"phase\":\"completed\"}\n"
        );
    }

    #[test]
    fn guards_retain_their_operation_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retained.lock");
        let held = {
            let session = LockSession::default();
            StoreLock::acquire_path_in_session(
                &session,
                path.clone(),
                &crate::control::ControlHandle::default(),
            )
            .unwrap()
        };
        assert!(StoreLock::acquire_path(path.clone()).is_err());
        drop(held);
        assert!(StoreLock::acquire_path(path).is_ok());
    }

    #[test]
    fn same_session_shares_directory_gate_for_distinct_leaves() {
        let dir = tempfile::tempdir().unwrap();
        let session = LockSession::default();
        let control = crate::control::ControlHandle::default();
        let first =
            StoreLock::acquire_path_in_session(&session, dir.path().join("first.lock"), &control)
                .unwrap();
        let second =
            StoreLock::acquire_path_in_session(&session, dir.path().join("second.lock"), &control)
                .unwrap();
        drop(first);
        assert!(StoreLock::acquire_path(dir.path().join("second.lock")).is_err());
        drop(second);
        assert!(StoreLock::acquire_path(dir.path().join("second.lock")).is_ok());
    }

    #[test]
    fn retained_nested_authority_is_explicit_and_public_roots_contend() {
        let dir = tempfile::tempdir().unwrap();
        let repo = super::super::Repository::init(dir.path()).unwrap();
        let session = LockSession::default();
        let outer = repo.lock_store_in_session(&session).unwrap();
        let other = super::super::Repository::open(dir.path())
            .unwrap()
            .in_lock_session(&session);
        let nested = other.lock_store_in_session(&session).unwrap();
        assert_eq!(
            repo.lock_store().err().unwrap().error_code(),
            "KIO-E-STORE-LOCKED-001"
        );
        assert_eq!(
            other.lock_store().err().unwrap().error_code(),
            "KIO-E-STORE-LOCKED-001"
        );
        drop(outer);
        assert!(repo.lock_store().is_err());
        drop(nested);
        assert!(repo.lock_store().is_ok());
    }

    #[test]
    fn explicit_session_nested_lock_releases_after_unwind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operation.lock");
        let result = std::panic::catch_unwind(|| {
            let session = LockSession::default();
            let control = crate::control::ControlHandle::default();
            let _outer =
                StoreLock::acquire_path_in_session(&session, path.clone(), &control).unwrap();
            let _nested =
                StoreLock::acquire_path_in_session(&session, path.clone(), &control).unwrap();
            panic!("operation failed");
        });
        assert!(result.is_err());
        assert!(StoreLock::acquire_path(path).is_ok());
    }
}
