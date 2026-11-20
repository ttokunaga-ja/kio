use crate::office_runtime_identity::{
    EntryCheck, RuntimeSnapshot, check_verification_deadline, path_error,
};
use kio_process::confinement::RenderSources;
use std::{
    collections::BTreeMap,
    fs::{self, Metadata},
    io::{self, Result},
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EntryIdentity {
    information: kio_core::platform::FileInformation,
    changed: i64,
}

pub(crate) fn entry_identity(path: &Path, metadata: &Metadata) -> io::Result<EntryIdentity> {
    use kio_core::platform::open_metadata_nofollow;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };

    let identity = windows_entry_identity(&open_metadata_nofollow(path)?)?;
    let information = identity.information;
    if information.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (information.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0) != metadata.is_dir()
        || (!metadata.is_dir() && information.number_of_links != 1)
    {
        return Err(io::Error::other(
            "Office runtime path is an alias or changed type",
        ));
    }
    Ok(identity)
}

fn windows_entry_identity(file: &fs::File) -> io::Result<EntryIdentity> {
    use kio_core::platform::{file_change_time, file_information};

    Ok(EntryIdentity {
        information: file_information(file)?,
        changed: file_change_time(file)?,
    })
}

pub(crate) fn runtime_link_target(
    path: &Path,
    _: &RenderSources,
    _: &[PathBuf],
    _: &EntryCheck<'_>,
    _: Instant,
    _: &mut BTreeMap<PathBuf, EntryIdentity>,
) -> Result<PathBuf> {
    Err(path_error(
        path,
        io::Error::other("Office runtime is a reparse point"),
    ))
}

impl RuntimeSnapshot {
    pub(crate) fn pin_before_launch(&self, deadline: Instant) -> Result<Vec<(&Path, fs::File)>> {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
        };

        // Hold the captured paths against write/delete access through the
        // signature check or the entire invocation, including child loads.
        self.entries
            .iter()
            .map(|(path, expected)| {
                check_verification_deadline(deadline)?;
                let mut options = fs::OpenOptions::new();
                options
                    .share_mode(FILE_SHARE_READ)
                    .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
                if expected.information.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    options.access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES);
                } else {
                    options.read(true);
                }
                let file = options
                    .open(path)
                    .map_err(|error| path_error(path, error))?;
                if windows_entry_identity(&file)? != *expected {
                    return Err(path_error(path, io::Error::other("Office runtime changed")));
                }
                Ok((path.as_path(), file))
            })
            .collect()
    }
}
