use std::ffi::OsString;
use std::path::PathBuf;

#[cfg(debug_assertions)]
use crate::xdg::home_dir_from;

pub(crate) fn profile_dir(_control: &crate::control::ControlHandle) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::ptr::{NonNull, null_mut};

    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Profile, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    #[cfg(debug_assertions)]
    if let Some(profile) = _control
        .debug()
        .core
        .windows_profile
        .as_ref()
        .and_then(|profile| home_dir_from(Some(profile.clone().into_os_string())))
    {
        return Some(profile);
    }

    let mut raw = null_mut();
    let status = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_Profile,
            KF_FLAG_DEFAULT as u32,
            null_mut(),
            &mut raw,
        )
    };
    let raw = NonNull::new(raw)?;
    if status < 0 {
        unsafe { CoTaskMemFree(raw.as_ptr().cast()) };
        return None;
    }
    let mut length = 0usize;
    unsafe {
        while *raw.as_ptr().add(length) != 0 {
            length = length.checked_add(1)?;
        }
    }
    let value = unsafe { OsString::from_wide(std::slice::from_raw_parts(raw.as_ptr(), length)) };
    unsafe { CoTaskMemFree(raw.as_ptr().cast()) };
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_known_profile_is_an_absolute_fallback() {
        let profile = profile_dir(&crate::control::ControlHandle::default())
            .expect("Windows profile known folder");
        assert!(profile.is_absolute());
    }
}
