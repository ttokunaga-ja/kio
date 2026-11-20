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

#[cfg(target_os = "macos")]
#[path = "macos/watch_service.rs"]
mod watch_service;
#[cfg(target_os = "linux")]
#[path = "linux/watch_service.rs"]
mod watch_service;
#[cfg(windows)]
#[path = "windows/watch_service.rs"]
mod watch_service;
pub(crate) use watch_service::{
    native_after_remove, native_definition_path, native_enable, native_loaded_definition_matches,
    native_manifest_anchor, native_manifest_locator, native_prepare_replacement,
    native_registration_absent, native_start, native_state, native_stop, native_uninstall,
    render_definition,
};

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[path = "unix/watch_service.rs"]
mod watch_runner;
#[cfg(windows)]
#[path = "windows/watch_runner.rs"]
mod watch_runner;
pub(crate) use watch_runner::run_watch_service_runner;

#[cfg(windows)]
#[path = "windows/watch_task.rs"]
mod watch_task;

#[cfg(any(windows, test))]
#[path = "../watch_service/runner.rs"]
mod watch_runner_model;
#[cfg(any(windows, test))]
pub(crate) use watch_runner_model::{runner_argv, validate_runner_manifest_locator};

#[cfg(any(windows, test))]
#[path = "../watch_service/task_definition.rs"]
pub(crate) mod watch_task_definition;
#[cfg(any(windows, test))]
#[path = "../watch_service/task_registration.rs"]
mod watch_task_registration;

#[cfg(any(target_os = "linux", test))]
#[path = "../watch_service/systemd_unit.rs"]
pub(crate) mod watch_systemd_unit;

#[cfg(any(target_os = "macos", all(test, unix)))]
#[path = "../watch_service/launch_agent.rs"]
mod watch_launch_agent;

#[cfg(any(windows, target_os = "macos"))]
#[path = "../watch_service/watch_argv.rs"]
mod watch_service_argv;
#[cfg(any(windows, target_os = "macos"))]
pub(crate) use watch_service_argv::watch_argv;
