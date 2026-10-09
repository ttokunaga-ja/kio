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

#[cfg(target_os = "linux")]
#[path = "linux/cache.rs"]
mod linux_cache;
#[cfg(target_os = "macos")]
#[path = "macos/cache.rs"]
mod macos_cache;
#[cfg(unix)]
#[path = "unix/cache.rs"]
mod unix_cache;
#[cfg(windows)]
#[path = "windows/cache.rs"]
mod windows_cache;
#[cfg(target_os = "linux")]
pub(crate) use linux_cache::open_bound_cache_connection;
#[cfg(target_os = "linux")]
use linux_cache::{
    bind_cache_parent, bound_cache_x_open, cache_internal_name_allowed, open_or_create_cache_parent,
};
#[cfg(target_os = "macos")]
pub(crate) use macos_cache::open_bound_cache_connection;
#[cfg(target_os = "macos")]
use macos_cache::{bound_cache_x_open, cache_internal_name_allowed};
#[cfg(unix)]
pub(crate) use unix_cache::{BoundCache, bind_cache, recreate_cache, validate_bound_cache_file};
#[cfg(target_os = "macos")]
use unix_cache::{
    bind_cache_parent_unchecked as bind_cache_parent,
    open_or_create_ambient_cache_parent as open_or_create_cache_parent,
};
#[cfg(windows)]
pub(crate) use windows_cache::{
    BoundCache, bind_cache, open_bound_cache_connection, recreate_cache, validate_bound_cache_file,
};
