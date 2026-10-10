use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use crate::error::{IoResultExt, KioError, Result};
use crate::store_dir::StoreDirectory;

use super::{
    BoundStoreLock, RetainedStoreLock, validate_bound_store_lock_authority,
    validate_store_lock_authority,
};

thread_local! {
    /// Reentrancy depth per `.lock` path for the current thread. A whole-command
    /// lock held by `kio index`/`repair`/`reindex` must not deadlock against the
    /// `snapshot` sub-step re-acquiring the same lock inside the same process.
    static LOCK_DEPTH: RefCell<HashMap<(PathBuf, bool), u32>> = RefCell::new(HashMap::new());
    /// Distinct lock names in one directory (notably `.lock` followed by
    /// `purge-publication.lock`) share the same directory-flock open file
    /// description within a thread. This keeps the crash-released gate
    /// reentrant without weakening cross-process serialization.
    static LOCK_GATE_POOL: RefCell<HashMap<PathBuf, (u32, File)>> = RefCell::new(HashMap::new());
}

pub(crate) struct LockGate {
    key: PathBuf,
    pub(super) _file: File,
    unlock: fn(&File),
}

impl Drop for LockGate {
    fn drop(&mut self) {
        let final_owner = LOCK_GATE_POOL.with(|pool| {
            let mut pool = pool.borrow_mut();
            let Some((count, _)) = pool.get_mut(&self.key) else {
                return false;
            };
            *count -= 1;
            if *count == 0 {
                pool.remove(&self.key);
                true
            } else {
                false
            }
        });
        if final_owner {
            (self.unlock)(&self._file);
        }
    }
}

/// RAII guard over the `.kio/.lock` store lock (05 §6). Reentrant within a
/// process/thread: nested acquisitions increment a depth counter instead of
/// contending on the same `O_EXCL` file; the on-disk lock is removed only when
/// the outermost guard drops.
pub struct StoreLock {
    path: PathBuf,
    store_gate: bool,
    reentrant: bool,
    // Windows nested guards share an Rc lease, including when the first guard
    // drops first. Unix keeps the original outer-owner release condition.
    pub(super) native: Option<crate::platform::PathLockLease>,
}

thread_local! {
    static PATH_LOCK_OWNERS: RefCell<HashMap<(PathBuf, bool), Weak<crate::platform::PathLock>>> = RefCell::new(HashMap::new());
}

impl StoreLock {
    pub fn acquire(kio_dir: &Path) -> Result<Self> {
        Self::acquire_with_control(kio_dir, &crate::control::ControlHandle::default())
    }

    pub fn acquire_with_control(
        kio_dir: &Path,
        control: &crate::control::ControlHandle,
    ) -> Result<Self> {
        validate_store_lock_authority(&StoreDirectory::open_with_control(
            kio_dir,
            control.clone(),
        )?)?;
        let lock = Self::acquire_path_kind(kio_dir.join(".lock"), true, control)?;
        // The outer acquisition already performed this check while it owned
        // the real lock.  Besides avoiding redundant work, skipping the
        // ambient-path read for a nested acquisition lets a descriptor-bound
        // automatic writer safely reuse this ordinary API.
        if lock.reentrant {
            return Ok(lock);
        }
        // Acquire first, then inspect through the strict GC parser. This closes
        // the marker-publication race for every ordinary writer sharing this
        // central lock entrypoint; GC uses its retained-descriptor counterpart.
        if let Err(error) = crate::gc::ensure_no_active_sweep_with_control(kio_dir, control) {
            drop(lock);
            return Err(error);
        }
        Ok(lock)
    }

    /// Acquire a lock at an explicit file path. Used for device-global locks that
    /// live outside any single `.kio` store (F8) — e.g. serializing a
    /// budget/cost read-check-append across every scope on the device (the
    /// pre-2026-07-18 JSONL cost ledger used this for exactly that; the current
    /// `cost-ledger.sqlite`, `kio_pipeline::ledger`, instead serializes writers via
    /// its own `BEGIN IMMEDIATE` transactions — 04-pipeline.md §5.4/§5.8 — so this
    /// primitive no longer has a live device-global caller, but remains available
    /// for any future one). Same reentrancy (thread-local depth, keyed by the lock
    /// path) and stale-reclaim semantics as [`acquire`]; the parent directory is
    /// created if missing.
    pub fn acquire_path(path: PathBuf) -> Result<Self> {
        Self::acquire_path_with_control(path, &crate::control::ControlHandle::default())
    }

    pub fn acquire_path_with_control(
        path: PathBuf,
        control: &crate::control::ControlHandle,
    ) -> Result<Self> {
        // Generic device/publication locks are explicitly outside store gating.
        Self::acquire_path_kind(path, false, control)
    }

