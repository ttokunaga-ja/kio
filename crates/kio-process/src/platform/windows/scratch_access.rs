//! Invocation-only scratch ACL lease. This policy must never be used for stores.
use super::*;
use std::rc::Rc;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_DELETE_PENDING};
use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, EqualSid, GetAce, GetSecurityDescriptorControl,
    GetSecurityDescriptorOwner, IsValidSid, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
};
use windows_sys::Win32::Storage::FileSystem::WRITE_OWNER;

const APP_ACCESS: u32 = FILE_ALL_ACCESS & !(WRITE_DAC | WRITE_OWNER);

#[derive(Clone, Copy)]
enum ScratchAcl {
    Private,
    Invocation(Option<PSID>),
}

impl ScratchAcl {
    fn app(self) -> Option<PSID> {
        match self {
            Self::Private => None,
            Self::Invocation(app) => app,
        }
    }
}

fn denied(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, message)
}

pub(super) fn scratch_step<T>(
    root: &Path,
    path: &Path,
    operation: &str,
    result: std::io::Result<T>,
) -> std::io::Result<T> {
    result.map_err(|error| {
        let relative = path
            .strip_prefix(root)
            .unwrap_or(Path::new("<unavailable>"));
        let name: String = relative.to_string_lossy().chars().take(240).collect();
        let name = if name.is_empty() { "." } else { &name };
        std::io::Error::new(
            error.kind(),
            format!("{operation} (scratch entry: {name:?}): {error}"),
        )
    })
}

pub(super) fn retry_live_scratch_open<T>(
    deadline: Option<Instant>,
    mut open: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let now = Instant::now();
    let deadline = deadline.unwrap_or(now).min(now + Duration::from_millis(50));
    let pause = Duration::from_millis(5);
    loop {
        // On Windows, CreateFile can report ACCESS_DENIED while a temporary
        // file is pending deletion. Reopen with identical access.
        match open() {
            Err(error)
                if error.raw_os_error().is_some_and(|code| {
                    code == ERROR_ACCESS_DENIED as i32 || code == ERROR_DELETE_PENDING as i32
                }) && Instant::now() + pause < deadline =>
            {
                std::thread::sleep(pause);
                if Instant::now() >= deadline {
                    return Err(error);
                }
            }
            result => return result,
        }
    }
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
    set_acl(handle, &owner, ScratchAcl::Private, false)
}

