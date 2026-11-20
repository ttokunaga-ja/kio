use crate::office_runtime_identity::{EntryCheck, RuntimeSnapshot, path_error};
use std::{
    fs::{self, Metadata},
    io::{self, Result},
    path::{Path, PathBuf},
    time::Instant,
};

/// Capture each traversed entry and reject a non-directory before the final component.
pub(crate) struct LinkWalk<'a> {
    start: &'a Path,
    check_entry: &'a EntryCheck<'a>,
    entries: &'a mut std::collections::BTreeMap<PathBuf, EntryIdentity>,
    hops: usize,
}

impl<'a> LinkWalk<'a> {
    pub(crate) fn new(
        start: &'a Path,
        check_entry: &'a EntryCheck<'a>,
        entries: &'a mut std::collections::BTreeMap<PathBuf, EntryIdentity>,
    ) -> Self {
        Self {
            start,
            check_entry,
            entries,
            hops: 0,
        }
    }

    pub(crate) fn observe(&mut self, path: &Path, has_more: bool) -> Result<bool> {
        let metadata = fs::symlink_metadata(path).map_err(|error| path_error(path, error))?;
        if metadata.is_symlink() {
            self.hops += 1;
            if self.hops >= 40 {
                return Err(path_error(
                    self.start,
                    io::Error::other("too many Office runtime link hops"),
                ));
            }
            (self.check_entry)(path, &metadata)?;
            self.entries
                .insert(path.to_path_buf(), entry_identity(path, &metadata)?);
            Ok(true)
        } else if !metadata.is_dir() && has_more {
            Err(path_error(
                path,
                io::Error::other("expected an Office runtime directory"),
            ))
        } else {
            Ok(false)
        }
    }
}

pub(crate) fn verify_link_target(path: &Path, physical: PathBuf) -> Result<PathBuf> {
    // Reject paths that have different targets on the host and in the
    // sandbox, for example when traversing a separate mount alias.
    let actual = fs::canonicalize(path).map_err(|error| path_error(path, error))?;
    if actual != physical {
        return Err(path_error(
            path,
            io::Error::other("runtime link resolves differently in the renderer sandbox"),
        ));
    }
    Ok(physical)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EntryIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    len: u64,
    modification: (i64, i64),
    change: (i64, i64),
}

pub(crate) fn entry_identity(_path: &Path, metadata: &Metadata) -> io::Result<EntryIdentity> {
    use std::os::unix::fs::MetadataExt;
    let kind = metadata.file_type();
    if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
        return Err(io::Error::other("unexpected Office runtime file type"));
    }
    Ok(EntryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        len: metadata.len(),
        modification: (metadata.mtime(), metadata.mtime_nsec()),
        change: (metadata.ctime(), metadata.ctime_nsec()),
    })
}

impl RuntimeSnapshot {
    pub(crate) fn pin_before_launch(&self, _: Instant) -> Result<Vec<(&Path, fs::File)>> {
        Ok(Vec::new())
    }
}
