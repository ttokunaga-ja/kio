#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod unix;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub(crate) use linux::clear_errno;
#[cfg(target_os = "linux")]
pub(crate) use linux::confinement::{NativeRenderSandbox, default_render_resource_limits};
#[cfg(target_os = "macos")]
pub(crate) use macos::clear_errno;
#[cfg(target_os = "macos")]
pub(crate) use macos::confinement::{NativeRenderSandbox, default_render_resource_limits};
#[cfg(target_os = "macos")]
pub(crate) use macos::monitor::run_bounded_command_with_unix_renderer_limits;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) use unix::confinement::apply_unix_limits;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) use unix::io::{StdinPump, read_stream};
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
pub(crate) use unix::process::configure_test_environment;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) use unix::process::{
    ProcessTree, child_has_exited, configure_process_isolation, terminate_process_tree,
};
#[cfg(target_os = "windows")]
pub(crate) use windows::confinement::{NativeRenderSandbox, default_render_resource_limits};
#[cfg(target_os = "windows")]
pub(crate) use windows::io::{StdinPump, read_stream};
#[cfg(all(test, target_os = "windows"))]
pub(crate) use windows::process::configure_test_environment;
#[cfg(target_os = "windows")]
pub(crate) use windows::process::{
    ProcessTree, child_has_exited, configure_process_isolation, terminate_process_tree,
};

#[cfg(target_os = "windows")]
pub use windows::confinement::protect_owner_private_scratch;
#[cfg(target_os = "windows")]
pub use windows::process::{
    replace_windows_pre_attach_delay_for_test, replace_windows_supervised_attach_failure_for_test,
    wait_supervised_child,
};

#[cfg(target_os = "linux")]
pub(crate) use linux::monitor::UnixRendererMonitor;
