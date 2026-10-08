#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
pub(crate) use macos::private_acl;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::{private_fs, profile::windows_profile_dir, store};
#[cfg(target_os = "windows")]
pub(crate) use windows::{private_fs, profile::windows_profile_dir, store};
