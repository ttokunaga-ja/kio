//! Immutable store-wide Windows byte-range gate. Acquiring it never writes.
use super::*;
use cap_primitives::fs as cap_fs;
use std::os::windows::io::AsRawHandle;
use windows_sys::Win32::Storage::FileSystem::{
    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, UnlockFileEx,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

pub(super) struct Gate {
    file: File,
    identity: crate::cas::WindowsRegularFileIdentity,
}

fn open(kio: &File) -> Result<File> {
    use windows_sys::Win32::Foundation::GENERIC_READ;
    let options = windows_lock_open_options(false, GENERIC_READ);
    let file = cap_fs::open(kio, Path::new(".store-gate"), &options)
        .map_err(|_| KioError::locked(".kio/.store-gate is missing, unsafe, or unavailable"))?;
    checked_identity(&file)?;
    Ok(file)
}

pub(super) fn validate(kio: &File) -> Result<()> {
    open(kio).map(drop)
}

fn checked_identity(file: &File) -> Result<crate::cas::WindowsRegularFileIdentity> {
    let identity = crate::cas::windows_regular_file_handle_identity(file)
        .ok_or_else(|| KioError::locked(".kio/.store-gate is not a single-link regular file"))?;
    if file
        .metadata()
        .map_err(|e| KioError::io(e.to_string(), ".store-gate"))?
        .len()
        != 0
    {
        return Err(KioError::locked(".kio/.store-gate is not empty"));
    }
    Ok(identity)
}

impl Gate {
    pub(super) fn acquire(kio: &File, exclusive: bool) -> Result<Self> {
        let file = open(kio)?;
        let identity = checked_identity(&file)?;
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        let flags = LOCKFILE_FAIL_IMMEDIATELY
            | if exclusive {
                LOCKFILE_EXCLUSIVE_LOCK
            } else {
                0
            };
        // Microsoft permits a byte range beyond EOF and either GENERIC_READ or
        // GENERIC_WRITE. Byte zero, length one therefore leaves the empty file
        // immutable. FAIL_IMMEDIATELY makes this a synchronous nonblocking call.
        if unsafe { LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &mut overlapped) } == 0 {
            return Err(KioError::locked(".kio/.store-gate"));
        }
        let gate = Self { file, identity };
        gate.recheck(kio)?;
        Ok(gate)
    }

    pub(super) fn recheck(&self, kio: &File) -> Result<()> {
        if checked_identity(&self.file)? != self.identity
            || checked_identity(&open(kio)?)? != self.identity
        {
            return Err(KioError::locked(".kio/.store-gate identity changed"));
        }
        Ok(())
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        // SAFETY: this guard owns the synchronous HANDLE and exactly this range.
        let _ = unsafe { UnlockFileEx(self.file.as_raw_handle(), 0, 1, 0, &mut overlapped) };
    }
}

#[derive(PartialEq, Eq)]
pub(super) struct IdleLock {
    identity: crate::cas::WindowsRegularFileIdentity,
    record: LockFile,
}

