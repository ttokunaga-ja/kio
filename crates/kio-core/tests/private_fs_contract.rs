#[cfg(unix)]
mod unix {
    use kio_core::private_fs::{
        read_private_file, read_private_file_at, resolve_inherited_private_root,
        verify_private_creation_parent, verify_private_directory, verify_private_directory_handle,
        verify_trusted_executable,
    };
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
    };

    fn private_dir() -> tempfile::TempDir {
        let cwd = std::env::current_dir().expect("current directory");
        let dir = tempfile::Builder::new()
            .prefix("kio-private-fs-")
            .tempdir_in(cwd)
            .expect("private fixture");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
            .expect("private directory mode");
        dir
    }
    fn private_file(dir: &Path, name: &str, body: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).expect("write fixture");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("private file mode");
        path
    }
    #[test]
    fn reads_owner_private_regular_file() {
        let dir = private_dir();
        let file = private_file(dir.path(), "device-ca.pem", b"certificate");
        assert_eq!(
            read_private_file(&file, 1024).expect("safe private file"),
            b"certificate"
        );
    }

    #[test]
    fn retains_owner_private_directory_without_creating_a_lock_or_file() {
        let dir = private_dir();
        let before = fs::read_dir(dir.path())
            .expect("read fixture entries")
            .map(|entry| entry.expect("fixture entry").file_name())
            .collect::<Vec<_>>();
        let retained = verify_private_directory(dir.path()).expect("safe private directory");
        assert_eq!(retained.path(), dir.path());
        assert!(!retained.contains_entry(Path::new(".lock")).unwrap());
        let after = fs::read_dir(dir.path())
            .expect("read fixture entries")
            .map(|entry| entry.expect("fixture entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(before, after, "verification must not create artifacts");

        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))
            .expect("make retained directory public");
        assert!(verify_private_directory_handle(&retained).is_err());
    }

    #[test]
    fn retains_non_writable_shared_read_creation_parent_without_mutation() {
        let dir = private_dir();
        let parent = dir.path().join("xdg-data");
        fs::create_dir(&parent).expect("create creation parent");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755))
            .expect("shared-read parent mode");
        let before = fs::read_dir(&parent).unwrap().count();
        let retained = verify_private_creation_parent(&parent).expect("safe 0755 parent");
        assert_eq!(retained.path(), parent);
        assert_eq!(fs::read_dir(retained.path()).unwrap().count(), before);

        let unsafe_parent = dir.path().join("unsafe-parent");
        let child = unsafe_parent.join("creation");
        fs::create_dir(&unsafe_parent).expect("create unsafe ancestor");
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777))
            .expect("unsafe ancestor mode");
        fs::create_dir(&child).expect("create child");
        fs::set_permissions(&child, fs::Permissions::from_mode(0o755)).expect("child mode");
        assert!(verify_private_creation_parent(&child).is_err());

        let writable_final = dir.path().join("writable-final");
        fs::create_dir(&writable_final).expect("create writable final parent");
        fs::set_permissions(&writable_final, fs::Permissions::from_mode(0o777))
            .expect("writable final mode");
        assert!(verify_private_creation_parent(&writable_final).is_err());
    }

    #[test]
    fn rejects_insecure_or_symlinked_private_directory() {
        let dir = private_dir();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))
            .expect("insecure directory mode");
        assert!(verify_private_directory(dir.path()).is_err());

        let outer = private_dir();
        let insecure_parent = outer.path().join("insecure-parent");
        let private_child = insecure_parent.join("private-child");
        fs::create_dir(&insecure_parent).expect("insecure parent directory");
        fs::set_permissions(&insecure_parent, fs::Permissions::from_mode(0o777))
            .expect("insecure parent mode");
        fs::create_dir(&private_child).expect("private child directory");
        fs::set_permissions(&private_child, fs::Permissions::from_mode(0o700))
            .expect("private child mode");
        assert!(verify_private_directory(&private_child).is_err());

        let target = outer.path().join("target");
        fs::create_dir(&target).expect("target directory");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700))
            .expect("target directory mode");
        let link = outer.path().join("linked");
        symlink(&target, &link).expect("directory symlink");
        assert!(verify_private_directory(&link).is_err());
    }
    #[test]
    fn rejects_relative_and_oversized_paths() {
        let dir = private_dir();
        let file = private_file(dir.path(), "device-ca.pem", b"certificate");
        assert_eq!(
            read_private_file(Path::new("device-ca.pem"), 1024)
                .unwrap_err()
                .error_code(),
            "KIO-E-PRIVATE-FILE-UNSAFE-001"
        );
        assert_eq!(
            read_private_file(&file, 3).unwrap_err().error_code(),
            "KIO-E-PRIVATE-FILE-OVERSIZE-001"
        );
    }
    #[test]
    fn rejects_insecure_parent_or_leaf_mode() {
        let dir = private_dir();
        let file = private_file(dir.path(), "device-ca.pem", b"certificate");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("insecure leaf mode");
        assert_eq!(
            read_private_file(&file, 1024).unwrap_err().error_code(),
            "KIO-E-PRIVATE-FILE-UNSAFE-001"
        );
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("restore leaf mode");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))
            .expect("insecure parent mode");
        assert_eq!(
            read_private_file(&file, 1024).unwrap_err().error_code(),
            "KIO-E-PRIVATE-FILE-UNSAFE-001"
        );
    }
    #[test]
    fn rejects_symlink_components_and_hardlinked_or_nonregular_leaf() {
        let dir = private_dir();
        let target = private_file(dir.path(), "target.pem", b"certificate");
        let leaf_link = dir.path().join("leaf-link.pem");
        symlink(&target, &leaf_link).expect("leaf symlink");
        assert!(read_private_file(&leaf_link, 1024).is_err());
        let nested_target = dir.path().join("nested-target");
        fs::create_dir(&nested_target).expect("target directory");
        fs::set_permissions(&nested_target, fs::Permissions::from_mode(0o700))
            .expect("target mode");
        private_file(&nested_target, "device-ca.pem", b"certificate");
        let component_link = dir.path().join("linked");
        symlink(&nested_target, &component_link).expect("component symlink");
        assert!(read_private_file(&component_link.join("device-ca.pem"), 1024).is_err());
        let hard_link = dir.path().join("hard-link.pem");
        fs::hard_link(&target, &hard_link).expect("hard link");
        assert!(read_private_file(&target, 1024).is_err());
        assert!(read_private_file(dir.path(), 1024).is_err());
    }

    #[test]
    fn inherited_fd_root_remains_anchored_after_rename_for_reads_and_creation() {
        use std::os::fd::AsRawFd;

        // Keep the renamed directory within a live cleanup guard.
        let container = private_dir();
        let root = tempfile::tempdir_in(container.path()).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        private_file(root.path(), "device-ca.pem", b"anchored");
        let held = fs::File::open(root.path()).expect("retain fixture root");
        let inherited = std::path::PathBuf::from(format!("/dev/fd/{}", held.as_raw_fd()));
        let renamed = root.path().with_extension("renamed");
        fs::rename(root.path(), &renamed).expect("rename original path");

        let read_path = inherited.join("device-ca.pem");
        assert_eq!(read_private_file(&read_path, 1024).unwrap(), b"anchored");
        let parent = verify_private_creation_parent(&inherited).expect("retained inherited root");
        parent
            .create_directory(Path::new("created"))
            .expect("create through retained root");
        assert!(renamed.join("created").is_dir());
    }

    #[test]
    fn inherited_root_clone_survives_original_descriptor_replacement() {
        use std::os::fd::AsRawFd;

        let first = private_dir();
        let second = private_dir();
        private_file(first.path(), "device-ca.pem", b"first");
        private_file(second.path(), "device-ca.pem", b"second");
        let held_first = fs::File::open(first.path()).unwrap();
        let held_second = fs::File::open(second.path()).unwrap();
        let descriptor = unsafe { libc::fcntl(held_first.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 100) };
        assert!(descriptor >= 100);
        let inherited = std::path::PathBuf::from(format!("/dev/fd/{descriptor}"));
        let (root, suffix) = resolve_inherited_private_root(&inherited)
            .unwrap()
            .expect("inherited directory capability");
        assert!(suffix.is_empty());
        assert_eq!(
            unsafe { libc::dup2(held_second.as_raw_fd(), descriptor) },
            descriptor
        );
        assert_eq!(
            read_private_file_at(&root, "device-ca.pem", 1024).unwrap(),
            b"first"
        );
        assert_eq!(unsafe { libc::close(descriptor) }, 0);
    }

    #[test]
    fn inherited_fd_roots_reject_aliases_bad_descriptors_and_symlink_suffixes() {
        use std::{os::fd::AsRawFd, os::unix::fs::symlink};

        let root = private_dir();
        let held = fs::File::open(root.path()).unwrap();
        let fd = held.as_raw_fd();
        for path in [
            "/dev/fd".to_owned(),
            "/dev/fd/not-a-number".to_owned(),
            "/dev/fd/-1".to_owned(),
            "/dev/fd/01".to_owned(),
            "/dev/fd/2147483648".to_owned(),
            format!("/dev/fd/{fd}/"),
            format!("/dev/fd/{fd}//child"),
            format!("/dev/fd/{fd}/./child"),
            format!("/dev/fd/{fd}/child/../other"),
            format!("/dev//fd/{fd}/child"),
            format!("/dev/./fd/{fd}/child"),
            format!("//dev/fd/{fd}/child"),
        ] {
            assert!(
                resolve_inherited_private_root(Path::new(&path)).is_err(),
                "{path}"
            );
        }
        let file = private_file(root.path(), "not-a-directory", b"file");
        let file_fd = fs::File::open(file).unwrap();
        assert!(
            resolve_inherited_private_root(Path::new(&format!("/dev/fd/{}", file_fd.as_raw_fd())))
                .is_err()
        );

        let public = root.path().join("public");
        fs::create_dir(&public).unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
        let public_fd = fs::File::open(&public).unwrap();
        assert!(
            resolve_inherited_private_root(Path::new(&format!(
                "/dev/fd/{}",
                public_fd.as_raw_fd()
            )))
            .is_err()
        );

        let victim = private_dir();
        symlink(victim.path(), root.path().join("linked")).unwrap();
        let inherited = std::path::PathBuf::from(format!("/dev/fd/{fd}/linked"));
        assert!(verify_private_creation_parent(&inherited).is_err());
        assert!(!victim.path().join("created").exists());
    }

    #[test]
    fn inherited_closed_descriptor_is_rejected_in_isolated_process() {
        const CHILD: &str = "KIO_TEST_CLOSED_FD_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "unix::inherited_closed_descriptor_is_rejected_in_isolated_process",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        // Other parallel tests deliberately duplicate descriptors. Isolating
        // this close/import pair prevents their reuse of the same fd number
        // from turning the negative fixture into a valid inherited handle.
        use std::os::fd::AsRawFd;
        let root = private_dir();
        let held = fs::File::open(root.path()).unwrap();
        let closed = unsafe { libc::fcntl(held.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 100) };
        assert!(closed >= 100);
        assert_eq!(unsafe { libc::close(closed) }, 0);
        assert!(resolve_inherited_private_root(Path::new(&format!("/dev/fd/{closed}"))).is_err());
    }

    #[test]
    fn scheduler_executable_requires_non_writable_file_and_ancestors() {
        let dir = private_dir();
        let executable = private_file(dir.path(), "kio", b"fixture");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("executable mode");
        assert_eq!(verify_trusted_executable(&executable).unwrap(), executable);
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o722))
            .expect("writable executable mode");
        assert!(verify_trusted_executable(&executable).is_err());
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("restore executable mode");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777))
            .expect("writable ancestor mode");
        assert!(verify_trusted_executable(&executable).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn retained_extended_acl_rejects_untrusted_write_but_ignores_inherit_only() {
        // The current primary group is deliberately untrusted by production:
        // group membership can authorize another local account.
        let dir = private_dir();
        let executable = private_file(dir.path(), "kio", b"fixture");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("executable mode");
        macos_acl_fixture::allow_untrusted_write(dir.path(), false).expect("active ACL fixture");
        assert!(verify_trusted_executable(&executable).is_err());
        macos_acl_fixture::allow_untrusted_write(dir.path(), true)
            .expect("inherit-only ACL fixture");
        assert_eq!(verify_trusted_executable(&executable).unwrap(), executable);

        let private = private_file(dir.path(), "device-ca.pem", b"certificate");
        macos_acl_fixture::allow_untrusted_write(&private, false).expect("private ACL fixture");
        assert!(read_private_file(&private, 1024).is_err());

        let protected_dir = private_dir();
        macos_acl_fixture::allow_untrusted_write(protected_dir.path(), false)
            .expect("directory ACL fixture");
        assert!(verify_private_directory(protected_dir.path()).is_err());
        assert!(verify_private_creation_parent(protected_dir.path()).is_err());
    }

    #[cfg(target_os = "macos")]
    mod macos_acl_fixture {
        use std::{ffi::c_void, fs::OpenOptions, os::fd::AsRawFd, path::Path};

        type Acl = *mut c_void;
        type AclEntry = *mut c_void;
        type AclFlagset = *mut c_void;

        const ACL_TYPE_EXTENDED: i32 = 0x100;
        const ACL_EXTENDED_ALLOW: i32 = 1;
        const ACL_WRITE_DATA: u64 = 1 << 2;
        const ACL_ENTRY_FILE_INHERIT: i32 = 1 << 5;
        const ACL_ENTRY_ONLY_INHERIT: i32 = 1 << 8;

        unsafe extern "C" {
            fn acl_init(count: i32) -> Acl;
            fn acl_free(object: *mut c_void) -> i32;
            fn acl_create_entry(acl: *mut Acl, entry: *mut AclEntry) -> i32;
            fn acl_set_tag_type(entry: AclEntry, tag: i32) -> i32;
            fn acl_set_qualifier(entry: AclEntry, qualifier: *const c_void) -> i32;
            fn acl_set_permset_mask_np(entry: AclEntry, mask: u64) -> i32;
            fn acl_get_flagset_np(object: *mut c_void, flags: *mut AclFlagset) -> i32;
            fn acl_add_flag_np(flags: AclFlagset, flag: i32) -> i32;
            fn acl_set_fd_np(fd: i32, acl: Acl, ty: i32) -> i32;
            fn mbr_gid_to_uuid(gid: u32, uuid: *mut u8) -> i32;
        }

        pub(super) fn allow_untrusted_write(
            path: &Path,
            inherit_only: bool,
        ) -> std::io::Result<()> {
            let file = OpenOptions::new().read(true).open(path)?;
            let mut acl = unsafe { acl_init(1) };
            if acl.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let result = (|| {
                let mut entry = std::ptr::null_mut();
                let untrusted_uuid = current_group_uuid()?;
                if unsafe { acl_create_entry(&mut acl, &mut entry) } != 0
                    || entry.is_null()
                    || unsafe { acl_set_tag_type(entry, ACL_EXTENDED_ALLOW) } != 0
                    || unsafe { acl_set_qualifier(entry, untrusted_uuid.as_ptr().cast()) } != 0
                    || unsafe { acl_set_permset_mask_np(entry, ACL_WRITE_DATA) } != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if inherit_only {
                    let mut flags = std::ptr::null_mut();
                    if unsafe { acl_get_flagset_np(entry.cast(), &mut flags) } != 0
                        || flags.is_null()
                        || unsafe { acl_add_flag_np(flags, ACL_ENTRY_FILE_INHERIT) } != 0
                        || unsafe { acl_add_flag_np(flags, ACL_ENTRY_ONLY_INHERIT) } != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if unsafe { acl_set_fd_np(file.as_raw_fd(), acl, ACL_TYPE_EXTENDED) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })();
            unsafe { acl_free(acl) };
            result
        }

        fn current_group_uuid() -> std::io::Result<[u8; 16]> {
            let mut uuid = [0_u8; 16];
            if unsafe { mbr_gid_to_uuid(libc::getgid(), uuid.as_mut_ptr()) } == 0 {
                Ok(uuid)
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use kio_core::private_fs::{
        read_private_file, read_private_file_at, verify_owner_private_handle,
        verify_private_creation_parent, verify_private_directory, verify_private_directory_handle,
    };
    use kio_core::store_dir::StoreDirectory;
    use std::{
        fs,
        os::windows::fs::symlink_file,
        path::{Path, PathBuf},
    };

    // The production check requires an exact protected owner-only DACL. The
    // existing registry snapshot suite owns the platform ACL fixture builder;
    // native CI exercises this contract against the same owner-private shape.
    fn private_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("private fixture");
        private_acl_fixture::protect_owner_only(dir.path(), true).expect("private directory ACL");
        dir
    }
    fn private_file(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).expect("write fixture");
        private_acl_fixture::protect_owner_only(&path, false).expect("private file ACL");
        path
    }
    #[test]
    fn reads_owner_private_regular_file() {
        let dir = private_dir();
        let file = private_file(dir.path(), "device-ca.pem", b"certificate");
        assert_eq!(
            read_private_file(&file, 1024).expect("safe private file"),
            b"certificate"
        );
    }
    #[test]
    fn retains_owner_private_directory_without_mutation() {
        let dir = private_dir();
        let before = fs::read_dir(dir.path()).unwrap().count();
        let retained = verify_private_directory(dir.path()).expect("private directory");
        assert_eq!(retained.path(), dir.path());
        assert!(verify_private_directory_handle(&retained).is_ok());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), before);
        let leaf = private_file(dir.path(), "retained.pem", b"certificate");
        assert_eq!(
            read_private_file_at(&retained, "retained.pem", 1024).unwrap(),
            b"certificate"
        );
        assert_eq!(leaf, dir.path().join("retained.pem"));
        assert!(verify_private_creation_parent(dir.path()).is_ok());
    }
    #[test]
    fn rejects_ads_private_trust_file_leaves() {
        let dir = private_dir();
        let retained = verify_private_directory(dir.path()).expect("private directory");
        let file = private_file(dir.path(), "retained.pem", b"certificate");
        assert!(read_private_file_at(&retained, "retained.pem:stream", 1024).is_err());
        assert!(read_private_file(&dir.path().join("retained.pem:stream"), 1024).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"certificate");
    }
    #[test]
    fn retained_read_only_directory_can_create_private_children() {
        let dir = private_dir();
        let retained =
            verify_private_creation_parent(dir.path()).expect("retained creation parent");
        let child = retained
            .create_directory(Path::new("child"))
            .expect("private child through read-only retained directory");
        let child = StoreDirectory::from_retained(child, dir.path().join("child"))
            .expect("retained private child");
        assert!(verify_private_directory_handle(&child).is_ok());
        assert!(retained.create_directory(Path::new("child")).is_err());
        fs::rename(dir.path().join("child"), dir.path().join("renamed-child"))
            .expect("rename held child namespace entry");
        assert!(verify_private_directory_handle(&child).is_ok());
        let gate = retained
            .lock_private_gate(Path::new("gate"))
            .expect("private gate through retained parent");
        assert_eq!(gate.metadata().unwrap().len(), 0);
        assert!(verify_owner_private_handle(&gate).is_ok());
    }
    #[test]
    fn inherited_acl_is_not_owner_private_but_safe_creation_parent_is_allowed() {
        let inherited = tempfile::tempdir().expect("inherited directory fixture");
        assert!(verify_private_directory(inherited.path()).is_err());
        assert!(verify_private_creation_parent(inherited.path()).is_ok());
        let delete_child = private_dir();
        private_acl_fixture::allow_untrusted_delete_child(delete_child.path())
            .expect("untrusted delete-child ACL fixture");
        assert!(verify_private_creation_parent(delete_child.path()).is_err());
        let dir = private_dir();
        let link = dir.path().join("directory-link");
        std::os::windows::fs::symlink_dir(dir.path(), &link).expect("directory reparse point");
        assert!(verify_private_directory(&link).is_err());
    }
    #[test]
    fn rejects_inherited_acl_final_leaf_and_parent() {
        let dir = tempfile::tempdir().expect("inherited directory fixture");
        let file = dir.path().join("device-ca.pem");
        fs::write(&file, b"certificate").expect("write fixture");
        assert!(read_private_file(&file, 1024).is_err());
        private_acl_fixture::protect_owner_only(dir.path(), true).expect("private parent ACL");
        assert!(read_private_file(&file, 1024).is_err());
    }
    #[test]
    fn rejects_final_reparse_and_hardlink() {
        let dir = private_dir();
        let target = private_file(dir.path(), "target.pem", b"certificate");
        let link = dir.path().join("link.pem");
        symlink_file(&target, &link).expect("create file reparse point");
        assert!(read_private_file(&link, 1024).is_err());
        let hard_link = dir.path().join("hard-link.pem");
        fs::hard_link(&target, &hard_link).expect("hard link");
        assert!(read_private_file(&target, 1024).is_err());
    }

    mod private_acl_fixture {
        // Kept here so the core contract can run without making the index
        // crate's private registry test helper a cross-crate test API.
        use std::{
            fs::OpenOptions,
            mem,
            os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
            path::Path,
            ptr,
        };
        use windows_sys::Win32::{
            Foundation::{CloseHandle, HANDLE},
            Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo},
            Security::{
                ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAceEx, CreateWellKnownSid,
                DACL_SECURITY_INFORMATION, GetLengthSid, GetTokenInformation, InitializeAcl,
                OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID, TOKEN_QUERY,
                TOKEN_USER, TokenUser, WinBuiltinUsersSid,
            },
            Storage::FileSystem::{
                FILE_ALL_ACCESS, FILE_DELETE_CHILD, FILE_READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC,
                WRITE_OWNER,
            },
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        };
        pub fn protect_owner_only(path: &Path, directory: bool) -> std::io::Result<()> {
            let owner = CurrentUserSid::current()?;
            let mut acl = owner_only_acl(&owner)?;
            let mut options = OpenOptions::new();
            options
                .read(true)
                .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC | WRITE_OWNER);
            if directory {
                options.custom_flags(0x0200_0000);
            }
            let file = options.open(path)?;
            // SAFETY: all buffers remain valid through this exact-handle call.
            let status = unsafe {
                SetSecurityInfo(
                    file.as_raw_handle() as HANDLE,
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION
                        | DACL_SECURITY_INFORMATION
                        | PROTECTED_DACL_SECURITY_INFORMATION,
                    owner.as_psid(),
                    ptr::null_mut(),
                    acl.as_mut_ptr().cast::<ACL>(),
                    ptr::null_mut(),
                )
            };
            if status == 0 {
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(status as i32))
            }
        }
        pub fn allow_untrusted_delete_child(path: &Path) -> std::io::Result<()> {
            let owner = CurrentUserSid::current()?;
            let mut acl = owner_and_users_delete_child_acl(&owner)?;
            let file = OpenOptions::new()
                .read(true)
                .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC | WRITE_OWNER)
                .custom_flags(0x0200_0000)
                .open(path)?;
            // SAFETY: the ACL storage remains valid for this exact-handle call.
            let status = unsafe {
                SetSecurityInfo(
                    file.as_raw_handle() as HANDLE,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    acl.as_mut_ptr().cast::<ACL>(),
                    ptr::null_mut(),
                )
            };
            if status == 0 {
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(status as i32))
            }
        }

        fn owner_and_users_delete_child_acl(owner: &CurrentUserSid) -> std::io::Result<Vec<usize>> {
            let users = builtin_users_sid()?;
            let fixed = mem::size_of::<ACCESS_ALLOWED_ACE>() - mem::size_of::<u32>();
            let acl_size = mem::size_of::<ACL>() + fixed + owner.len() + fixed + users.len();
            let mut acl = vec![0_usize; acl_size.div_ceil(mem::size_of::<usize>())];
            // SAFETY: aligned storage is large enough for the two ACEs below.
            if unsafe { InitializeAcl(acl.as_mut_ptr().cast(), acl_size as u32, ACL_REVISION) } == 0
                || unsafe {
                    AddAccessAllowedAceEx(
                        acl.as_mut_ptr().cast(),
                        ACL_REVISION,
                        0,
                        FILE_ALL_ACCESS,
                        owner.as_psid(),
                    )
                } == 0
                || unsafe {
                    AddAccessAllowedAceEx(
                        acl.as_mut_ptr().cast(),
                        ACL_REVISION,
                        0,
                        FILE_DELETE_CHILD,
                        users.as_ptr().cast_mut().cast(),
                    )
                } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(acl)
        }

        fn builtin_users_sid() -> std::io::Result<Vec<u8>> {
            let mut bytes = 0_u32;
            // SAFETY: null SID is the documented size probe.
            let _ = unsafe {
                CreateWellKnownSid(
                    WinBuiltinUsersSid,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut bytes,
                )
            };
            if bytes == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut sid = vec![0_u8; bytes as usize];
            // SAFETY: `sid` has the requested size and remains valid for the call.
            if unsafe {
                CreateWellKnownSid(
                    WinBuiltinUsersSid,
                    ptr::null_mut(),
                    sid.as_mut_ptr().cast(),
                    &mut bytes,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            sid.truncate(bytes as usize);
            Ok(sid)
        }

        fn owner_only_acl(owner: &CurrentUserSid) -> std::io::Result<Vec<usize>> {
            let ace_size =
                mem::size_of::<ACCESS_ALLOWED_ACE>() - mem::size_of::<u32>() + owner.len();
            let acl_size = mem::size_of::<ACL>() + ace_size;
            let mut acl = vec![0_usize; acl_size.div_ceil(mem::size_of::<usize>())];
            // SAFETY: aligned storage is large enough for one ACE.
            if unsafe { InitializeAcl(acl.as_mut_ptr().cast(), acl_size as u32, ACL_REVISION) } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: initialized ACL has space for its one owner ACE.
            if unsafe {
                AddAccessAllowedAceEx(
                    acl.as_mut_ptr().cast(),
                    ACL_REVISION,
                    0,
                    FILE_ALL_ACCESS,
                    owner.as_psid(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(acl)
        }
        struct CurrentUserSid {
            bytes: Vec<usize>,
        }
        impl CurrentUserSid {
            fn current() -> std::io::Result<Self> {
                let mut token = ptr::null_mut();
                // SAFETY: valid current process pseudo-handle and output pointer.
                if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let result = Self::from_token(token);
                // SAFETY: OpenProcessToken returned this handle.
                unsafe { CloseHandle(token) };
                result
            }
            fn from_token(token: HANDLE) -> std::io::Result<Self> {
                let mut needed = 0_u32;
                // SAFETY: null size probe is documented by GetTokenInformation.
                let _ = unsafe {
                    GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed)
                };
                if needed == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut words = vec![0_usize; (needed as usize).div_ceil(mem::size_of::<usize>())];
                // SAFETY: storage has at least the requested bytes.
                if unsafe {
                    GetTokenInformation(
                        token,
                        TokenUser,
                        words.as_mut_ptr().cast(),
                        needed,
                        &mut needed,
                    )
                } == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                // SAFETY: successful TokenUser response contains TOKEN_USER.
                let sid = unsafe { (*(words.as_ptr().cast::<TOKEN_USER>())).User.Sid };
                if sid.is_null() {
                    return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
                }
                // SAFETY: SID points into the valid token response.
                let len = unsafe { GetLengthSid(sid) } as usize;
                if len == 0 || len > needed as usize {
                    return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
                }
                let mut bytes = vec![0_usize; len.div_ceil(mem::size_of::<usize>())];
                // SAFETY: valid non-overlapping buffers of `len` bytes.
                unsafe {
                    ptr::copy_nonoverlapping(sid.cast::<u8>(), bytes.as_mut_ptr().cast::<u8>(), len)
                };
                Ok(Self { bytes })
            }
            fn as_psid(&self) -> PSID {
                self.bytes.as_ptr().cast_mut().cast()
            }
            fn len(&self) -> usize {
                // SAFETY: `self` owns the validated SID buffer.
                unsafe { GetLengthSid(self.as_psid()) as usize }
            }
        }
    }
}
