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

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "unix/watch_native.rs"]
mod watch_native;
#[cfg(windows)]
#[path = "windows/watch_native.rs"]
mod watch_native;
#[cfg(any(windows, test))]
#[path = "windows/watch_native_model.rs"]
mod watch_native_model;
pub(crate) use watch_native::{NativeWatchBackend, watch_backend_name};

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "unix/watch_lock.rs"]
mod watch_lock;
#[cfg(windows)]
#[path = "windows/watch_lock.rs"]
mod watch_lock;
pub(crate) use watch_lock::open_watch_instance_lock;
