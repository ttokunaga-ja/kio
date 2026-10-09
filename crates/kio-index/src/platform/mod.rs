#[cfg(target_os = "linux")]
#[path = "linux/registry.rs"]
mod registry_linux;
#[cfg(target_os = "macos")]
#[path = "macos/registry.rs"]
mod registry_macos;
#[cfg(windows)]
#[path = "windows/registry_security.rs"]
mod registry_security;
#[cfg(unix)]
#[path = "unix/registry.rs"]
mod registry_unix;
#[cfg(windows)]
#[path = "windows/registry.rs"]
mod registry_windows;
#[cfg(target_os = "linux")]
pub(crate) use registry_linux::{
    RegistryParentBinding, fresh_registry_parent_binding, prepare_registry_writer,
    stable_registry_parent_binding,
};
#[cfg(all(test, target_os = "macos"))]
pub(crate) use registry_macos::normalized_absolute_registry_path;
#[cfg(target_os = "macos")]
pub(crate) use registry_macos::{
    RegistryParentBinding, fresh_registry_parent_binding, prepare_registry_writer,
    stable_registry_parent_binding,
};
#[cfg(unix)]
pub(crate) use registry_unix::{
    FileIdentity, PrivateSnapshotStorage, leaf_identity, observe_leaf, validate_regular,
};
#[cfg(all(test, windows))]
pub(crate) use registry_windows::registry_filesystem_root;
#[cfg(windows)]
pub(crate) use registry_windows::{
    FileIdentity, PrivateSnapshotStorage, leaf_identity, observe_leaf, validate_regular,
};
#[cfg(windows)]
pub(crate) use registry_windows::{
    RegistryParentBinding, fresh_registry_parent_binding, prepare_registry_writer,
    stable_registry_parent_binding,
};
#[cfg(all(test, windows))]
pub(crate) use registry_windows::{normalized_absolute_registry_path, registry_parent_and_leaf};

#[cfg(all(test, target_os = "linux"))]
pub(crate) use registry_linux::normalized_absolute_registry_path;
