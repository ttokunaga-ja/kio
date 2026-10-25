//! Set the suspended invocation's default object owner, never the host token.
use super::{OwnedHandle, PSID};
use std::{io, mem, ptr};
use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, HANDLE},
    Security::{
        GetLengthSid, GetTokenInformation, IsValidSid, SetTokenInformation, TOKEN_ADJUST_DEFAULT,
        TOKEN_APPCONTAINER_INFORMATION, TOKEN_INFORMATION_CLASS, TOKEN_OWNER, TOKEN_QUERY,
        TOKEN_STATISTICS, TOKEN_USER, TokenAppContainerSid, TokenCapabilities, TokenIsAppContainer,
        TokenOwner, TokenPrimary, TokenStatistics, TokenUser,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

fn unsafe_token(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
struct Information(Vec<usize>, usize);
impl Information {
    fn query(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Self> {
        let mut size = 0;
        let ok = unsafe { GetTokenInformation(token, class, ptr::null_mut(), 0, &mut size) };
        if ok != 0
            || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
            || size == 0
            || size > 64 * 1024
        {
            return Err(unsafe_token("invalid renderer token information length"));
        }
        let mut data = vec![0_usize; (size as usize).div_ceil(mem::size_of::<usize>())];
        let capacity = size;
        if unsafe {
            GetTokenInformation(token, class, data.as_mut_ptr().cast(), capacity, &mut size)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if size > capacity {
            return Err(unsafe_token("renderer token information grew unexpectedly"));
        }
        Ok(Self(data, size as usize))
    }
    fn value<T>(&self) -> io::Result<&T> {
        if self.1 < mem::size_of::<T>() {
            return Err(unsafe_token("truncated renderer token information"));
        }
        Ok(unsafe { &*self.0.as_ptr().cast::<T>() })
    }
    fn sid(&self, sid: PSID) -> io::Result<Vec<u8>> {
        let start = self.0.as_ptr() as usize;
        let end = start
            .checked_add(self.1)
            .ok_or_else(|| unsafe_token("invalid token buffer bound"))?;
        let address = sid as usize;
        if address < start || address.checked_add(8).is_none_or(|value| value > end) {
            return Err(unsafe_token(
                "renderer token SID is outside its information buffer",
            ));
        }
        let length = 8 + 4 * usize::from(unsafe { *sid.cast::<u8>().add(1) });
        if address.checked_add(length).is_none_or(|value| value > end)
            || unsafe { IsValidSid(sid) } == 0
            || unsafe { GetLengthSid(sid) } as usize != length
        {
            return Err(unsafe_token("invalid renderer token SID"));
        }
        Ok(unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), length) }.to_vec())
    }
}
fn user(token: HANDLE) -> io::Result<Vec<u8>> {
    let info = Information::query(token, TokenUser)?;
    info.sid(info.value::<TOKEN_USER>()?.User.Sid)
}
fn owner(token: HANDLE) -> io::Result<Vec<u8>> {
    let info = Information::query(token, TokenOwner)?;
    info.sid(info.value::<TOKEN_OWNER>()?.Owner)
}
fn token_id(token: HANDLE) -> io::Result<(u32, i32)> {
    let info = Information::query(token, TokenStatistics)?;
    let stats = info.value::<TOKEN_STATISTICS>()?;
    if stats.TokenType != TokenPrimary {
        return Err(unsafe_token("renderer token is not primary"));
    }
    Ok((stats.TokenId.LowPart, stats.TokenId.HighPart))
}
fn isolated(token: HANDLE, user_sid: &[u8], expected_app: PSID) -> io::Result<()> {
    if user(token)? != user_sid
        || *Information::query(token, TokenIsAppContainer)?.value::<u32>()? != 1
    {
        return Err(unsafe_token(
            "renderer token is not the invoking user's AppContainer",
        ));
    }
    let app = Information::query(token, TokenAppContainerSid)?;
    let actual = app.sid(
        app.value::<TOKEN_APPCONTAINER_INFORMATION>()?
            .TokenAppContainer,
    )?;
    if expected_app.is_null() || unsafe { IsValidSid(expected_app) } == 0 {
        return Err(unsafe_token("invalid expected invocation SID"));
    }
    let expected = unsafe {
        std::slice::from_raw_parts(
            expected_app.cast::<u8>(),
            GetLengthSid(expected_app) as usize,
        )
    };
    if actual != expected || *Information::query(token, TokenCapabilities)?.value::<u32>()? != 0 {
        return Err(unsafe_token(
            "renderer token does not have the exact zero-capability invocation SID",
        ));
    }
    Ok(())
}

pub(super) fn prepare_suspended_renderer_token(process: HANDLE, app_sid: PSID) -> io::Result<()> {
    let mut parent = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut parent) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let parent = OwnedHandle(parent);
    let parent_id = token_id(parent.0)?;
    let parent_owner = owner(parent.0)?;
    let parent_user = user(parent.0)?;
    let mut child = ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY | TOKEN_ADJUST_DEFAULT, &mut child) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let child = OwnedHandle(child);
    let child_id = token_id(child.0)?;
    if child_id == parent_id {
        return Err(unsafe_token(
            "renderer unexpectedly shares the host token object",
        ));
    }
    isolated(child.0, &parent_user, app_sid)?;
    let child_user = Information::query(child.0, TokenUser)?;
    let desired = TOKEN_OWNER {
        Owner: child_user.value::<TOKEN_USER>()?.User.Sid,
    };
    if unsafe {
        SetTokenInformation(
            child.0,
            TokenOwner,
            (&desired as *const TOKEN_OWNER).cast(),
            mem::size_of::<TOKEN_OWNER>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if owner(child.0)? != parent_user
        || token_id(child.0)? != child_id
        || token_id(parent.0)? != parent_id
        || owner(parent.0)? != parent_owner
        || user(parent.0)? != parent_user
    {
        return Err(unsafe_token(
            "renderer owner adjustment changed unexpected token state",
        ));
    }
    isolated(child.0, &parent_user, app_sid)
}
