use crate::error::{KioError, Result};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::Arc;

pub(crate) fn retain_bound_kio(retained: File) -> Result<Arc<File>> {
    super::store::validate_directory(&retained, Path::new(".kio"))?;
    Ok(Arc::new(super::store::normalize_directory_handle(
        retained,
        Path::new(".kio"),
    )?))
}
pub(crate) struct RepairSource<'a> {
    source: File,
    _borrow: std::marker::PhantomData<&'a mut Option<File>>,
}
impl RepairSource<'_> {
    pub(crate) fn file(&self) -> &File {
        &self.source
    }
    pub(crate) fn prepare_replace(self) {
        drop(self.source);
    }
}
pub(crate) fn retain_repair_source(source: &mut Option<File>) -> RepairSource<'_> {
    RepairSource {
        source: source
            .take()
            .expect("repair source is retained before closure capture"),
        _borrow: std::marker::PhantomData,
    }
}
pub(crate) fn prepare_bound_raw_stage(source: &mut Option<File>) {
    drop(source.take());
}
pub(crate) fn validate_bound_directory_native(directory: &File, label: &Path) -> Result<()> {
    if super::cas_identity::directory_handle_identity(directory).is_none() {
        return Err(KioError::io(
            "bound CAS directory is a reparse point or changed type",
            label.display().to_string(),
        ));
    }
    Ok(())
}
pub(crate) fn same_bound_directory_native(current: &File, retained: &File) -> Result<bool> {
    Ok(super::cas_identity::directory_handle_identity(current)
        == super::cas_identity::directory_handle_identity(retained))
}
pub(crate) fn normalize_bound_child(directory: File, leaf: &str) -> Result<File> {
    super::store::validate_directory(&directory, Path::new(leaf))?;
    let retained = super::store::normalize_directory_handle(directory, Path::new(leaf))?;
    retained
        .try_clone()
        .map_err(|error| KioError::io(error.to_string(), leaf))
}
pub(crate) fn configure_bound_read(_options: &mut cap_primitives::fs::OpenOptions) {}
pub(crate) fn same_repair_quarantine(
    source: &File,
    quarantined: &File,
    _label: &Path,
) -> Result<bool> {
    Ok(
        super::identity_logic::same_windows_repair_quarantine_file_components(
            super::cas_identity::windows_file_information(source),
            super::cas_identity::windows_file_information(quarantined),
        ),
    )
}
pub(crate) fn private_bound_temp_is_safe(file: &File, _label: &str) -> Result<bool> {
    Ok(super::cas_identity::regular_file_handle_identity(file).is_some())
}
pub(crate) fn configure_removal_root(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
}
pub(crate) fn configure_private_temp(_options: &mut OpenOptions) {}
