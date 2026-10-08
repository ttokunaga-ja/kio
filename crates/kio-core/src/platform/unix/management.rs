use crate::Result;
use crate::management::{DirectoryIdentity, err, io};
use cap_primitives::fs as cap_fs;
use std::fs::File;

pub(crate) fn same_case_probe_file(left: &File, right: &File) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let left = left.metadata().map_err(io)?;
    let right = right.metadata().map_err(io)?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino() && left.nlink() == 1)
}

pub(crate) fn file_identity(f: &File) -> Result<DirectoryIdentity> {
    use cap_primitives::fs::MetadataExt;
    let m = cap_fs::Metadata::from_file(f).map_err(io)?;
    if !m.is_dir() {
        return Err(err("retained handle is not a directory"));
    }
    Ok(DirectoryIdentity::Unix {
        device: m.dev(),
        inode: m.ino(),
    })
}
