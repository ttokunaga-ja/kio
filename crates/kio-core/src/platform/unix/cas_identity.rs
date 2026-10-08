use std::fs::{self, File, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectoryHandleIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegularFileHandleIdentity {
    device: u64,
    inode: u64,
}

fn directory_identity(metadata: &Metadata) -> Option<DirectoryHandleIdentity> {
    metadata.is_dir().then_some(DirectoryHandleIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn regular_file_identity(metadata: &Metadata) -> Option<RegularFileHandleIdentity> {
    (metadata.is_file() && metadata.nlink() == 1).then_some(RegularFileHandleIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[must_use]
pub fn directory_handle_identity(file: &File) -> Option<DirectoryHandleIdentity> {
    file.metadata().ok().as_ref().and_then(directory_identity)
}

#[must_use]
pub fn regular_file_handle_identity(file: &File) -> Option<RegularFileHandleIdentity> {
    file.metadata()
        .ok()
        .as_ref()
        .and_then(regular_file_identity)
}

pub fn real_directory_identity(path: &Path) -> std::io::Result<Option<DirectoryHandleIdentity>> {
    fs::symlink_metadata(path).map(|metadata| directory_identity(&metadata))
}

pub fn real_regular_file_identity(
    path: &Path,
) -> std::io::Result<Option<RegularFileHandleIdentity>> {
    fs::symlink_metadata(path).map(|metadata| regular_file_identity(&metadata))
}

pub fn directory_is_real(path: &Path) -> std::io::Result<bool> {
    real_directory_identity(path).map(|identity| identity.is_some())
}

pub fn regular_file_is_safe(path: &Path) -> std::io::Result<bool> {
    real_regular_file_identity(path).map(|identity| identity.is_some())
}
