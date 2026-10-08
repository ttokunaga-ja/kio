use crate::error::Result;
use crate::gc::{FileState, id_meta};
use cap_primitives::fs as cap_fs;

pub(crate) fn canonical_gc_index_identity_from_metadata(
    metadata: &cap_fs::Metadata,
) -> Result<String> {
    use cap_primitives::fs::MetadataExt;
    Ok(format!(
        "unix:{:016x}:{:016x}",
        metadata.dev(),
        metadata.ino()
    ))
}

pub(crate) fn same_file_state(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> Result<bool> {
    Ok(id_meta(left)? == id_meta(right)? && file_state(left) == file_state(right))
}

pub(crate) fn file_state(metadata: &cap_fs::Metadata) -> FileState {
    use cap_primitives::fs::MetadataExt;
    FileState {
        len: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanos: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    }
}
