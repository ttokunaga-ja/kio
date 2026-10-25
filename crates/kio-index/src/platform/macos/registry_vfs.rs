//! Darwin's thread-local cwd binds native SQLite relative filenames to one
//! retained directory without changing the process cwd or another thread.
pub(super) use super::registry_unix_native::{configure_open, native_matches, release_shm_pin};
use crate::{IndexError, Result};
use kio_core::store_dir::StoreDirectory;
use std::{
    cell::{Cell, RefCell},
    ffi::{CStr, CString, c_int},
    fs::File,
    os::fd::{AsRawFd, RawFd},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

unsafe extern "C" {
    fn pthread_fchdir_np(fd: c_int) -> c_int;
}
thread_local! {
    static ACTIVE: RefCell<Vec<RawFd>> = const { RefCell::new(Vec::new()) };
    static POISONED: Cell<bool> = const { Cell::new(false) };
    #[cfg(test)]
    static FAIL_RESTORE: Cell<bool> = const { Cell::new(false) };
}

pub(super) struct Binding {
    directory: File,
    locator: CString,
    path: PathBuf,
    failed: AtomicBool,
}
pub(super) fn bind(path: &Path, directory: &StoreDirectory) -> Result<Binding> {
    use std::os::unix::ffi::OsStrExt;
    let leaf = path
        .file_name()
        .ok_or_else(|| IndexError::Schema("registry path has no leaf".into()))?;
    let path = PathBuf::from(leaf);
    let locator = CString::new(path.as_os_str().as_bytes())
        .map_err(|error| IndexError::Schema(error.to_string()))?;
    let directory = directory
        .root_handle()
        .try_clone()
        .map_err(|error| IndexError::Schema(error.to_string()))?;
    Ok(Binding {
        directory,
        locator,
        path,
        failed: AtomicBool::new(false),
    })
}
fn set_directory(fd: RawFd) -> std::io::Result<()> {
    loop {
        // SAFETY: fd is either a live retained directory or -1, Darwin's
        // documented reset to the process-wide cwd for the calling thread.
        if unsafe { pthread_fchdir_np(fd) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
struct Guard<'a> {
    binding: &'a Binding,
    previous: Option<RawFd>,
    active: bool,
}
impl Guard<'_> {
    fn restore(&mut self) -> bool {
        if !self.active {
            return !self.binding.failed.load(Ordering::Acquire);
        }
        self.active = false;
        let matched = ACTIVE.with(|active| active.borrow_mut().pop())
            == Some(self.binding.directory.as_raw_fd());
        let restored = set_directory(self.previous.unwrap_or(-1)).is_ok();
        #[cfg(test)]
        let restored = restored && !FAIL_RESTORE.with(|fail| fail.replace(false));
        if !matched || !restored {
            self.binding.failed.store(true, Ordering::Release);
            POISONED.with(|poisoned| poisoned.set(true));
            return false;
        }
        true
    }
}
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.restore();
    }
}
impl Binding {
    pub(super) fn locator(&self) -> &CStr {
        &self.locator
    }
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn execute(&self, failure: c_int, operation: impl FnOnce() -> c_int) -> c_int {
        if self.failed.load(Ordering::Acquire) || POISONED.with(Cell::get) {
            return failure;
        }
        let previous = ACTIVE.with(|active| active.borrow().last().copied());
        if set_directory(self.directory.as_raw_fd()).is_err() {
            return failure;
        }
        ACTIVE.with(|active| active.borrow_mut().push(self.directory.as_raw_fd()));
        let mut guard = Guard {
            binding: self,
            previous,
            active: true,
        };
        let result = operation();
        if guard.restore() { result } else { failure }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    fn fixture() -> (tempfile::TempDir, Binding) {
        let directory = tempfile::tempdir().unwrap();
        let retained = StoreDirectory::open(directory.path()).unwrap();
        let binding = bind(&directory.path().join("registry.sqlite"), &retained).unwrap();
        (directory, binding)
    }
    fn cwd() -> PathBuf {
        std::env::current_dir().unwrap().canonicalize().unwrap()
    }
    #[test]
    fn registry_thread_directory_restores_nested_error_and_panic() {
        let original = cwd();
        let (directory_a, binding_a) = fixture();
        let (directory_b, binding_b) = fixture();
        let a = directory_a.path().canonicalize().unwrap();
        let b = directory_b.path().canonicalize().unwrap();
        assert_eq!(
            binding_a.execute(-1, || {
                assert_eq!(cwd(), a);
                assert_eq!(
                    binding_b.execute(-1, || {
                        assert_eq!(cwd(), b);
                        -7
                    }),
                    -7
                );
                assert_eq!(cwd(), a);
                1
            }),
            1
        );
        assert_eq!(cwd(), original);
        assert!(
            catch_unwind(AssertUnwindSafe(
                || binding_a.execute(-1, || panic!("guard cleanup"))
            ))
            .is_err()
        );
        assert_eq!(cwd(), original);
    }
    #[test]
    fn registry_thread_directory_isolated_between_threads() {
        use std::{sync::mpsc, time::Duration};
        let original = cwd();
        let (directory_a, a) = fixture();
        let (directory_b, b) = fixture();
        let expected_a = directory_a.path().canonicalize().unwrap();
        let expected_b = directory_b.path().canonicalize().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let mut releases = Vec::new();
        let mut threads = Vec::new();
        for (binding, expected) in [(a, expected_a), (b, expected_b)] {
            let ready_tx = ready_tx.clone();
            let (release_tx, release_rx) = mpsc::channel();
            releases.push(release_tx);
            threads.push(std::thread::spawn(move || {
                binding.execute(-1, || {
                    assert_eq!(cwd(), expected);
                    ready_tx.send(()).unwrap();
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("isolation test release timed out");
                    assert_eq!(cwd(), expected);
                    1
                })
            }));
        }
        drop(ready_tx);
        for _ in 0..2 {
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("native thread directory binding did not become ready");
        }
        assert_eq!(cwd(), original);
        for release in releases {
            release.send(()).unwrap();
        }
        for thread in threads {
            assert_eq!(thread.join().unwrap(), 1);
        }
        assert_eq!(cwd(), original);
    }
    #[test]
    fn registry_thread_directory_restore_failure_is_latched() {
        std::thread::spawn(|| {
            let original = cwd();
            let (_directory, binding) = fixture();
            FAIL_RESTORE.with(|fail| fail.set(true));
            assert_eq!(binding.execute(-1, || 1), -1);
            assert_eq!(cwd(), original);
            assert_eq!(
                binding.execute(-1, || panic!("poisoned binding reused")),
                -1
            );
            let (_other, other) = fixture();
            assert_eq!(other.execute(-1, || panic!("poisoned thread reused")), -1);
        })
        .join()
        .unwrap();
    }
}