pub(super) fn idle_lock(kio: &File) -> Result<Option<IdleLock>> {
    use windows_sys::Win32::Foundation::GENERIC_READ;
    let options = windows_lock_open_options(false, GENERIC_READ);
    let file = match cap_fs::open(kio, Path::new(".lock"), &options) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(KioError::locked(".kio/.lock")),
    };
    let (record, identity) =
        read_windows_lock(&file).map_err(|_| KioError::locked(".kio/.lock"))?;
    if record.pid != RELEASED_LOCK_PID {
        return Err(KioError::locked(".kio/.lock"));
    }
    Ok(Some(IdleLock { identity, record }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retained(repo: &Repository) -> File {
        repo.store_directory()
            .unwrap()
            .root_handle()
            .try_clone()
            .unwrap()
    }
    fn locked<T>(result: Result<T>) {
        match result {
            Err(error) => {
                assert_eq!(error.error_code(), "KIO-E-STORE-LOCKED-001");
                assert_eq!(error.exit_code(), ExitCode::PartialFailure);
            }
            Ok(_) => panic!("expected a closed store gate"),
        }
    }

    #[test]
    fn readers_share_and_exclude_both_writer_entrypoints_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let kio = retained(&repo);
        let before = fs::metadata(repo.kio_dir().join(".store-gate"))
            .unwrap()
            .modified()
            .unwrap();
        let first = acquire_bound_store_read_guard(&kio).unwrap();
        let second = acquire_bound_store_read_guard(&kio).unwrap();
        first.recheck_idle().unwrap();
        second.recheck_idle().unwrap();
        locked(acquire_bound_store_lock(&kio));
        locked(StoreLock::acquire(repo.kio_dir()));
        assert!(!repo.kio_dir().join(".lock").exists());
        assert_eq!(fs::read(repo.kio_dir().join(".store-gate")).unwrap(), b"");
        assert_eq!(
            fs::metadata(repo.kio_dir().join(".store-gate"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
        drop(first);
        locked(acquire_bound_store_lock(&kio));
        drop(second);
        drop(acquire_bound_store_lock(&kio).unwrap());
        assert!(!repo.kio_dir().join(".lock").exists());
        acquire_bound_store_read_guard(&kio)
            .unwrap()
            .recheck_idle()
            .unwrap();
    }

    #[test]
    fn both_writer_entrypoints_exclude_readers_until_owner_handle_closes() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let kio = retained(&repo);
        let ordinary = StoreLock::acquire(repo.kio_dir()).unwrap();
        locked(acquire_bound_store_read_guard(&kio));
        let nested = StoreLock::acquire(repo.kio_dir()).unwrap();
        drop(nested);
        locked(acquire_bound_store_read_guard(&kio));
        drop(ordinary);
        acquire_bound_store_read_guard(&kio)
            .unwrap()
            .recheck_idle()
            .unwrap();
        let bound = repo.lock_store().unwrap();
        let nested = repo.lock_store().unwrap();
        let publication = repo.lock_purge_publication().unwrap();
        assert!(
            !repo
                .kio_dir()
                .join("internal/publication/.store-gate")
                .exists()
        );
        locked(acquire_bound_store_read_guard(&kio));
        drop(publication);
        drop(bound);
        locked(acquire_bound_store_read_guard(&kio));
        drop(nested);
        assert!(!repo.kio_dir().join(".lock").exists());
        acquire_bound_store_read_guard(&kio)
            .unwrap()
            .recheck_idle()
            .unwrap();
    }

    #[test]
    fn ordinary_nested_owner_survives_outer_guard_drop() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let kio = retained(&repo);
        let outer = StoreLock::acquire(repo.kio_dir()).unwrap();
        let nested = StoreLock::acquire(repo.kio_dir()).unwrap();
        drop(outer);
        assert!(repo.kio_dir().join(".lock").exists());
        locked(acquire_bound_store_read_guard(&kio));
        locked(acquire_bound_store_lock(&kio));
        drop(nested);
        assert!(!repo.kio_dir().join(".lock").exists());
        acquire_bound_store_read_guard(&kio)
            .unwrap()
            .recheck_idle()
            .unwrap();
    }

    #[test]
    fn generic_lock_lease_cannot_satisfy_store_gate_acquisition() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let kio = retained(&repo);
        let generic = StoreLock::acquire_path(repo.kio_dir().join(".lock")).unwrap();
        locked(StoreLock::acquire(repo.kio_dir()));
        drop(generic);
        let nongated = acquire_retained_lock_kind(&kio, false).unwrap();
        locked(repo.lock_store());
        drop(nongated);
        repo.lock_store().unwrap();
    }

    #[test]
    fn missing_nonempty_hardlinked_and_directory_gates_reject_before_lock_creation() {
        for variant in ["missing", "nonempty", "hardlink", "directory"] {
            let temp = tempfile::tempdir().unwrap();
            let repo = Repository::init(temp.path()).unwrap();
            let kio = retained(&repo);
            let gate = repo.kio_dir().join(".store-gate");
            match variant {
                "missing" => fs::remove_file(&gate).unwrap(),
                "nonempty" => fs::write(&gate, b"foreign").unwrap(),
                "hardlink" => fs::hard_link(&gate, temp.path().join("other-gate")).unwrap(),
                "directory" => {
                    fs::remove_file(&gate).unwrap();
                    fs::create_dir(&gate).unwrap();
                }
                _ => unreachable!(),
            }
            locked(acquire_bound_store_read_guard(&kio));
            locked(acquire_bound_store_lock(&kio));
            locked(StoreLock::acquire(repo.kio_dir()));
            assert!(!repo.kio_dir().join(".lock").exists());
        }
    }

    #[test]
    fn retained_gate_denies_replacement_and_rechecks_link_count() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let kio = retained(&repo);
        let reader = acquire_bound_store_read_guard(&kio).unwrap();
        let gate = repo.kio_dir().join(".store-gate");
        assert!(fs::rename(&gate, temp.path().join("moved-gate")).is_err());
        assert!(fs::remove_file(&gate).is_err());
        assert!(fs::write(&gate, b"changed").is_err());
        // NTFS may refuse hardlink insertion while the restrictive handle is
        // held. If permitted, the recheck must reject the changed link count.
        match fs::hard_link(&gate, temp.path().join("extra-gate")) {
            Ok(()) => locked(reader.recheck_idle()),
            Err(error) => {
                assert!(
                    matches!(error.raw_os_error(), Some(5 | 32)),
                    "unexpected hardlink error: {error}"
                );
                reader.recheck_idle().unwrap();
            }
        }
    }

    #[test]
    fn malformed_and_live_transient_locks_are_not_reclaimed_by_readers() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let kio = retained(&repo);
        for bytes in [
            b"malformed".to_vec(),
            canonical_lock_bytes(std::process::id(), "live-test").unwrap(),
            canonical_lock_bytes(123456, "dead-but-not-release-sentinel").unwrap(),
        ] {
            fs::write(repo.kio_dir().join(".lock"), &bytes).unwrap();
            locked(acquire_bound_store_read_guard(&kio));
            assert_eq!(fs::read(repo.kio_dir().join(".lock")).unwrap(), bytes);
        }
        fs::remove_file(repo.kio_dir().join(".lock")).unwrap();
        let reader = acquire_bound_store_read_guard(&kio).unwrap();
        fs::write(repo.kio_dir().join(".lock"), b"replacement").unwrap();
        locked(reader.recheck_idle());
    }

    #[test]
    fn reparse_gate_is_never_followed() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let kio = retained(&repo);
        let gate = repo.kio_dir().join(".store-gate");
        fs::remove_file(&gate).unwrap();
        let target = temp.path().join("external-empty");
        fs::write(&target, b"").unwrap();
        std::os::windows::fs::symlink_file(&target, &gate)
            .expect("native gate acceptance requires symlink creation capability");
        locked(acquire_bound_store_read_guard(&kio));
        locked(acquire_bound_store_lock(&kio));
        assert_eq!(fs::read(target).unwrap(), b"");
    }

    #[test]
    fn crash_release_child() {
        let Some(root) = std::env::var_os("KIO_TEST_WINDOWS_GATE_CRASH_ROOT") else {
            return;
        };
        let repo = Repository::open(root).unwrap();
        let kio = retained(&repo);
        let _gate = Gate::acquire(&kio, true).unwrap();
        // Exit bypasses destructors, exercising operating-system release.
        std::process::exit(0);
    }

    #[test]
    fn crashed_gate_owner_can_be_reacquired() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "scope::windows_gate::tests::crash_release_child",
                "--nocapture",
            ])
            .env("KIO_TEST_WINDOWS_GATE_CRASH_ROOT", temp.path())
            .status()
            .unwrap();
        assert!(status.success());
        let kio = retained(&repo);
        let _writer = acquire_bound_store_lock(&kio).unwrap();
    }
}
