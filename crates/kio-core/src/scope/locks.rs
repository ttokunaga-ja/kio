use std::cell::{Cell, RefCell};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use super::{
    BoundStoreLock, RetainedStoreLock, validate_bound_store_lock_authority,
    validate_store_lock_authority,
};
use crate::error::{IoResultExt, KioError, Result};
use crate::store_dir::StoreDirectory;

#[cfg(test)]
thread_local! {
    static STORE_AUTHORITY_ADMITTED_HOOK: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}

#[cfg(test)]
fn after_store_authority_admitted() {
    let hook = STORE_AUTHORITY_ADMITTED_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

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
    paths: Vec<(NamedLeaseIdentity, Weak<PathOwner>)>,
    gates: Vec<(crate::platform::DirectoryHandleIdentity, Weak<GateOwner>)>,
    retained: Vec<(BoundLeaseIdentity, Weak<BoundStoreLock>)>,
}

#[derive(PartialEq, Eq)]
struct NamedLeaseIdentity {
    parent: crate::platform::DirectoryHandleIdentity,
    leaf: PathBuf,
    store_gate: bool,
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

pub(crate) struct NamedLockTarget {
    pub(crate) parent: File,
    pub(crate) path: PathBuf,
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
        let directory = StoreDirectory::open_with_control(kio_dir, control.clone())?;
        validate_store_lock_authority(&directory)?;
        #[cfg(test)]
        after_store_authority_admitted();
        let lock =
            Self::acquire_named_kind(session, &directory, Path::new(".lock"), true, control)?;
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
        let parent_path = path
            .parent()
            .ok_or_else(|| KioError::io("lock path has no parent", path.display().to_string()))?;
        let parent = if store_gate {
            StoreDirectory::open_with_control(parent_path, control.clone())?
        } else {
            StoreDirectory::open_or_create_directories(parent_path)?
        };
        let leaf = path
            .file_name()
            .ok_or_else(|| KioError::io("lock path has no leaf", path.display().to_string()))?;
        Self::acquire_named_kind(session, &parent, Path::new(leaf), store_gate, control)
    }

    pub(crate) fn acquire_at_in_session(
        session: &LockSession,
        parent: &StoreDirectory,
        leaf: &Path,
        control: &crate::control::ControlHandle,
    ) -> Result<Self> {
        Self::acquire_named_kind(session, parent, leaf, false, control)
    }

