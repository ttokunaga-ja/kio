#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub(crate) use linux::*;
#[cfg(target_os = "macos")]
pub(crate) use macos::*;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::*;
#[cfg(target_os = "windows")]
pub(crate) use windows::*;

pub(crate) use configure_private_cache_file as configure_exact_cas_temp;
