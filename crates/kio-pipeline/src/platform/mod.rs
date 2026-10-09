#[cfg(target_os = "linux")]
#[path = "linux/unsupported.rs"]
mod linux_unsupported;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "unix/scan.rs"]
pub(crate) mod scan;
#[cfg(windows)]
#[path = "windows/scan.rs"]
pub(crate) mod scan;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "unix/task.rs"]
pub(crate) mod task;
#[cfg(windows)]
#[path = "windows/task.rs"]
pub(crate) mod task;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "unix/unsupported.rs"]
pub(crate) mod unsupported;
#[cfg(windows)]
#[path = "windows/unsupported.rs"]
pub(crate) mod unsupported;
#[cfg(target_os = "linux")]
pub(crate) use linux_unsupported::configure_no_follow;
#[cfg(target_os = "macos")]
#[path = "macos/unsupported.rs"]
mod macos_unsupported;
#[cfg(target_os = "macos")]
pub(crate) use macos_unsupported::configure_no_follow;
#[cfg(windows)]
pub(crate) use unsupported::configure_no_follow;
#[cfg(any(windows, test))]
#[path = "windows/windows_file.rs"]
pub(crate) mod windows_file;
#[cfg(windows)]
#[path = "windows/windows_file_io.rs"]
mod windows_file_io;
#[cfg(windows)]
pub(crate) use windows_file_io::{information, open_path_no_follow};

#[cfg(target_os = "linux")]
#[path = "linux/ledger_lifecycle.rs"]
pub(crate) mod ledger_lifecycle;
#[cfg(target_os = "macos")]
#[path = "macos/ledger_lifecycle.rs"]
pub(crate) mod ledger_lifecycle;
#[cfg(windows)]
#[path = "windows/ledger_lifecycle.rs"]
pub(crate) mod ledger_lifecycle;
#[cfg(windows)]
#[path = "windows/ledger_snapshot_security.rs"]
pub(crate) mod snapshot_windows_security;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "unix/ledger_lifecycle.rs"]
mod unix_ledger_lifecycle;

#[cfg(target_os = "linux")]
#[path = "linux/ledger_snapshot.rs"]
pub(crate) mod ledger_snapshot;
#[cfg(target_os = "macos")]
#[path = "macos/ledger_snapshot.rs"]
pub(crate) mod ledger_snapshot;
#[cfg(windows)]
#[path = "windows/ledger_snapshot.rs"]
pub(crate) mod ledger_snapshot;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "unix/ledger_snapshot.rs"]
mod unix_ledger_snapshot;
