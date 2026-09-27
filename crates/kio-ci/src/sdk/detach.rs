//! Break source hardlinks by publishing an independent regular file. No metadata
//! mutation is ever performed on the retained read-only source descriptor.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
pub(super) const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
fn open_at(parent: &File, name: &CString, flags: i32) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    require(
        fd >= 0,
        &format!(
            "cannot open detach file: {}",
            std::io::Error::last_os_error()
        ),
    )?;
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn same_source(a: &Metadata, b: &Metadata) -> bool {
    identity(a) == identity(b)
        && a.len() == b.len()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && a.nlink() == b.nlink()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
struct Pending<'a> {
    parent: &'a File,
    name: CString,
    file: File,
    published: bool,
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if !self.published {
            // Do not remove a replacement if a privileged concurrent writer changed
            // the name, even though normal operation has protected this parent.
            if let Ok(named) = open_at(self.parent, &self.name, libc::O_RDONLY)
                && let (Ok(named), Ok(owned)) = (named.metadata(), self.file.metadata())
                && identity(&named) == identity(&owned)
            {
                unsafe {
                    libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0);
                }
            }
        }
    }
}
pub(super) fn detach(
    path: &Path,
    expected: &Identity,
    expected_size: u64,
    parent_id: &Identity,
) -> Result<Identity> {
    detach_with_hook(path, expected, expected_size, parent_id, || Ok(()))
}
fn detach_with_hook(
    path: &Path,
    expected: &Identity,
    expected_size: u64,
    parent_id: &Identity,
    before_publish: impl FnOnce() -> Result<()>,
) -> Result<Identity> {
    let result: Result<Identity> = (|| {
        let parent = open_node(path.parent().ok_or("missing shared-file parent")?)?;
        require(
            identity(&io(parent.metadata())?) == *parent_id,
            "shared-file parent changed",
        )?;
        let name = cstring(Path::new(
            path.file_name().ok_or("missing shared-file name")?,
        ))?;
        let mut source = open_at(&parent, &name, libc::O_RDONLY)?;
        let before = io(source.metadata())?;
        require(
            identity(&before) == *expected
                && before.len() == expected_size
                && source_kind(&before, true)? == Kind::File,
            "shared-file source changed",
        )?;
        require(
            before.len() <= MAX_FILE_BYTES,
            "shared SDK file exceeds copy byte limit",
        )?;
        let mut pending = None;
        for _ in 0..16 {
            let temporary = CString::new(format!(
                ".kio-ci-detach-{}-{}",
                std::process::id(),
                NEXT_FILE.fetch_add(1, Ordering::Relaxed)
            ))
            .map_err(|e| e.to_string())?;
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    temporary.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd >= 0 {
                pending = Some(Pending {
                    parent: &parent,
                    name: temporary,
                    file: unsafe { File::from_raw_fd(fd) },
                    published: false,
                });
                break;
            }
            require(
                std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST),
                "cannot create independent SDK file",
            )?;
        }
        let mut pending = pending.ok_or("exclusive SDK copy name limit exceeded")?;
        let copied = io(std::io::copy(
            &mut (&mut source).take(before.len() + 1),
            &mut pending.file,
        ))?;
        require(
            copied == before.len() && same_source(&before, &io(source.metadata())?),
            "shared-file source changed during copy",
        )?;
        // Preserve executable/read bits, remove write authority only on the new inode.
        require(
            unsafe {
                libc::fchmod(
                    pending.file.as_raw_fd(),
                    (before.mode() & 0o777 & !0o022) as libc::mode_t,
                )
            } == 0,
            "cannot set independent SDK file mode",
        )?;
        io(pending.file.sync_all())?;
        let copied_metadata = io(pending.file.metadata())?;
        require(
            kind(&copied_metadata)? == Kind::File && copied_metadata.len() == copied,
            "unsafe independent SDK file",
        )?;
        before_publish()?;
        let named_source = open_at(&parent, &name, libc::O_RDONLY)?;
        require(
            same_source(&before, &io(source.metadata())?)
                && same_source(&before, &io(named_source.metadata())?),
            "shared-file source changed before publication",
        )?;
        let named_copy = open_at(&parent, &pending.name, libc::O_RDONLY)?;
        require(
            identity(&io(named_copy.metadata())?) == identity(&copied_metadata),
            "independent SDK file changed before publication",
        )?;
        require(
            unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    pending.name.as_ptr(),
                    parent.as_raw_fd(),
                    name.as_ptr(),
                )
            } == 0,
            "cannot publish independent SDK file",
        )?;
        pending.published = true;
        let published = open_at(&parent, &name, libc::O_RDONLY)?;
        let after = io(published.metadata())?;
        require(
            kind(&after)? == Kind::File
                && identity(&after) == identity(&copied_metadata)
                && after.len() == copied,
            "independent SDK file changed after publication",
        )?;
        // The retained source's link count/ctime necessarily changes on rename;
        // ownership, permissions and content were never written through that inode.
        Ok(identity(&after))
    })();
    result.map_err(|error| format!("{error}: {}", path.display()))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let dir = fs::canonicalize(temp.path()).unwrap();
        let sdk = dir.join("sdk");
        fs::create_dir(&sdk).unwrap();
        let source = sdk.join("file");
        let outside = dir.join("external-alias");
        fs::write(&outside, b"sdk bytes").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o775)).unwrap();
        fs::hard_link(&outside, &source).unwrap();
        (temp, source, outside)
    }
    #[test]
    fn detaches_external_alias_without_mutating_original_metadata_or_content() {
        let (_temp, source, outside) = fixture();
        let before = fs::metadata(&outside).unwrap();
        let id = detach(
            &source,
            &identity(&before),
            before.len(),
            &identity(&fs::metadata(source.parent().unwrap()).unwrap()),
        )
        .unwrap();
        let original = fs::metadata(&outside).unwrap();
        let copied = fs::metadata(&source).unwrap();
        assert_ne!(id, identity(&original));
        assert_eq!(copied.nlink(), 1);
        assert_eq!(
            (original.uid(), original.gid(), original.mode()),
            (before.uid(), before.gid(), before.mode())
        );
        assert_eq!(fs::read(&outside).unwrap(), b"sdk bytes");
        assert_eq!(fs::read(&source).unwrap(), b"sdk bytes");
        assert_eq!(copied.mode() & 0o777, 0o755);
    }
    #[test]
    fn source_symlink_and_inode_swap_fail_before_publication_and_clean_up() {
        for as_link in [false, true] {
            let (_temp, source, outside) = fixture();
            let before = fs::metadata(&source).unwrap();
            let result = detach_with_hook(
                &source,
                &identity(&before),
                before.len(),
                &identity(&fs::metadata(source.parent().unwrap()).unwrap()),
                || {
                    fs::remove_file(&source).unwrap();
                    if as_link {
                        symlink(&outside, &source).unwrap();
                    } else {
                        fs::write(&source, "replacement").unwrap();
                    }
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert_eq!(fs::read(&outside).unwrap(), b"sdk bytes");
            assert_eq!(fs::read_dir(source.parent().unwrap()).unwrap().count(), 1);
            if as_link {
                assert!(fs::symlink_metadata(&source).unwrap().is_symlink());
            } else {
                assert_eq!(fs::read(&source).unwrap(), b"replacement");
            }
        }
    }
    #[test]
    fn publication_failure_cleans_exclusive_file_and_preserves_source() {
        let (_temp, source, outside) = fixture();
        let before = fs::metadata(&source).unwrap();
        assert!(
            detach_with_hook(
                &source,
                &identity(&before),
                before.len(),
                &identity(&fs::metadata(source.parent().unwrap()).unwrap()),
                || Err("injected publication failure".into())
            )
            .is_err()
        );
        assert_eq!(identity(&fs::metadata(&source).unwrap()), identity(&before));
        assert_eq!(fs::read_dir(source.parent().unwrap()).unwrap().count(), 1);
        assert_eq!(fs::read(&outside).unwrap(), b"sdk bytes");
    }
    #[test]
    fn parent_symlink_is_rejected() {
        let (temp, source, _) = fixture();
        let before = fs::metadata(&source).unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        symlink("sdk", root.join("redirect")).unwrap();
        assert!(
            detach(
                &root.join("redirect/file"),
                &identity(&before),
                before.len(),
                &identity(&fs::metadata(source.parent().unwrap()).unwrap())
            )
            .is_err()
        );
    }
    #[test]
    fn same_inode_content_change_is_rejected_before_publication() {
        let (_temp, source, outside) = fixture();
        let before = fs::metadata(&source).unwrap();
        let result = detach_with_hook(
            &source,
            &identity(&before),
            before.len(),
            &identity(&fs::metadata(source.parent().unwrap()).unwrap()),
            || {
                fs::write(&outside, b"new bytes").unwrap();
                Ok(())
            },
        );
        assert!(result.unwrap_err().contains("source changed"));
        assert_eq!(identity(&fs::metadata(&source).unwrap()), identity(&before));
        assert_eq!(fs::read(&source).unwrap(), b"new bytes");
        assert_eq!(fs::read_dir(source.parent().unwrap()).unwrap().count(), 1);
    }
}
