use std::{
    ffi::c_void,
    fs::File,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::Path,
};

use crate::Result;
use crate::private_fs::unsafe_file;

type Acl = *mut c_void;
type AclEntry = *mut c_void;
type AclFlagset = *mut c_void;

const ACL_TYPE_EXTENDED: i32 = 0x100;
const ACL_FIRST_ENTRY: i32 = 0;
const ACL_NEXT_ENTRY: i32 = -1;
const MAX_ACL_ENTRIES: usize = 128;
const ACL_EXTENDED_ALLOW: i32 = 1;
const ACL_ENTRY_ONLY_INHERIT: i32 = 1 << 8;
const ACL_WRITE_DATA: u64 = 1 << 2;
const ACL_DELETE: u64 = 1 << 4;
const ACL_APPEND_DATA: u64 = 1 << 5;
const ACL_DELETE_CHILD: u64 = 1 << 6;
const ACL_WRITE_ATTRIBUTES: u64 = 1 << 8;
const ACL_WRITE_EXTATTRIBUTES: u64 = 1 << 10;
const ACL_WRITE_SECURITY: u64 = 1 << 12;
const ACL_CHANGE_OWNER: u64 = 1 << 13;
const ACL_ALL_KNOWN_PERMISSIONS: u64 = (1 << 21) - 1;
const MUTATE_FILE: u64 = ACL_WRITE_DATA
    | ACL_DELETE
    | ACL_APPEND_DATA
    | ACL_WRITE_ATTRIBUTES
    | ACL_WRITE_EXTATTRIBUTES
    | ACL_WRITE_SECURITY
    | ACL_CHANGE_OWNER;
const MUTATE_DIRECTORY: u64 = MUTATE_FILE | ACL_DELETE_CHILD;

unsafe extern "C" {
    fn acl_get_fd_np(fd: i32, ty: i32) -> Acl;
    fn acl_valid(acl: Acl) -> i32;
    fn acl_free(object: *mut c_void) -> i32;
    fn acl_get_entry(acl: Acl, entry_id: i32, entry: *mut AclEntry) -> i32;
    fn acl_get_tag_type(entry: AclEntry, tag: *mut i32) -> i32;
    fn acl_get_qualifier(entry: AclEntry) -> *mut c_void;
    fn acl_get_permset_mask_np(entry: AclEntry, mask: *mut u64) -> i32;
    fn acl_get_flagset_np(object: *mut c_void, flags: *mut AclFlagset) -> i32;
    fn acl_get_flag_np(flags: AclFlagset, flag: i32) -> i32;
    fn mbr_uid_to_uuid(uid: u32, uuid: *mut u8) -> i32;
}

#[derive(Clone, Copy)]
pub(crate) enum Scope {
    PrivateParent,
    PrivateLeaf,
    ExecutableAncestor,
    ExecutableLeaf,
}

fn trusted_uuids(file: &File, current_uid: u32) -> std::result::Result<Vec<[u8; 16]>, ()> {
    // Derive the owner through the retained descriptor; permit the
    // current account, root, and the retained object owner.
    let owner_uid = file.metadata().map_err(|_| ())?.uid();
    let mut values = Vec::with_capacity(3);
    let uids = [current_uid, 0, owner_uid];
    for (index, uid) in uids.into_iter().enumerate() {
        if uids[..index].contains(&uid) {
            continue;
        }
        let mut uuid = [0_u8; 16];
        if unsafe { mbr_uid_to_uuid(uid, uuid.as_mut_ptr()) } != 0 {
            return Err(());
        }
        if !values.contains(&uuid) {
            values.push(uuid);
        }
    }
    Ok(values)
}

pub(crate) fn verify(file: &File, current_uid: u32, scope: Scope, path: &Path) -> Result<()> {
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        // ENOENT means that this descriptor has no extended ACL.
        return if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            Ok(())
        } else {
            Err(unsafe_file("cannot inspect retained macOS ACL", Some(path)))
        };
    }
    let result = (|| {
        // macOS acl_get_entry returns 0 for an entry and -1/EINVAL
        // at the end. acl_valid validates the retained extended ACL;
        // acl_valid_fd_np does not support ACL_TYPE_EXTENDED here.
        if unsafe { acl_valid(acl) } != 0 {
            return Err(());
        }
        let mut trusted = None;
        let mut entry = std::ptr::null_mut();
        let mut selector = ACL_FIRST_ENTRY;
        let mut reached_end = false;
        for _ in 0..MAX_ACL_ENTRIES {
            let state = unsafe { acl_get_entry(acl, selector, &mut entry) };
            if state == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
                reached_end = true;
                break;
            }
            if state != 0 || entry.is_null() {
                return Err(());
            }
            selector = ACL_NEXT_ENTRY;
            let mut tag = 0;
            let mut flags = std::ptr::null_mut();
            let mut mask = 0_u64;
            if unsafe { acl_get_tag_type(entry, &mut tag) } != 0
                || unsafe { acl_get_flagset_np(entry.cast(), &mut flags) } != 0
                || flags.is_null()
                || unsafe { acl_get_permset_mask_np(entry, &mut mask) } != 0
            {
                return Err(());
            }
            // INHERIT_ONLY is not effective for this retained object.
            let inherit_only = unsafe { acl_get_flag_np(flags, ACL_ENTRY_ONLY_INHERIT) };
            if inherit_only < 0 {
                return Err(());
            }
            if inherit_only == 1 {
                continue;
            }
            if tag != ACL_EXTENDED_ALLOW {
                // Denies do not grant a right; this retains normal
                // macOS deny-delete entries on private home directories.
                continue;
            }
            let forbidden = match scope {
                Scope::PrivateParent | Scope::PrivateLeaf => {
                    mask & crate::platform::macos::private_acl::ACL_ALL_KNOWN_PERMISSIONS
                }
                Scope::ExecutableAncestor => {
                    mask & crate::platform::macos::private_acl::MUTATE_DIRECTORY
                }
                Scope::ExecutableLeaf => mask & MUTATE_FILE,
            };
            if forbidden == 0 {
                continue;
            }
            // Resolve identities only for effective, relevant grants,
            // once per verification, using fresh retained metadata.
            // Resolve before allocating the qualifier so lookup errors
            // cannot leak it.
            if trusted.is_none() {
                trusted = Some(trusted_uuids(file, current_uid)?);
            }
            let qualifier = unsafe { acl_get_qualifier(entry) };
            if qualifier.is_null() {
                return Err(());
            }
            let uuid = unsafe { std::slice::from_raw_parts(qualifier.cast::<u8>(), 16) };
            let trusted_grant = trusted
                .as_ref()
                .is_some_and(|values| values.iter().any(|candidate| uuid == candidate));
            unsafe { acl_free(qualifier) };
            // A group UUID is untrusted: evaluating group membership
            // safely would otherwise authorize another local account.
            if !trusted_grant {
                return Err(());
            }
        }
        if !reached_end
            && (unsafe { acl_get_entry(acl, ACL_NEXT_ENTRY, &mut entry) } != -1
                || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL))
        {
            return Err(());
        }
        Ok(())
    })();
    unsafe { acl_free(acl) };
    result.map_err(|_| {
        unsafe_file(
            "retained macOS ACL grants an untrusted principal",
            Some(path),
        )
    })
}
