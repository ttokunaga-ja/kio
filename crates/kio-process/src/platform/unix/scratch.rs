use crate::*;
use std::fs::File;

pub(crate) fn sampled_scratch_bytes(
    root: &File,
    root_path: &std::path::Path,
) -> Result<u64, std::io::Error> {
    use std::{
        ffi::{CStr, CString},
        os::{fd::AsRawFd, unix::fs::MetadataExt},
    };

    const MAX_SCAN_DEPTH: usize = 32;
    const MAX_SCAN_ENTRIES: usize = 10_000;
    // This is an observation budget, not a resource quota for the child. A
    // short scheduler preemption must not make an otherwise bounded tree
    // fail; the renderer's independent command timeout remains authoritative.
    const SCAN_DEADLINE: Duration = Duration::from_secs(1);

    struct Directory(*mut libc::DIR);

    impl Directory {
        fn open(fd: libc::c_int) -> Result<Self, std::io::Error> {
            // SAFETY: `fd` is an owned descriptor returned by `openat`. On
            // success `fdopendir` transfers that ownership to `closedir`.
            let directory = unsafe { libc::fdopendir(fd) };
            if directory.is_null() {
                let error = std::io::Error::last_os_error();
                // SAFETY: `fdopendir` did not take ownership on failure.
                unsafe { libc::close(fd) };
                return Err(error);
            }
            Ok(Self(directory))
        }

        fn fd(&self) -> libc::c_int {
            // SAFETY: `self.0` is owned by this `Directory` and remains live
            // until `closedir` in `Drop`.
            unsafe { libc::dirfd(self.0) }
        }

        fn next(&mut self) -> Result<Option<&CStr>, std::io::Error> {
            // `readdir` distinguishes end-of-directory from an error through
            // errno, so clear it before every call.
            crate::platform::clear_errno();
            // SAFETY: `self.0` remains valid until `Drop`; the returned entry
            // is consumed before another `readdir` call.
            let entry = unsafe { libc::readdir(self.0) };
            if entry.is_null() {
                let error = std::io::Error::last_os_error();
                return if error.raw_os_error() == Some(0) {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            // SAFETY: POSIX guarantees a NUL-terminated `d_name` in a
            // successful `readdir` result.
            Ok(Some(unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }))
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            // SAFETY: `Directory` exclusively owns this DIR pointer.
            unsafe { libc::closedir(self.0) };
        }
    }

    fn scan_depth_exceeded() -> std::io::Error {
        std::io::Error::other("renderer scratch scan exceeded depth limit")
    }

    fn scan_entry_limit_exceeded() -> std::io::Error {
        std::io::Error::other("renderer scratch scan exceeded entry limit")
    }

    fn scan_deadline_exceeded() -> std::io::Error {
        std::io::Error::other("renderer scratch scan exceeded one-second observation budget")
    }

    fn disappeared(error: &std::io::Error) -> bool {
        matches!(error.raw_os_error(), Some(libc::ENOENT))
    }

    fn open_directory(parent: libc::c_int, name: &CStr) -> Result<Directory, std::io::Error> {
        // SAFETY: `parent` is a live directory descriptor and `name` is a
        // NUL-terminated single component obtained from `readdir`.
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Directory::open(fd)
    }

    fn same_identity(left: &libc::stat, right: &libc::stat) -> bool {
        left.st_dev == right.st_dev && left.st_ino == right.st_ino
    }

    fn visit(
        directory: Directory,
        depth: usize,
        total: &mut u64,
        entries: &mut usize,
        started: Instant,
    ) -> Result<(), std::io::Error> {
        if depth > MAX_SCAN_DEPTH {
            return Err(scan_depth_exceeded());
        }
        if started.elapsed() > SCAN_DEADLINE {
            return Err(scan_deadline_exceeded());
        }
        let parent = directory.fd();
        let mut directory = directory;
        while let Some(name) = directory.next()? {
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            *entries = entries
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("renderer scratch entry count overflow"))?;
            if *entries > MAX_SCAN_ENTRIES {
                return Err(scan_entry_limit_exceeded());
            }
            if started.elapsed() > SCAN_DEADLINE {
                return Err(scan_deadline_exceeded());
            }
            let name = CString::new(name.to_bytes()).expect("readdir names cannot contain NUL");
            let mut before = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: `parent` is a live directory descriptor, `name` is a
            // NUL-terminated child name, and `before` has sufficient storage.
            let result = unsafe {
                libc::fstatat(
                    parent,
                    name.as_ptr(),
                    before.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if disappeared(&error) {
                    continue;
                }
                return Err(error);
            }
            // SAFETY: successful `fstatat` initialized `before`.
            let before = unsafe { before.assume_init() };
            match before.st_mode & libc::S_IFMT {
                libc::S_IFREG => {
                    let size = u64::try_from(before.st_size).map_err(|_| {
                        std::io::Error::other("renderer scratch regular file has a negative size")
                    })?;
                    *total = total
                        .checked_add(size)
                        .ok_or_else(|| std::io::Error::other("renderer scratch size overflow"))?;
                }
                libc::S_IFDIR => {
                    let child = match open_directory(parent, &name) {
                        Ok(child) => child,
                        Err(error) if disappeared(&error) => continue,
                        Err(error) => return Err(error),
                    };
                    let mut after = std::mem::MaybeUninit::<libc::stat>::uninit();
                    // SAFETY: the descriptor backing `child` is live and
                    // `after` has sufficient storage for `fstat`.
                    if unsafe { libc::fstat(child.fd(), after.as_mut_ptr()) } != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // SAFETY: successful `fstat` initialized `after`.
                    let after = unsafe { after.assume_init() };
                    if !same_identity(&before, &after) {
                        return Err(std::io::Error::other(
                            "renderer scratch directory changed during scan",
                        ));
                    }
                    visit(child, depth + 1, total, entries, started)?;
                }
                libc::S_IFLNK => {
                    return Err(std::io::Error::other("renderer scratch contains a symlink"));
                }
                // LibreOffice's confined single-instance IPC is a pathname
                // Unix socket beneath this invocation's private scratch.
                // It consumes an entry but does not represent scratch bytes.
                libc::S_IFSOCK => {}
                _ => {
                    return Err(std::io::Error::other(
                        "renderer scratch contains a non-regular file",
                    ));
                }
            }
        }
        Ok(())
    }
    let retained = root.metadata()?;
    let named = std::fs::symlink_metadata(root_path)?;
    if !named.is_dir() || named.dev() != retained.dev() || named.ino() != retained.ino() {
        return Err(std::io::Error::other(
            "renderer scratch changed after retained monitor setup",
        ));
    }
    let mut total = 0;
    let mut entries = 0;
    let directory = open_directory(root.as_raw_fd(), c".")?;
    visit(directory, 0, &mut total, &mut entries, Instant::now())?;
    Ok(total)
}

#[cfg(test)]
mod unix_renderer_scratch_tests {
    use std::{fs, fs::File};

    use super::sampled_scratch_bytes;

    #[test]
    fn counts_regular_files_in_nested_scratch_directories() {
        let scratch = tempfile::tempdir().unwrap();
        fs::write(scratch.path().join("top.bin"), [0_u8; 3]).unwrap();
        fs::create_dir(scratch.path().join("nested")).unwrap();
        fs::write(scratch.path().join("nested").join("child.bin"), [0_u8; 5]).unwrap();

        assert_eq!(
            sampled_scratch_bytes(&File::open(scratch.path()).unwrap(), scratch.path()).unwrap(),
            8
        );
    }

    #[test]
    fn rejects_a_named_root_that_no_longer_matches_the_retained_directory() {
        let parent = tempfile::tempdir().unwrap();
        let scratch = parent.path().join("scratch");
        fs::create_dir(&scratch).unwrap();
        let retained = File::open(&scratch).unwrap();
        fs::rename(&scratch, parent.path().join("former-scratch")).unwrap();
        fs::create_dir(&scratch).unwrap();

        let error = sampled_scratch_bytes(&retained, &scratch).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("changed after retained monitor setup")
        );
    }

    #[test]
    fn rejects_a_descendant_swapped_for_a_symlink() {
        use std::os::unix::fs::symlink;

        let scratch = tempfile::tempdir().unwrap();
        let child = scratch.path().join("child");
        fs::create_dir(&child).unwrap();
        fs::write(child.join("before-swap.bin"), [0_u8; 1]).unwrap();
        let parked = scratch.path().join("parked-child");
        fs::rename(&child, &parked).unwrap();
        symlink(&parked, &child).unwrap();

        let error = sampled_scratch_bytes(&File::open(scratch.path()).unwrap(), scratch.path())
            .unwrap_err();
        assert!(error.to_string().contains("contains a symlink"));
    }

    #[test]
    fn ignores_private_unix_socket_bytes_without_accepting_other_special_files() {
        use std::os::unix::net::UnixListener;

        let scratch = tempfile::tempdir().unwrap();
        let socket = UnixListener::bind(scratch.path().join("renderer.sock")).unwrap();

        assert_eq!(
            sampled_scratch_bytes(&File::open(scratch.path()).unwrap(), scratch.path()).unwrap(),
            0
        );
        drop(socket);
    }

    #[test]
    fn rejects_scratch_trees_deeper_than_the_bounded_scan_depth() {
        let scratch = tempfile::tempdir().unwrap();
        let mut current = scratch.path().to_owned();
        for index in 0..33 {
            current.push(format!("level-{index}"));
            fs::create_dir(&current).unwrap();
        }

        let error = sampled_scratch_bytes(&File::open(scratch.path()).unwrap(), scratch.path())
            .unwrap_err();
        assert!(error.to_string().contains("depth limit"));
    }

    #[test]
    fn rejects_scratch_trees_exceeding_the_entry_bound() {
        let scratch = tempfile::tempdir().unwrap();
        for index in 0..10_001 {
            fs::write(scratch.path().join(format!("entry-{index}")), b"").unwrap();
        }

        let error = sampled_scratch_bytes(&File::open(scratch.path()).unwrap(), scratch.path())
            .unwrap_err();
        assert!(error.to_string().contains("entry limit"));
    }
}
