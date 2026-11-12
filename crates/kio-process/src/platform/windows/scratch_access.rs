//! Invocation-only scratch ACL lease. This policy must never be used for stores.
use super::*;
use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, EqualSid, GetAce, GetSecurityDescriptorControl,
    GetSecurityDescriptorOwner, IsValidSid, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
};
use windows_sys::Win32::Storage::FileSystem::WRITE_OWNER;

const APP_ACCESS: u32 = FILE_ALL_ACCESS & !(WRITE_DAC | WRITE_OWNER);

fn denied(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, message)
}

pub(super) struct CurrentUser(Vec<usize>);
impl CurrentUser {
    pub(super) fn new() -> std::io::Result<Self> {
        let mut token = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let token = OwnedHandle(token);
        let mut size = 0;
        unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut size) };
        if (size as usize) < std::mem::size_of::<TOKEN_USER>() {
            return Err(denied("invalid current user token"));
        }
        let mut buffer = vec![0_usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                size,
                &mut size,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(buffer))
    }
    fn sid(&self) -> PSID {
        unsafe { (&*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

// GetSecurityInfo allocates the descriptor even when only the owner is needed.
fn descriptor(handle: HANDLE) -> std::io::Result<*mut std::ffi::c_void> {
    let mut result = std::ptr::null_mut();
    let code = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut result,
        )
    };
    if code != 0 {
        if !result.is_null() {
            unsafe {
                LocalFree(result);
            }
        }
        return Err(std::io::Error::from_raw_os_error(code as i32));
    }
    if result.is_null() {
        return Err(denied("missing scratch descriptor"));
    }
    Ok(result)
}

fn verify_owner(descriptor: *mut std::ffi::c_void, owner: &CurrentUser) -> std::io::Result<()> {
    let mut actual = std::ptr::null_mut();
    let mut defaulted = 0;
    if unsafe { GetSecurityDescriptorOwner(descriptor, &mut actual, &mut defaulted) } == 0
        || actual.is_null()
        || defaulted != 0
        || unsafe { EqualSid(actual, owner.sid()) } == 0
    {
        return Err(denied("scratch owner is not current user"));
    }
    Ok(())
}

// Only newly and exclusively created empty scratch is eligible for this
// initialization; callers already prove emptiness with the retained handle.
pub(super) fn protect_empty(handle: HANDLE) -> std::io::Result<()> {
    let owner = CurrentUser::new()?;
    let value = descriptor(handle)?;
    let result = verify_owner(value, &owner);
    unsafe {
        LocalFree(value);
    }
    result?;
    set_acl(handle, &owner, None, false)
}

fn verify_acl(
    handle: HANDLE,
    owner: &CurrentUser,
    app: Option<PSID>,
    inherited: bool,
) -> std::io::Result<()> {
    use windows_sys::Win32::Security::GetSecurityDescriptorDacl;
    let value = descriptor(handle)?;
    let result = (|| {
        verify_owner(value, owner)?;
        let mut control = 0;
        let mut revision = 0;
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = std::ptr::null_mut();
        if unsafe { GetSecurityDescriptorControl(value, &mut control, &mut revision) } == 0
            || unsafe { GetSecurityDescriptorDacl(value, &mut present, &mut acl, &mut defaulted) }
                == 0
            || present == 0
            || acl.is_null()
            || defaulted != 0
            || revision != 1
            || (!inherited && control & SE_DACL_PROTECTED == 0)
        {
            return Err(denied("scratch DACL is not protected and explicit"));
        }
        let count = unsafe { (*acl).AceCount };
        if usize::from(count) != if app.is_some() { 2 } else { 1 } {
            return Err(denied("scratch contains an unknown principal"));
        }
        let mut seen_owner = false;
        let mut seen_app = false;
        for index in 0..count {
            let mut ace = std::ptr::null_mut();
            if unsafe { GetAce(acl, u32::from(index), &mut ace) } == 0 || ace.is_null() {
                return Err(denied("invalid scratch ACE"));
            }
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            let offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
            if header.AceType != 0
                || usize::from(header.AceSize) < offset + 8
                || u32::from(header.AceFlags) & !(OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE | 0x10)
                    != 0
                || (app.is_none() && header.AceFlags != 0)
            {
                return Err(denied("unsupported scratch ACE"));
            }
            let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            let sid = std::ptr::addr_of!(allowed.SidStart)
                .cast_mut()
                .cast::<std::ffi::c_void>();
            // Bound the variable-length SID before Win32 inspects it.
            let subauthorities = unsafe { *sid.cast::<u8>().add(1) } as usize;
            if usize::from(header.AceSize) != offset + 8 + 4 * subauthorities
                || unsafe { IsValidSid(sid) } == 0
            {
                return Err(denied("invalid scratch SID or access mask"));
            }
            if unsafe { EqualSid(sid, owner.sid()) } != 0
                && !seen_owner
                && allowed.Mask == FILE_ALL_ACCESS
            {
                seen_owner = true;
            } else if let Some(app) = app
                && unsafe { EqualSid(sid, app) } != 0
                && !seen_app
                && allowed.Mask == APP_ACCESS
            {
                seen_app = true;
            } else {
                return Err(denied("scratch contains an unknown or duplicate principal"));
            }
        }
        if !seen_owner || (app.is_some() && !seen_app) {
            return Err(denied("scratch lacks exact invocation authority"));
        }
        Ok(())
    })();
    unsafe {
        LocalFree(value);
    }
    result
}

fn set_acl(
    handle: HANDLE,
    owner: &CurrentUser,
    app: Option<PSID>,
    directory: bool,
) -> std::io::Result<()> {
    let flags = if app.is_some() && directory {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    };
    let entry = |sid: PSID, mask: u32| {
        let mut value = EXPLICIT_ACCESS_W {
            grfAccessPermissions: mask,
            grfAccessMode: SET_ACCESS,
            grfInheritance: flags,
            ..Default::default()
        };
        value.Trustee.TrusteeForm = TRUSTEE_IS_SID;
        value.Trustee.TrusteeType = TRUSTEE_IS_USER;
        value.Trustee.ptstrName = sid.cast();
        value
    };
    let entries = [
        entry(owner.sid(), FILE_ALL_ACCESS),
        entry(app.unwrap_or(owner.sid()), APP_ACCESS),
    ];
    let mut acl = std::ptr::null_mut();
    let code = unsafe {
        SetEntriesInAclW(
            if app.is_some() { 2 } else { 1 },
            entries.as_ptr(),
            std::ptr::null(),
            &mut acl,
        )
    };
    if code != 0 {
        return Err(std::io::Error::from_raw_os_error(code as i32));
    }
    let code = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    unsafe {
        LocalFree(acl.cast());
    }
    if code != 0 {
        return Err(std::io::Error::from_raw_os_error(code as i32));
    }
    verify_acl(handle, owner, app, false)
}

struct Entry {
    path: PathBuf,
    handle: OwnedHandle,
    info: BY_HANDLE_FILE_INFORMATION,
}

pub(super) struct ScratchGrant {
    root: PathBuf,
    owner: CurrentUser,
    app: PSID,
    initial: Vec<Entry>,
    pub(super) stopped: bool,
    retired: bool,
}

fn same_identity(a: &BY_HANDLE_FILE_INFORMATION, b: &BY_HANDLE_FILE_INFORMATION) -> bool {
    a.dwVolumeSerialNumber == b.dwVolumeSerialNumber
        && a.nFileIndexHigh == b.nFileIndexHigh
        && a.nFileIndexLow == b.nFileIndexLow
        && (a.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY)
            == (b.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY)
}

impl Entry {
    fn acl_writer(&self) -> std::io::Result<OwnedHandle> {
        let (handle, info) = open_scratch_entry(
            &self.path,
            FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        )?;
        if !same_identity(&self.info, &info) {
            return Err(denied("scratch ACL mutation target changed identity"));
        }
        Ok(handle)
    }
}

impl ScratchGrant {
    pub(super) fn new(root: &Path, app: PSID) -> std::io::Result<Self> {
        let mut value = Self {
            root: root.to_owned(),
            owner: CurrentUser::new()?,
            app,
            initial: Vec::new(),
            stopped: true,
            retired: false,
        };
        // Validate the whole staged tree before changing any object.
        let entries = value.collect(false)?;
        for (granted, entry) in entries.iter().enumerate() {
            let writer = entry.acl_writer()?;
            verify_acl(writer.0, &value.owner, None, false)?;
            if let Err(error) = set_acl(
                writer.0,
                &value.owner,
                Some(app),
                entry.info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            ) {
                for previous in entries.iter().take(granted) {
                    let rollback = previous.acl_writer()?;
                    verify_acl(rollback.0, &value.owner, Some(app), false)?;
                    set_acl(rollback.0, &value.owner, None, false)?;
                }
                return Err(error);
            }
        }
        value.initial = entries;
        Ok(value)
    }

    fn collect(&self, active: bool) -> std::io::Result<Vec<Entry>> {
        fn visit(
            value: &ScratchGrant,
            path: &Path,
            depth: usize,
            active: bool,
            result: &mut Vec<Entry>,
            start: Instant,
        ) -> std::io::Result<()> {
            if depth > MAX_SCRATCH_SCAN_DEPTH
                || result.len() > MAX_SCRATCH_SCAN_ENTRIES
                || start.elapsed() > SCRATCH_SCAN_BUDGET
            {
                return Err(denied("scratch ACL scan exceeded its bound"));
            }
            let (handle, info) = open_scratch_entry(
                path,
                FILE_READ_ATTRIBUTES | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
            )?;
            let directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            let (handle, info) = if directory {
                let (pin, pinned) = open_scratch_entry(
                    path,
                    FILE_READ_ATTRIBUTES | READ_CONTROL | FILE_LIST_DIRECTORY,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                )?;
                if !same_identity(&info, &pinned) {
                    return Err(denied("scratch directory changed identity before pinning"));
                }
                (pin, pinned)
            } else {
                (handle, info)
            };
            if !directory && info.nNumberOfLinks != 1 {
                return Err(denied("scratch contains a multiply-linked file"));
            }
            verify_acl(handle.0, &value.owner, active.then_some(value.app), active)?;
            result.push(Entry {
                path: path.to_owned(),
                handle,
                info,
            });
            if directory {
                for entry in std::fs::read_dir(path)? {
                    match visit(value, &entry?.path(), depth + 1, active, result, start) {
                        Err(error) if active && error.kind() == std::io::ErrorKind::NotFound => {}
                        other => other?,
                    }
                }
            }
            Ok(())
        }
        let mut entries = Vec::new();
        visit(self, &self.root, 0, active, &mut entries, Instant::now())?;
        for original in &self.initial {
            let Some(current) = entries.iter().find(|entry| entry.path == original.path) else {
                return Err(denied("staged scratch entry disappeared"));
            };
            if !same_identity(&original.info, &current.info) {
                return Err(denied("staged scratch entry changed identity"));
            }
            if active {
                // Pre-staged objects must retain their protected DACL; only
                // newly generated descendants may still carry inherited ACEs.
                verify_acl(current.handle.0, &self.owner, Some(self.app), false)?;
            }
        }
        Ok(entries)
    }

    pub(super) fn verify_active(&self) -> std::io::Result<()> {
        self.collect(true).map(|_| ())
    }

    pub(super) fn retire(&mut self) -> std::io::Result<()> {
        if self.retired {
            return Ok(());
        }
        if !self.stopped {
            return Err(denied(
                "scratch recovery required: renderer exit is unconfirmed",
            ));
        }
        let entries = self.collect(true)?;
        // Every entry is pinned while all grants are retired; reverse order
        // prevents parent ACL changes from altering unprocessed descendants.
        for entry in entries.iter().rev() {
            let writer = entry.acl_writer()?;
            verify_acl(writer.0, &self.owner, Some(self.app), true)?;
            set_acl(writer.0, &self.owner, None, false)?;
        }
        self.retired = true;
        Ok(())
    }
}

impl Drop for ScratchGrant {
    fn drop(&mut self) {
        // Explicit retirement surfaces all errors. Drop is only a best-effort
        // rollback before launch or after proven exit; never repair live state.
        if self.stopped && !self.retired {
            let _ = self.retire();
        }
    }
}
