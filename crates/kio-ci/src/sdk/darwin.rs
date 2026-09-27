use super::*;
use std::ffi::c_void;
const ACL_TYPE_EXTENDED: i32 = 0x100;
unsafe extern "C" {
    fn acl_get_fd_np(fd: i32, kind: i32) -> *mut c_void;
    fn acl_get_link_np(path: *const libc::c_char, kind: i32) -> *mut c_void;
    fn acl_get_entry(acl: *mut c_void, entry_id: i32, entry: *mut *mut c_void) -> i32;
    fn acl_init(count: i32) -> *mut c_void;
    fn acl_set_fd_np(fd: i32, acl: *mut c_void, kind: i32) -> i32;
    fn acl_free(acl: *mut c_void) -> i32;
}
struct Acl(*mut c_void);
impl Drop for Acl {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                acl_free(self.0);
            }
        }
    }
}
fn acl_present(file: Option<&File>, link: Option<&Path>) -> Result<bool> {
    let pointer = if let Some(file) = file {
        unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) }
    } else {
        let name = cstring(link.ok_or("missing ACL target")?)?;
        unsafe { acl_get_link_np(name.as_ptr(), ACL_TYPE_EXTENDED) }
    };
    if pointer.is_null() {
        require(
            std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT),
            "cannot read SDK ACL",
        )?;
        return Ok(false);
    }
    let acl = Acl(pointer);
    let mut entry = std::ptr::null_mut();
    let result = unsafe { acl_get_entry(acl.0, 0, &mut entry) };
    if result == -1 {
        require(
            std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL),
            "cannot enumerate SDK ACL",
        )?;
        return Ok(false);
    }
    require(result == 0, "unexpected ACL result")?;
    Ok(true)
}
fn clear_acl(file: &File) -> Result<()> {
    let acl = Acl(unsafe { acl_init(0) });
    require(!acl.0.is_null(), "cannot allocate empty ACL")?;
    require(
        unsafe { acl_set_fd_np(file.as_raw_fd(), acl.0, ACL_TYPE_EXTENDED) } == 0,
        "cannot clear SDK ACL",
    )
}
struct Darwin;
impl Operations for Darwin {
    fn inspect(&self, path: &Path, expected: Option<&Identity>, mutate: bool) -> Result<()> {
        let file = open_node(path)?;
        let before = io(file.metadata())?;
        kind(&before)?;
        if let Some(expected) = expected {
            require(identity(&before) == *expected, "SDK inode changed")?;
        }
        let has_acl = acl_present(Some(&file), None)?;
        let evidence = path.components().count() <= Path::new(SDKS).components().count() + 1
            || matches!(
                path.file_name().and_then(|s| s.to_str()),
                Some("SDKSettings.json" | "SystemVersion.plist")
            );
        if mutate {
            if evidence {
                println!(
                    "{}",
                    serde_json::json!({"before":path,"uid":before.uid(),"gid":before.gid(),"mode":format!("{:o}", before.mode() & 0o7777),"acl":has_acl})
                );
            }
            require(
                unsafe { libc::fchown(file.as_raw_fd(), 0, 0) } == 0,
                "cannot chown SDK node",
            )?;
            clear_acl(&file)?;
            require(
                unsafe {
                    libc::fchmod(
                        file.as_raw_fd(),
                        (before.mode() & 0o7777 & !0o022) as libc::mode_t,
                    )
                } == 0,
                "cannot chmod SDK node",
            )?;
        }
        let after = io(file.metadata())?;
        require(identity(&after) == identity(&before), "SDK inode changed")?;
        require(
            trusted(path, after.uid(), after.gid(), after.mode())
                && !acl_present(Some(&file), None)?,
            "unsafe SDK node",
        )?;
        if mutate && evidence {
            println!(
                "{}",
                serde_json::json!({"after":path,"uid":after.uid(),"gid":after.gid(),"mode":format!("{:o}",after.mode() & 0o7777),"acl":false})
            );
        }
        Ok(())
    }
    fn metadata(&self, root: &Path) -> Result<()> {
        read_metadata(root)
    }
    fn link(&self, path: &Path, entry: &Entry, mutate: bool, require_root: bool) -> Result<()> {
        let Kind::Link(target) = &entry.kind else {
            return Err("expected link".into());
        };
        let parent = open_node(path.parent().ok_or("missing parent")?)?;
        let name = cstring(Path::new(path.file_name().ok_or("missing link name")?))?;
        let inspect = || -> Result<u32> {
            let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
            require(
                unsafe {
                    libc::fstatat(
                        parent.as_raw_fd(),
                        name.as_ptr(),
                        info.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                } == 0,
                "cannot stat SDK link",
            )?;
            let info = unsafe { info.assume_init() };
            require(
                Identity(
                    info.st_dev as u64,
                    info.st_ino,
                    u32::from(info.st_mode) & 0o170000,
                ) == entry.id,
                "SDK symlink changed",
            )?;
            let mut bytes = vec![0u8; 65537];
            let count = unsafe {
                libc::readlinkat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            require(
                count >= 0 && (count as usize) < bytes.len(),
                "cannot read SDK link",
            )?;
            require(
                &bytes[..count as usize] == target.as_os_str().as_bytes(),
                "SDK symlink changed",
            )?;
            // ACL API has no *at variant. Check path identity around its no-follow read;
            // ownership mutation itself remains bound to the retained parent descriptor.
            require(
                identity(&io(fs::symlink_metadata(path))?) == entry.id,
                "SDK symlink path changed",
            )?;
            require(!acl_present(None, Some(path))?, "SDK symlink has an ACL")?;
            require(
                identity(&io(fs::symlink_metadata(path))?) == entry.id,
                "SDK symlink path changed",
            )?;
            Ok(info.st_uid)
        };
        inspect()?;
        if mutate {
            require(
                unsafe {
                    libc::fchownat(
                        parent.as_raw_fd(),
                        name.as_ptr(),
                        0,
                        0,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                } == 0,
                "cannot chown SDK symlink",
            )?;
        }
        let uid = inspect()?;
        require(!require_root || uid == 0, "unsafe SDK symlink ownership")
    }
    fn select(&self) -> Result<()> {
        command(
            "/usr/bin/xcode-select",
            &["--switch", DEVELOPER],
            false,
            &[],
        )
        .map(|_| ())
    }
}
pub fn prepare() -> Result<()> {
    guards(
        true,
        unsafe { libc::geteuid() },
        std::env::args_os().count(),
    )?;
    // Check the two shared ancestors before asking system tools to inspect Xcode.
    for path in ["/", "/Applications"] {
        Darwin.inspect(Path::new(path), None, false)?;
    }
    let (root, discovery) = discover(true)?;
    println!("{}", serde_json::json!({"discovery_before":discovery}));
    let count = normalize(&root, &Darwin)?;
    let (selected, discovery) = discover(false)?;
    require(root == selected, "selected SDK changed")?;
    println!(
        "{}",
        serde_json::json!({"discovery_after":discovery,"verified_entries":count})
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_temporary_file_acl() {
        let file = tempfile::tempfile().unwrap();
        assert!(!acl_present(Some(&file), None).unwrap());
        clear_acl(&file).unwrap();
        assert!(!acl_present(Some(&file), None).unwrap());
    }
}
