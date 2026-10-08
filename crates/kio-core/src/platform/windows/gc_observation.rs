use crate::error::Result;
use crate::gc::{FileState, corrupt, id_meta};
use cap_primitives::fs as cap_fs;

pub(crate) fn canonical_gc_index_identity_from_metadata(
    metadata: &cap_fs::Metadata,
) -> Result<String> {
    use cap_fs::_WindowsByHandle;
    let volume = metadata
        .volume_serial_number()
        .ok_or_else(|| corrupt("GC source index has no Windows volume identity"))?;
    let index = metadata
        .file_index()
        .ok_or_else(|| corrupt("GC source index has no Windows file identity"))?;
    Ok(format!("windows:{volume:08x}:{index:016x}"))
}

pub(crate) fn same_file_state(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> Result<bool> {
    Ok(id_meta(left)? == id_meta(right)?
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok())
}

pub(crate) fn file_state(metadata: &cap_fs::Metadata) -> FileState {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.into_std().duration_since(std::time::UNIX_EPOCH).ok());
    FileState {
        len: metadata.len(),
        modified_seconds: modified
            .as_ref()
            .and_then(|duration| i64::try_from(duration.as_secs()).ok())
            .unwrap_or(-1),
        modified_nanos: modified
            .as_ref()
            .map_or(-1, |duration| i64::from(duration.subsec_nanos())),
        changed_seconds: -1,
        changed_nanos: -1,
    }
}
