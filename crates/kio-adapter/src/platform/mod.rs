#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
#[path = "linux/package.rs"]
mod linux_package;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "linux")]
pub(crate) use linux::office::{OfficeRuntime, canonical_renderer_program, private_temp_dir};
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) use linux_package::{
    linux_tdf_bundle_candidate, linux_tdf_runtime_roots, trusted_linux_package_mode,
    verify_linux_package_path,
};
#[cfg(target_os = "macos")]
pub(crate) use macos::office::{OfficeRuntime, canonical_renderer_program, private_temp_dir};
#[cfg(all(test, target_os = "macos"))]
pub(crate) use macos::office::{
    PrivateUnoCatalog, is_macos_office_passthrough_wrapper, macos_office_bundle_candidate,
    macos_spell_component_range, office_bundle_path_snapshot, private_uno_catalog_for_program,
    renderer_environment, renderer_runtime_roots, validate_macos_office_version,
    verify_macos_office_bundle, write_private_uno_catalog,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::office_fs::{file_url, restrict_renderer_state_directory};
#[cfg(all(test, target_os = "windows"))]
pub(crate) use windows::office::renderer_environment;
#[cfg(target_os = "windows")]
pub(crate) use windows::office::{
    OfficeRuntime, canonical_renderer_program, file_url, private_temp_dir,
    restrict_renderer_state_directory,
};