fn verify_acl(
    handle: HANDLE,
    owner: &CurrentUser,
    layout: ScratchAcl,
    inherited: bool,
) -> std::io::Result<()> {
    use windows_sys::Win32::Security::GetSecurityDescriptorDacl;
    let app = layout.app();
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
                || (matches!(layout, ScratchAcl::Private) && header.AceFlags != 0)
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
    layout: ScratchAcl,
    directory: bool,
) -> std::io::Result<()> {
    let app = layout.app();
    let flags = if matches!(layout, ScratchAcl::Invocation(_)) && directory {
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
    verify_acl(handle, owner, layout, false)
}

struct Entry {
    path: PathBuf,
    handle: Rc<OwnedHandle>,
    info: BY_HANDLE_FILE_INFORMATION,
}

pub(super) struct ScratchGrant {
    root: PathBuf,
    owner: CurrentUser,
    active_acl: ScratchAcl,
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
    fn reopen(&self, access: u32, share: u32) -> std::io::Result<OwnedHandle> {
        let (handle, info) =
            open_scratch_entry(&self.path, scratch_observation::METADATA | access, share)?;
        if !same_identity(&self.info, &info) {
            return Err(denied("staged scratch entry changed identity"));
        }
        Ok(handle)
    }

    fn transition(
        &self,
        grant: &ScratchGrant,
        from: ScratchAcl,
        to: ScratchAcl,
        inherited: bool,
    ) -> std::io::Result<()> {
        scratch_step(
            &grant.root,
            &self.path,
            "update scratch ACL",
            (|| {
                let writer = self.reopen(WRITE_DAC, FILE_SHARE_READ | FILE_SHARE_WRITE)?;
                verify_acl(writer.0, &grant.owner, from, inherited)?;
                set_acl(
                    writer.0,
                    &grant.owner,
                    to,
                    self.info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                )
            })(),
        )
    }
}

impl ScratchGrant {
    pub(super) fn new(root: &Path, app: Option<PSID>) -> std::io::Result<Self> {
        let owner = scratch_step(root, root, "query scratch owner", CurrentUser::new())?;
        let mut value = Self {
            root: root.to_owned(),
            owner,
            // Without an AppContainer SID, only the owner's ACE inherits.
            // This keeps renderer-created files out of the token's default DACL.
            active_acl: ScratchAcl::Invocation(app),
            initial: Vec::new(),
            stopped: true,
            retired: false,
        };
        // Validate the whole staged tree before changing any object.
        let entries = value.collect(false, None)?;
        for (granted, entry) in entries.iter().enumerate() {
            if let Err(error) =
                entry.transition(&value, ScratchAcl::Private, value.active_acl, false)
            {
                for previous in entries.iter().take(granted) {
                    previous.transition(&value, value.active_acl, ScratchAcl::Private, false)?;
                }
                return Err(error);
            }
        }
        value.initial = entries;
        if app.is_none() {
            // The old pins remain until every staged identity has a fully
            // shared, metadata-only replacement.
            value.initial = value
                .initial
                .iter()
                .map(|entry| {
                    let handle = scratch_step(
                        root,
                        &entry.path,
                        "retain shared scratch identity",
                        entry.reopen(0, scratch_observation::SHARED),
                    )?;
                    Ok(Entry {
                        path: entry.path.clone(),
                        handle: Rc::new(handle),
                        info: entry.info,
                    })
                })
                .collect::<std::io::Result<Vec<_>>>()?;
        }
        Ok(value)
    }

    fn collect(&self, active: bool, live: Option<(Instant, u64)>) -> std::io::Result<Vec<Entry>> {
        use scratch_observation::{METADATA, ScanMode};
        let budget = Instant::now() + SCRATCH_SCAN_BUDGET;
        let (root, mode, deadline, max_bytes) = if let Some((deadline, max_bytes)) = live {
            let root = self
                .initial
                .first()
                .ok_or_else(|| denied("scratch has no staged root"))?;
            (
                Rc::clone(&root.handle),
                ScanMode::LiveOffice,
                deadline.min(budget),
                max_bytes,
            )
        } else {
            let (handle, _) =
                open_scratch_entry(&self.root, METADATA, FILE_SHARE_READ | FILE_SHARE_WRITE)?;
            (
                Rc::new(handle),
                ScanMode::Acl {
                    allow_missing: active,
                },
                budget,
                u64::MAX,
            )
        };
        let layout = if active {
            self.active_acl
        } else {
            ScratchAcl::Private
        };
        let mut entries = Vec::new();
        let mut pending = self
            .initial
            .iter()
            .map(|entry| (entry.path.as_path(), &entry.info))
            .collect::<std::collections::BTreeMap<_, _>>();
        scratch_observation::observe(root, mode, deadline, max_bytes, |handle, info, relative| {
            let path = self.root.join(relative);
            let original = pending.remove(path.as_path());
            scratch_step(
                &self.root,
                &path,
                "verify scratch entry ACL and identity",
                (|| {
                    if original.is_some_and(|old| !same_identity(old, info)) {
                        return Err(denied("staged scratch entry changed identity"));
                    }
                    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
                        && info.nNumberOfLinks != 1
                    {
                        return Err(denied("scratch contains a multiply-linked file"));
                    }
                    verify_acl(handle.0, &self.owner, layout, active && original.is_none())
                })(),
            )?;
            if live.is_none() {
                entries.push(Entry {
                    path,
                    handle: Rc::clone(handle),
                    info: *info,
                });
            }
            Ok(())
        })?;
        if let Some(path) = pending.keys().next() {
            return scratch_step(
                &self.root,
                path,
                "verify staged scratch entry",
                Err(denied("staged scratch entry disappeared")),
            );
        }
        Ok(entries)
    }

    pub(super) fn verify_active(&self) -> std::io::Result<()> {
        self.collect(true, None).map(|_| ())
    }

    pub(super) fn observe_office_live(
        &self,
        deadline: Instant,
        max_bytes: u64,
    ) -> std::io::Result<()> {
        self.collect(true, Some((deadline, max_bytes))).map(|_| ())
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
        let entries = self.collect(true, None)?;
        // Retire child ACLs first to avoid altering unprocessed descendants.
        for entry in entries.iter().rev() {
            entry.transition(self, self.active_acl, ScratchAcl::Private, true)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_scratch_open_reobserves_a_delete_pending_file() {
        use windows_sys::Win32::{
            Foundation::ERROR_ACCESS_DENIED, Storage::FileSystem::DeleteFileW,
        };

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("temporary");
        let mut held = Some(File::create(&path).unwrap());
        assert_ne!(unsafe { DeleteFileW(wide(path.as_os_str()).as_ptr()) }, 0);
        let mut attempts = 0;
        let error = retry_live_scratch_open(Some(Instant::now() + Duration::from_secs(1)), || {
            attempts += 1;
            let result = open_scratch_entry(
                &path,
                FILE_READ_ATTRIBUTES | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
            );
            if attempts == 1 {
                assert_eq!(
                    result.as_ref().err().and_then(std::io::Error::raw_os_error),
                    Some(ERROR_ACCESS_DENIED as i32)
                );
                drop(held.take());
            }
            result
        })
        .err()
        .expect("temporary entry should disappear");
        assert_eq!(attempts, 2);
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn live_scratch_open_preserves_a_persistent_win32_denial() {
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;

        let error =
            retry_live_scratch_open(Some(Instant::now() + Duration::from_millis(10)), || {
                Err::<(), _>(std::io::Error::from_raw_os_error(
                    ERROR_ACCESS_DENIED as i32,
                ))
            })
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(ERROR_ACCESS_DENIED as i32));
    }
}
