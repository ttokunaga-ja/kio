use std::fs::Metadata;

#[cfg(windows)]
pub(super) fn is_link(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
}

#[cfg(not(windows))]
pub(super) fn is_link(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}
