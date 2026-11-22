#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "linux")]
pub(crate) use linux::office::{OfficeRuntime, private_temp_dir};
#[cfg(target_os = "macos")]
pub(crate) use macos::office::{OfficeRuntime, private_temp_dir};
#[cfg(all(test, target_os = "macos"))]
pub(crate) use macos::office::{
    PrivateUnoCatalog, macos_office_bundle_candidate, macos_spell_component_range,
    office_bundle_path_snapshot, renderer_environment, renderer_runtime_roots,
    verify_macos_office_bundle, write_private_uno_catalog,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::office_fs::{
    OFFICE_LAUNCH_PROFILE, file_url, finish_scratch, office_conversion_paths,
    office_render_sandbox, restrict_renderer_state_directory, scratch_recovery_note,
};
#[cfg(all(test, target_os = "windows"))]
pub(crate) use windows::office::renderer_environment;
#[cfg(target_os = "windows")]
pub(crate) use windows::office::{
    OFFICE_LAUNCH_PROFILE, OfficeRuntime, file_url, finish_scratch, office_conversion_paths,
    office_render_sandbox, private_temp_dir, restrict_renderer_state_directory,
    scratch_recovery_note,
};

#[cfg(any(target_os = "macos", test))]
#[path = "macos/office_macos_seal.rs"]
pub(crate) mod office_macos_seal;

#[cfg(any(windows, test))]
#[path = "windows/office_signature_worker.rs"]
pub(crate) mod office_signature_worker;

#[cfg(unix)]
pub(crate) use unix::office_fs::DEFAULT_OFFICE_CONVERTER_PROGRAM;
#[cfg(windows)]
pub(crate) use windows::office::DEFAULT_OFFICE_CONVERTER_PROGRAM;

#[cfg(all(debug_assertions, target_os = "linux"))]
pub(crate) use linux::office::office_isolation_fault;
#[cfg(all(debug_assertions, any(windows, target_os = "macos")))]
pub(crate) use signed_runtime::office_isolation_fault;
#[cfg(any(windows, target_os = "macos"))]
#[path = "../office_runtime_identity/signed_runtime.rs"]
mod signed_runtime;
#[cfg(any(test, windows, target_os = "macos"))]
#[path = "../office_runtime_identity/signed_snapshot.rs"]
mod signed_snapshot;

#[cfg(target_os = "linux")]
pub(crate) use linux::office_snapshot::runtime_link_target;
#[cfg(target_os = "macos")]
pub(crate) use macos::office_snapshot::runtime_link_target;
#[cfg(unix)]
pub(crate) use unix::office_snapshot::{EntryIdentity, entry_identity};
#[cfg(windows)]
pub(crate) use windows::office_snapshot::{EntryIdentity, entry_identity, runtime_link_target};
#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
#[path = "macos/office_links.rs"]
mod office_links;
#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
pub(crate) use office_links::macos_runtime_link_target;

#[cfg(unix)]
pub(crate) use unix::office_fs::office_package_launch_roots;
#[cfg(windows)]
pub(crate) use windows::office::office_package_launch_roots;
