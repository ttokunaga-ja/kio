#[cfg(target_os = "linux")]
mod linux;
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::{
    cas_io,
    durability::sync_directory,
    gc_io::{
        open_verified_receipt_durable_handle as gc_receipt_handle,
        sync_bound_directory as gc_sync_bound_directory,
    },
    gc_observation::{
        canonical_gc_index_identity_from_metadata as gc_canonical_index_identity,
        file_state as gc_file_state, same_file_state as gc_same_file_state,
    },
    management::{
        file_identity as management_file_identity,
        same_case_probe_file as management_same_case_probe_file,
    },
    planned_removal,
};

#[cfg(target_os = "windows")]
pub(crate) use windows::{
    cas_io,
    durability::sync_directory,
    gc_io::{
        open_verified_receipt_durable_handle as gc_receipt_handle,
        sync_bound_directory as gc_sync_bound_directory,
    },
    gc_observation::{
        canonical_gc_index_identity_from_metadata as gc_canonical_index_identity,
        file_state as gc_file_state, same_file_state as gc_same_file_state,
    },
    management::{
        file_identity as management_file_identity,
        same_case_probe_file as management_same_case_probe_file,
    },
    planned_removal,
};

#[cfg(target_os = "linux")]
pub(crate) use linux::{
    gc_syscalls::{
        exchange_between as gc_exchange_between, exchange_bound as gc_exchange_bound,
        remove_reserved_leaf as gc_remove_reserved_leaf,
        rename_noreplace_between as gc_rename_noreplace_between,
        rename_noreplace_bound as gc_rename_noreplace_bound,
    },
    mount::mount_id,
};
#[cfg(target_os = "macos")]
pub(crate) use macos::gc_syscalls::{
    exchange_between as gc_exchange_between, exchange_bound as gc_exchange_bound,
    remove_reserved_leaf as gc_remove_reserved_leaf,
    rename_noreplace_between as gc_rename_noreplace_between,
    rename_noreplace_bound as gc_rename_noreplace_bound,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::{
    gc_io::{
        open_gc_tree_writable as gc_open_tree_writable,
        open_verified_file_handle as gc_open_verified_handle,
    },
    gc_retirement::{
        exchange_capture_verified as gc_exchange_capture_verified,
        exchange_capture_verified_named as gc_exchange_capture_verified_named,
        remove_sentinel_leaf as gc_remove_sentinel_leaf,
        remove_verified_leaf as gc_remove_verified_leaf,
        rename_snapshot_state_noreplace as gc_rename_snapshot_state_noreplace,
        retire_snapshot_state_temporary as gc_retire_snapshot_state_temporary,
    },
    scope_io::{
        configure_scope_no_follow as scope_configure_no_follow,
        effective_uid as scope_effective_uid,
        open_bound_scope_file_nofollow as scope_open_bound_nofollow,
        read_bound_regular_text_observed_at as scope_read_bound_regular_text_observed_at,
    },
};
#[cfg(target_os = "windows")]
pub(crate) use windows::{
    cas_identity, gc_io,
    gc_io::{
        rename_snapshot_state_noreplace as gc_rename_snapshot_state_noreplace,
        retire_snapshot_state_temporary as gc_retire_snapshot_state_temporary,
        verify_retained_receipt_observation as gc_verify_retained_receipt_observation,
    },
    gc_snapshot_io,
    scheduled::read_bound_regular_text_observed_at as scope_read_bound_regular_text_observed_at,
    scope_io::{
        configure_scope_no_follow as scope_configure_no_follow,
        open_bound_scope_file_nofollow as scope_open_bound_nofollow,
    },
};