    fn acquire_named_kind(
        session: &LockSession,
        parent: &StoreDirectory,
        leaf: &Path,
        store_gate: bool,
        control: &crate::control::ControlHandle,
    ) -> Result<Self> {
        if leaf.components().count() != 1
            || !matches!(
                leaf.components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return Err(KioError::locked("invalid named lock leaf"));
        }
        let key = NamedLeaseIdentity {
            parent: crate::platform::directory_lease_identity(&parent.root_handle())?,
            leaf: leaf.to_path_buf(),
            store_gate,
        };
        let owner = session
            .0
            .borrow()
            .paths
            .iter()
            .find(|(identity, _)| *identity == key)
            .and_then(|(_, owner)| owner.upgrade());
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
            NamedLockTarget {
                parent: parent.root_handle().try_clone().kio_io(parent.path())?,
                path: parent.path().join(leaf),
            },
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
        state.paths.retain(|(_, owner)| owner.strong_count() != 0);
        state.paths.push((key, Rc::downgrade(&owner)));
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
    fn(&LockSession, &File, &Path, fn(&File, &Path) -> Result<File>, fn(&File)) -> Result<LockGate>;

pub(crate) fn acquire_lock_gate(
    session: &LockSession,
    parent: &File,
    lock_path: &Path,
    acquire: fn(&File, &Path) -> Result<File>,
    unlock: fn(&File),
) -> Result<LockGate> {
    let identity = crate::platform::directory_lease_identity(parent)?;
    let existing = session
        .0
        .borrow()
        .gates
        .iter()
        .find(|(candidate, _)| *candidate == identity)
        .and_then(|(_, owner)| owner.upgrade());
    let owner = match existing {
        Some(owner) => owner,
        None => {
            let owner = Rc::new(GateOwner {
                file: acquire(parent, lock_path)?,
                unlock,
            });
            let mut state = session.0.borrow_mut();
            state.gates.retain(|(_, owner)| owner.strong_count() != 0);
            state.gates.push((identity, Rc::downgrade(&owner)));
            owner
        }
    };
    Ok(LockGate {
        _file: owner.file.try_clone().kio_io(lock_path)?,
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

    fn lock_tree_snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn visit(
            root: &Path,
            directory: &Path,
            entries: &mut std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
        ) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                let metadata = std::fs::symlink_metadata(&path).unwrap();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                if metadata.is_dir() {
                    entries.insert(relative, None);
                    visit(root, &path, entries);
                } else {
                    assert!(
                        metadata.is_file(),
                        "unexpected fixture link: {}",
                        path.display()
                    );
                    entries.insert(relative, Some(std::fs::read(path).unwrap()));
                }
            }
        }
        let mut entries = std::collections::BTreeMap::new();
        visit(root, root, &mut entries);
        entries
    }

    #[test]
    fn store_lock_keeps_initially_admitted_repository_after_namespace_replacement() {
        use std::cell::Cell;
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        }
        for name in ["owned", "replacement"] {
            let scope = root.join(name);
            std::fs::create_dir(&scope).unwrap();
            drop(super::super::Repository::init(&scope).unwrap());
        }
        let before = lock_tree_snapshot(&root.join("replacement/.kio"));
        let swapped = Rc::new(Cell::new(false));
        let attempted = Rc::new(Cell::new(false));
        let swapped_hook = swapped.clone();
        let attempted_hook = attempted.clone();
        let root_hook = root.clone();
        STORE_AUTHORITY_ADMITTED_HOOK.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(move || {
                attempted_hook.set(true);
                swapped_hook.set(swap_lock_trees(&root_hook));
            }));
        });
        let result = StoreLock::acquire(&root.join("owned/.kio"));
        assert!(
            attempted.get(),
            "the admission hook must exercise the binding boundary"
        );
        STORE_AUTHORITY_ADMITTED_HOOK.with(|slot| assert!(slot.borrow().is_none()));
        // Success may operate only on retained A; safe refusal is also valid.
        // Include guard Drop, which must not touch B through the old public name.
        drop(result);
        let replacement = root.join(if swapped.get() {
            "owned/.kio"
        } else {
            "replacement/.kio"
        });
        assert_eq!(
            lock_tree_snapshot(&replacement),
            before,
            "validated A authority must not create/reclaim/release B's .lock or alter any B content"
        );
    }

    fn private_lock_tree(root: &StoreDirectory, name: &str) -> StoreDirectory {
        StoreDirectory::from_retained(
            root.create_directory(Path::new(name)).unwrap(),
            root.path().join(name),
        )
        .unwrap()
    }

    fn swap_lock_trees(root: &Path) -> bool {
        let original = root.join("owned");
        let parked = root.join("parked");
        if let Err(error) = std::fs::rename(&original, &parked) {
            #[cfg(not(windows))]
            panic!("cannot park admitted lock tree: {error}");
            #[cfg(windows)]
            assert!(
                matches!(error.raw_os_error(), Some(5 | 32 | 33)),
                "unexpected rename refusal: {error}"
            );
            #[cfg(windows)]
            return false;
        }
        if let Err(error) = std::fs::rename(root.join("replacement"), &original) {
            std::fs::rename(&parked, &original).unwrap();
            #[cfg(not(windows))]
            panic!("cannot install replacement lock tree: {error}");
            #[cfg(windows)]
            assert!(
                matches!(error.raw_os_error(), Some(5 | 32 | 33)),
                "unexpected rename refusal: {error}"
            );
            #[cfg(windows)]
            return false;
        }
        true
    }

    fn assert_lock_released(path: &Path) {
        #[cfg(unix)]
        {
            let bytes = crate::private_fs::read_private_file(path, 4096).unwrap();
            let released: crate::scope::LockFile = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(released.pid, crate::scope::RELEASED_LOCK_PID);
        }
        #[cfg(windows)]
        assert!(!path.exists(), "the held owner must be removed on release");
    }

    #[test]
    fn path_lock_release_stays_with_admitted_parent_after_namespace_replacement() {
        use std::io::Write;

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        }
        let external = StoreDirectory::open(&root).unwrap();
        let original = private_lock_tree(&external, "owned");
        let replacement = private_lock_tree(&external, "replacement");
        let path = original.path().join("scrub.lock");
        let held = StoreLock::acquire_path(path.clone()).unwrap();
        let owned_bytes = crate::private_fs::read_private_file(&path, 4096).unwrap();
        // The same canonical owner in B makes a pathname-based release mutate B.
        // Release authority must come from the retained parent, not matching bytes.
        let mut leaf = replacement
            .create_private_file(Path::new("scrub.lock"))
            .unwrap();
        leaf.write_all(&owned_bytes).unwrap();
        leaf.sync_all().unwrap();
        drop(leaf);
        drop(original);
        drop(replacement);
        drop(external);
        let swapped = swap_lock_trees(&root);
        let replacement_path = root
            .join(if swapped { "owned" } else { "replacement" })
            .join("scrub.lock");
        assert_eq!(std::fs::read(&replacement_path).unwrap(), owned_bytes);
        drop(held);
        assert_eq!(
            std::fs::read(&replacement_path).unwrap(),
            owned_bytes,
            "releasing A must not rewrite or remove B's matching canonical record"
        );
        assert_lock_released(
            &root
                .join(if swapped { "parked" } else { "owned" })
                .join("scrub.lock"),
        );
    }

    #[test]
    fn same_session_path_lock_binds_reentrancy_to_actual_parent() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        }
        let external = StoreDirectory::open(&root).unwrap();
        let original = private_lock_tree(&external, "owned");
        let replacement = private_lock_tree(&external, "replacement");
        let path = original.path().join("scrub.lock");
        let session = LockSession::default();
        let control = crate::control::ControlHandle::default();
        let outer = StoreLock::acquire_path_in_session(&session, path.clone(), &control).unwrap();
        let original_bytes = crate::private_fs::read_private_file(&path, 4096).unwrap();
        let nested = StoreLock::acquire_path_in_session(&session, path.clone(), &control).unwrap();
        assert_eq!(
            crate::private_fs::read_private_file(&path, 4096).unwrap(),
            original_bytes
        );
        assert!(StoreLock::acquire_path(path.clone()).is_err());
        drop(nested);
        drop(original);
        drop(replacement);
        drop(external);
        let swapped = swap_lock_trees(&root);
        if !swapped {
            assert_eq!(
                crate::private_fs::read_private_file(&path, 4096).unwrap(),
                original_bytes
            );
            assert!(!root.join("replacement/scrub.lock").exists());
            drop(outer);
            assert_lock_released(&path);
            return;
        }
        assert!(!path.exists());
        let replacement_guard =
            StoreLock::acquire_path_in_session(&session, path.clone(), &control).unwrap();
        let replacement_bytes = crate::private_fs::read_private_file(&path, 4096)
            .expect("same public name in B requires its own physical lock");
        assert_ne!(replacement_bytes, original_bytes);
        assert!(StoreLock::acquire_path(path.clone()).is_err());
        assert!(StoreLock::acquire_path(root.join("parked/scrub.lock")).is_err());
        drop(replacement_guard);
        assert_lock_released(&path);
        drop(outer);
        assert_lock_released(&root.join("parked/scrub.lock"));
    }

    #[test]
    fn path_lock_creates_private_owned_parents_and_leaf() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let path = root.join("device/kio/logs/scrub.lock");
        let guard = StoreLock::acquire_path(path.clone()).unwrap();
        for relative in ["device", "device/kio", "device/kio/logs"] {
            crate::private_fs::verify_private_directory(&root.join(relative)).unwrap();
        }
        // Windows locks deliberately deny write/delete sharing while held;
        // the read-only trust reader is compatible with that existing contract.
        let bytes = crate::private_fs::read_private_file(&path, 4096).unwrap();
        assert!(!bytes.is_empty());
        drop(guard);
        #[cfg(unix)]
        crate::private_fs::read_private_file(&path, 4096).unwrap();
        #[cfg(windows)]
        assert!(!path.exists());
        let guard = StoreLock::acquire_path(path.clone()).unwrap();
        crate::private_fs::read_private_file(&path, 4096).unwrap();
        drop(guard);
    }

    #[test]
    fn path_lock_rejects_unsafe_stale_leaf_without_adoption() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let path = root.join("unsafe.lock");
        let bytes = crate::scope::canonical_lock_bytes_with_control(
            &crate::control::ControlHandle::default(),
            crate::scope::RELEASED_LOCK_PID,
            "unsafe-stale-owner",
        )
        .unwrap();
        std::fs::write(&path, &bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        #[cfg(windows)]
        {
            let status = std::process::Command::new("icacls.exe")
                .arg(&path)
                .args(["/grant", "*S-1-1-0:F"])
                .status()
                .unwrap();
            assert!(status.success());
        }
        assert!(StoreLock::acquire_path(path.clone()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(crate::private_fs::read_private_file(&path, 4096).is_err());
    }

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
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let external = StoreDirectory::open(&root).unwrap();
        let dir = StoreDirectory::from_retained(
            external.create_directory(Path::new("logs")).unwrap(),
            root.join("logs"),
        )
        .unwrap();
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