    fn acquire_path_kind(
        path: PathBuf,
        _store_gate: bool,
        control: &crate::control::ControlHandle,
    ) -> Result<Self> {
        let pid = std::process::id();

        // Reentrant fast path: this thread already holds the lock for `path`.
        let already_held = LOCK_DEPTH.with(|depth| {
            let mut depth = depth.borrow_mut();
            if let Some(count) = depth.get_mut(&(path.clone(), _store_gate)) {
                *count += 1;
                true
            } else {
                false
            }
        });
        if already_held {
            let owner = PATH_LOCK_OWNERS.with(|owners| {
                owners
                    .borrow()
                    .get(&(path.clone(), _store_gate))
                    .and_then(Weak::upgrade)
            });
            let native = match crate::platform::scope_semantics::reentrant_path_lock_lease(owner) {
                Ok(native) => native,
                Err(error) => {
                    LOCK_DEPTH.with(|depth| {
                        if let Some(count) =
                            depth.borrow_mut().get_mut(&(path.clone(), _store_gate))
                        {
                            *count -= 1;
                        }
                    });
                    return Err(error);
                }
            };
            return Ok(Self {
                path,
                store_gate: _store_gate,
                reentrant: true,
                native,
            });
        }

        let native = crate::platform::acquire_path_lock(
            path.clone(),
            control,
            pid,
            _store_gate,
            validate_bound_store_lock_authority,
            acquire_lock_gate,
        )?;
        let (native, owner) = crate::platform::scope_semantics::prepare_path_lock_lease(native);
        if let Some(owner) = owner {
            PATH_LOCK_OWNERS.with(|owners| {
                let mut owners = owners.borrow_mut();
                owners.retain(|_, owner| owner.strong_count() != 0);
                owners.insert((path.clone(), _store_gate), Rc::downgrade(&owner));
            });
        }
        LOCK_DEPTH.with(|depth| depth.borrow_mut().insert((path.clone(), _store_gate), 1));
        Ok(Self {
            path,
            store_gate: _store_gate,
            reentrant: false,
            native: Some(native),
        })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _released = LOCK_DEPTH.with(|depth| {
            let mut depth = depth.borrow_mut();
            if let Some(count) = depth.get_mut(&(self.path.clone(), self.store_gate)) {
                *count -= 1;
                if *count == 0 {
                    depth.remove(&(self.path.clone(), self.store_gate));
                    return true;
                }
            }
            false
        });
        crate::platform::finish_path_lock(&mut self.native, _released && !self.reentrant);
        // `gate` drops after the on-disk release operation, and the kernel
        // automatically releases it after a process crash.
    }
}

pub(crate) type AcquireLockGate =
    fn(&Path, fn(&Path, &Path) -> Result<File>, fn(&File)) -> Result<LockGate>;

pub(crate) fn acquire_lock_gate(
    lock_path: &Path,
    acquire: fn(&Path, &Path) -> Result<File>,
    unlock: fn(&File),
) -> Result<LockGate> {
    let parent = lock_path
        .parent()
        .ok_or_else(|| KioError::io("lock path has no parent", lock_path.display().to_string()))?;
    let key = parent.to_path_buf();
    if let Some(cloned) = LOCK_GATE_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        let (count, file) = pool.get_mut(&key)?;
        let cloned = file.try_clone().ok()?;
        *count += 1;
        Some(cloned)
    }) {
        return Ok(LockGate {
            key,
            _file: cloned,
            unlock,
        });
    }
    let gate = acquire(parent, lock_path)?;
    let pooled = gate.try_clone().kio_io(parent)?;
    LOCK_GATE_POOL.with(|pool| {
        pool.borrow_mut().insert(key.clone(), (1, pooled));
    });
    Ok(LockGate {
        key,
        _file: gate,
        unlock,
    })
}

#[derive(Clone, PartialEq, Eq)]
struct BoundLeaseIdentity {
    store_gate: bool,
    identity: crate::platform::DirectoryHandleIdentity,
}

thread_local! {
    static BOUND_STORE_LEASES: RefCell<Vec<(BoundLeaseIdentity, Weak<BoundStoreLock>)>> = const { RefCell::new(Vec::new()) };
}

/// Acquire or share the physical lock only for the same retained directory
/// identity in this thread. A weak entry never keeps an old store alive after
/// the final guard is dropped.
pub(crate) fn acquire_retained_store_lock(kio: &File) -> Result<RetainedStoreLock> {
    acquire_retained_store_lock_with_control(kio, &crate::control::ControlHandle::default())
}

pub(crate) fn acquire_retained_store_lock_with_control(
    kio: &File,
    control: &crate::control::ControlHandle,
) -> Result<RetainedStoreLock> {
    validate_bound_store_lock_authority(kio, control)?;
    acquire_retained_lock_kind_with_control(kio, true, control)
}

pub(crate) fn acquire_retained_lock_kind_with_control(
    kio: &File,
    _store_gate: bool,
    control: &crate::control::ControlHandle,
) -> Result<RetainedStoreLock> {
    let identity = BoundLeaseIdentity {
        store_gate: _store_gate,
        identity: crate::platform::directory_lease_identity(kio)?,
    };
    if let Some(owner) = BOUND_STORE_LEASES.with(|leases| {
        let mut leases = leases.borrow_mut();
        leases.retain(|(_, owner)| owner.strong_count() != 0);
        leases
            .iter()
            .find_map(|(known, owner)| (known == &identity).then(|| owner.upgrade()).flatten())
    }) {
        return Ok(RetainedStoreLock { _owner: owner });
    }
    if _store_gate {
        validate_bound_store_lock_authority(kio, control)?;
    }
    let owner = Rc::new(BoundStoreLock {
        _native: crate::platform::acquire_bound_lock(kio, _store_gate, control)?,
    });
    BOUND_STORE_LEASES.with(|leases| leases.borrow_mut().push((identity, Rc::downgrade(&owner))));
    Ok(RetainedStoreLock { _owner: owner })
}
