use crate::Result;
use crate::store_dir::{err, ioerr};
use cap_primitives::fs as cap_fs;
use std::{ffi::OsStr, fs::File, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity(u64, u64);

pub(crate) fn identity(file: &File, directory: bool, label: &Path) -> Result<Identity> {
    {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata().map_err(|e| ioerr(label, e))?;
        if (directory && !m.is_dir()) || (!directory && (!m.is_file() || m.nlink() != 1)) {
            return Err(err(
                label,
                "planned removal target is not a safe single-link file or directory",
            ));
        }
        Ok(Identity(m.dev(), m.ino()))
    }
}

pub(crate) fn require_same_filesystem(parent: &File, child: &File, label: &Path) -> Result<()> {
    let same = {
        use std::os::unix::fs::MetadataExt;
        parent.metadata().map_err(|e| ioerr(label, e))?.dev()
            == child.metadata().map_err(|e| ioerr(label, e))?.dev()
    };
    if !same {
        return Err(err(label, "planned removal crosses a filesystem boundary"));
    }
    #[cfg(target_os = "linux")]
    if crate::platform::mount_id(parent, label)? != crate::platform::mount_id(child, label)? {
        return Err(err(label, "planned removal crosses a mount boundary"));
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
    if directory && !mutation {
        let file =
            cap_fs::open_dir_nofollow(parent, Path::new(leaf)).map_err(|e| ioerr(label, e))?;
        identity(&file, true, label)?;
        require_same_filesystem(parent, &file, label)?;
        return Ok(file);
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    {
        use cap_fs::OpenOptionsExt;
        let mut flags = libc::O_NONBLOCK;
        if directory {
            flags |= libc::O_DIRECTORY;
        }
        options.custom_flags(flags);
        let _ = mutation;
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
    // A retained root quarantine is the Unix reserved namespace boundary; no
    // final unlink claims a portable atomic compare-and-delete syscall.
    let named = open(parent, leaf, directory, false, label)?;
    if identity(&named, directory, label)? != *expected
        || identity(held, directory, label)? != *expected
    {
        return Err(err(label, "planned child changed before unlink"));
    }
    if directory {
        cap_fs::remove_dir(parent, Path::new(leaf))
    } else {
        cap_fs::remove_file(parent, Path::new(leaf))
    }
    .map_err(|e| ioerr(label, e))?;
    let _ = mutator;
    Ok(())
}
