use crate::error::Result;
use crate::gc::{Identity, corrupt, snapshot_auto_unsafe_entry};
use cap_primitives::fs as cap_fs;
pub(crate) fn gc_metadata_identity(m: &cap_fs::Metadata) -> Result<Identity> {
    use cap_fs::_WindowsByHandle;
    let volume = m
        .volume_serial_number()
        .ok_or_else(|| corrupt("store identity is unavailable"))?;
    let index = m
        .file_index()
        .ok_or_else(|| corrupt("store identity is unavailable"))?;
    Ok(Identity(u64::from(volume), index))
}

pub(crate) fn is_canonical_gc_index_identity(value: &str) -> bool {
    let Some((platform, volume, index)) = value.split_once(':').and_then(|(platform, rest)| {
        rest.split_once(':')
            .map(|(volume, index)| (platform, volume, index))
    }) else {
        return false;
    };
    platform == "windows"
        && volume.len() == 8
        && index.len() == 16
        && volume
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && index
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(crate) fn validate_gc_file_links(m: &cap_fs::Metadata) -> Result<()> {
    use cap_fs::_WindowsByHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || m.number_of_links() != Some(1) {
        return Err(corrupt("linked store entry"));
    }
    Ok(())
}
pub(crate) fn validate_gc_directory_metadata(metadata: &cap_fs::Metadata) -> Result<()> {
    use cap_fs::_WindowsByHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(corrupt("directory is a Windows reparse point"));
    }
    Ok(())
}
pub(crate) fn validate_gc_direct_directory_metadata(metadata: &cap_fs::Metadata) -> Result<()> {
    use cap_fs::_WindowsByHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(snapshot_auto_unsafe_entry(
            "direct directory is a reparse point",
        ));
    }
    Ok(())
}
