use crate::Result;
use crate::platform::store as platform;
use crate::store_dir::{err, ioerr};
use cap_primitives::fs as cap_fs;
use std::{ffi::OsStr, fs::File, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Identity {
    File(crate::platform::RegularFileHandleIdentity),
    Directory(crate::platform::DirectoryHandleIdentity),
}

pub(crate) fn identity(file: &File, directory: bool, label: &Path) -> Result<Identity> {
    {
        if directory {
            crate::platform::directory_handle_identity(file).map(Identity::Directory)
        } else {
            crate::platform::regular_file_handle_identity(file).map(Identity::File)
        }
        .ok_or_else(|| {
            err(
                label,
                "planned removal target is reparse, hardlinked, or the wrong kind",
            )
        })
    }
}

pub(crate) fn require_same_filesystem(parent: &File, child: &File, label: &Path) -> Result<()> {
    let same = platform::volume_serial(parent, label)? == platform::volume_serial(child, label)?;
    if !same {
        return Err(err(label, "planned removal crosses a filesystem boundary"));
    }
    Ok(())
}

pub(crate) fn open(
    parent: &File,
    leaf: &OsStr,
    directory: bool,
    mutation: bool,
    label: &Path,
) -> Result<File> {
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    {
        use cap_fs::OpenOptionsExt;
        use windows_sys::Win32::{
            Foundation::{GENERIC_READ, GENERIC_WRITE},
            Storage::FileSystem::{
                DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            },
        };
        // Capture pins permit the later exact DELETE mutator; that mutator
        // denies further writers/deleters and is compared to the pinned identity.
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
        if mutation {
            options
                .access_mode(GENERIC_READ | DELETE | if directory { 0 } else { GENERIC_WRITE })
                .share_mode(FILE_SHARE_READ | if directory { FILE_SHARE_WRITE } else { 0 });
        }
        options.custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT
                | if directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
        );
    }
    let file = cap_fs::open(parent, Path::new(leaf), &options).map_err(|e| ioerr(label, e))?;
    identity(&file, directory, label)?;
    require_same_filesystem(parent, &file, label)?;
    Ok(file)
}

pub(crate) fn retire_leaf(
    parent: &File,
    leaf: &OsStr,
    held: &File,
    mutator: &File,
    directory: bool,
    expected: &Identity,
    label: &Path,
) -> Result<()> {
    if directory {
        crate::platform::planned_removal::retire_directory(mutator, label)?;
    } else {
        platform::retire_gc_handle_retained(mutator, label)?;
    }
    let _ = (parent, leaf, held, expected);
    Ok(())
}
