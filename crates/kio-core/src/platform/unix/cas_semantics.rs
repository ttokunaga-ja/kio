use crate::error::{IoResultExt, KioError, Result};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::Arc;

pub(crate) fn retain_bound_kio(retained: File) -> Result<Arc<File>> {
    Ok(Arc::new(retained))
}
pub(crate) struct RepairSource<'a>(&'a File);
impl RepairSource<'_> {
    pub(crate) fn file(&self) -> &File {
        self.0
    }
    pub(crate) fn prepare_replace(self) {}
}
pub(crate) fn retain_repair_source(source: &mut Option<File>) -> RepairSource<'_> {
    RepairSource(
        source
            .as_ref()
            .expect("repair source is retained before closure capture"),
    )
}
pub(crate) fn prepare_bound_raw_stage(_source: &mut Option<File>) {}
pub(crate) fn validate_bound_directory_native(_directory: &File, _label: &Path) -> Result<()> {
    Ok(())
}
pub(crate) fn same_bound_directory_native(current: &File, retained: &File) -> Result<bool> {
    use cap_primitives::fs::MetadataExt;
    let current = cap_primitives::fs::Metadata::from_file(current)
        .kio_io(Path::new("bound object layout"))?;
    let retained = cap_primitives::fs::Metadata::from_file(retained)
        .kio_io(Path::new("retained object layout"))?;
    Ok(current.dev() == retained.dev() && current.ino() == retained.ino())
}
pub(crate) fn normalize_bound_child(directory: File, _leaf: &str) -> Result<File> {
    Ok(directory)
}
pub(crate) fn configure_bound_read(options: &mut cap_primitives::fs::OpenOptions) {
    use cap_primitives::fs::OpenOptionsExt;
    // Reject a substituted FIFO without blocking before opened-handle validation.
    options.custom_flags(libc::O_NONBLOCK);
}
pub(crate) fn same_repair_quarantine(
    source: &File,
    quarantined: &File,
    label: &Path,
) -> Result<bool> {
    use cap_primitives::fs::{self as cap_fs, MetadataExt};
    let source = cap_fs::Metadata::from_file(source).kio_io(label)?;
    let quarantined = cap_fs::Metadata::from_file(quarantined).kio_io(label)?;
    Ok(source.is_file()
        && quarantined.is_file()
        && source.nlink() == 2
        && quarantined.nlink() == 2
        && source.dev() == quarantined.dev()
        && source.ino() == quarantined.ino())
}
pub(crate) fn private_bound_temp_is_safe(file: &File, label: &str) -> Result<bool> {
    let metadata = cap_primitives::fs::Metadata::from_file(file)
        .map_err(|error| KioError::io(error.to_string(), label))?;
    Ok(metadata.is_file() && super::cas_io::bound_metadata_single_link(&metadata))
}
pub(crate) fn configure_removal_root(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK);
}
pub(crate) fn configure_private_temp(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}
