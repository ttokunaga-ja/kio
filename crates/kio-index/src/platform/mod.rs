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
use linux_cache::{bind_cache_parent, open_or_create_cache_parent};
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

#[cfg(unix)]
#[path = "unix/source.rs"]
mod unix_source;
#[cfg(windows)]
#[path = "windows/source.rs"]
mod windows_source;
#[cfg(unix)]
use unix_source as source;
#[cfg(windows)]
use windows_source as source;

#[cfg(target_os = "linux")]
#[path = "linux/source_vfs.rs"]
mod linux_source_vfs;
#[cfg(target_os = "macos")]
#[path = "macos/source_vfs.rs"]
mod macos_source_vfs;
#[cfg(target_os = "macos")]
#[path = "unix/source_vfs.rs"]
mod unix_source_vfs;
#[cfg(windows)]
#[path = "windows/source_vfs.rs"]
mod windows_source_vfs;
#[cfg(target_os = "linux")]
use linux_source_vfs as source_vfs;
#[cfg(target_os = "macos")]
use macos_source_vfs as source_vfs;
#[cfg(windows)]
pub(crate) use source_vfs::open as open_bound_source_connection;
#[cfg(unix)]
pub(crate) use source_vfs::open_bound_source_connection;
#[cfg(windows)]
use windows_source_vfs as source_vfs;

#[cfg(unix)]
#[path = "unix/source_gc.rs"]
mod unix_source_gc;
#[cfg(windows)]
#[path = "windows/source_gc.rs"]
mod windows_source_gc;
#[cfg(unix)]
pub(crate) use unix_source_gc::{exchange_prepared_bound_gc_index, remove_prepared_bound_gc_index};
#[cfg(windows)]
pub use windows_source_gc::recover_prepared_bound_gc_index;
#[cfg(windows)]
pub(crate) use windows_source_gc::{
    exchange_prepared_bound_gc_index, remove_prepared_bound_gc_index,
};

#[cfg(unix)]
pub(crate) use unix_source::{
    BoundSourceIndex, SourceFileIdentity, bind_gc_source_leaf, bind_source_index,
    canonical_gc_index_identity, cap_source_leaf_identity, gc_cleanup_directory,
    open_gc_source_copy, remove_stale_gc_index_file, source_file_identity, source_file_state,
    sync_bound_gc_directory, validate_bound_source_file,
};
#[cfg(windows)]
pub(crate) use windows_source::{
    BoundSourceIndex, SourceFileIdentity, bind_gc_source_leaf, bind_source_index,
    canonical_gc_index_identity, cap_source_leaf_identity, gc_cleanup_directory,
    open_gc_source_copy, remove_stale_gc_index_file, source_file_identity, source_file_state,
    sync_bound_gc_directory, validate_bound_source_file,
};

#[path = "../bound_sqlite.rs"]
mod bound_sqlite;
pub(crate) use bound_sqlite::BoundSqliteConnection;

#[cfg(target_os = "linux")]
#[path = "linux/bound_sqlite.rs"]
mod linux_bound_sqlite;

#[path = "../registry_vfs.rs"]
mod registry_vfs;
pub(crate) use registry_vfs::{
    open as open_private_registry_connection, open_snapshot as open_private_registry_snapshot,
};

#[cfg(target_os = "linux")]
#[path = "linux/registry_vfs.rs"]
mod registry_native;
#[cfg(target_os = "macos")]
#[path = "macos/registry_vfs.rs"]
mod registry_native;
#[cfg(windows)]
#[path = "windows/registry_vfs.rs"]
mod registry_native;
#[cfg(unix)]
#[path = "unix/registry_vfs.rs"]
mod registry_unix_native;
