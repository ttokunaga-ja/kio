use std::fs::{File, OpenOptions};
use std::path::Path;

use crate::cas::{WindowsFileInformation, same_windows_regular_file_components};

pub(crate) fn same_windows_regular_file(opened: &File, path: &File) -> bool {
    same_windows_regular_file_components(
        windows_file_information(opened),
        windows_file_information(path),
    )
}

/// Stable identity of a real, non-reparse Windows directory.
///
/// Unlike directory metadata timestamps, this remains unchanged when children
/// are created or removed.  Its fields stay private so callers can compare
/// identities without treating their representation as a persistence format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectoryHandleIdentity {
    volume_serial_number: u32,
    file_index: u64,
}

impl DirectoryHandleIdentity {
    pub(crate) const fn new(volume_serial_number: u32, file_index: u64) -> Self {
        Self {
            volume_serial_number,
            file_index,
        }
    }
}

/// Stable identity of a real, non-reparse, single-link Windows regular file.
///
/// The public representation is opaque. Core may encode its numeric components
/// only in an explicitly versioned local recovery intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegularFileHandleIdentity {
    volume_serial_number: u32,
    file_index: u64,
}

impl RegularFileHandleIdentity {
    pub(crate) const fn new(volume_serial_number: u32, file_index: u64) -> Self {
        Self {
            volume_serial_number,
            file_index,
        }
    }
}

impl RegularFileHandleIdentity {
    /// Components for the core's versioned, local atomic recovery intent.
    /// External callers still treat this identity as an opaque observation.
    pub(crate) const fn atomic_recovery_components(self) -> (u32, u64) {
        (self.volume_serial_number, self.file_index)
    }
}

pub(crate) fn windows_file_information(file: &File) -> Option<WindowsFileInformation> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` owns a valid handle for the duration of the call, and
    // `information` points to writable storage of the required Win32 layout.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return None;
    }
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Some(WindowsFileInformation::new(
        information.dwVolumeSerialNumber,
        file_index,
        information.nNumberOfLinks,
        information.dwFileAttributes,
    ))
}

/// Open a Windows directory without traversing its final reparse point and
/// verify its handle attributes. `symlink_metadata().is_symlink()` alone does
/// not cover every directory reparse-point kind (notably junctions).
pub fn real_directory_identity(path: &Path) -> std::io::Result<Option<DirectoryHandleIdentity>> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    let directory = options.open(path)?;
    Ok(windows_file_information(&directory).and_then(WindowsFileInformation::directory_identity))
}

/// Open a Windows path without following its final reparse point and return a
/// stable identity only for a real, single-link regular file.
pub fn real_regular_file_identity(
    path: &Path,
) -> std::io::Result<Option<RegularFileHandleIdentity>> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    Ok(windows_file_information(&file).and_then(WindowsFileInformation::regular_file_identity))
}

/// Return the stable identity of an already-opened Windows directory.
///
/// This is the by-handle counterpart of [`real_directory_identity`]:
/// callers that already hold the handle they intend to use must compare against
/// the handle itself, never re-open the path, or a component swapped between
/// the two opens would simply be re-observed and accepted.
#[must_use]
pub fn directory_handle_identity(file: &File) -> Option<DirectoryHandleIdentity> {
    windows_file_information(file).and_then(WindowsFileInformation::directory_identity)
}

/// Return the stable identity of an already-opened Windows regular file.
#[must_use]
pub fn regular_file_handle_identity(file: &File) -> Option<RegularFileHandleIdentity> {
    windows_file_information(file).and_then(WindowsFileInformation::regular_file_identity)
}

/// Return whether a Windows path is a real directory rather than any reparse
/// point, including junction kinds not reported by `is_symlink()`.
pub fn directory_is_real(path: &Path) -> std::io::Result<bool> {
    real_directory_identity(path).map(|identity| identity.is_some())
}

/// Open a Windows leaf without following a reparse point and require a real,
/// single-link regular file. Restore uses this before replacing a destination.
pub fn regular_file_is_safe(path: &Path) -> std::io::Result<bool> {
    real_regular_file_identity(path).map(|identity| identity.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::open_regular_nofollow;
    use std::fs;

    #[cfg(windows)]
    #[test]
    fn windows_handle_identity_distinguishes_distinct_equal_sized_files() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.bin");
        let second = dir.path().join("second.bin");
        fs::write(&first, b"same-size").unwrap();
        fs::write(&second, b"same-size").unwrap();
        let first_handle = File::open(&first).unwrap();
        let same_handle = File::open(&first).unwrap();
        let second_handle = File::open(&second).unwrap();

        let information = windows_file_information(&first_handle).unwrap();
        assert!(information.is_regular_file());
        assert!(information.is_single_link());
        assert_eq!(
            real_regular_file_identity(&first).unwrap(),
            regular_file_handle_identity(&first_handle)
        );
        assert_ne!(
            real_regular_file_identity(&first).unwrap(),
            real_regular_file_identity(&second).unwrap()
        );
        assert!(same_windows_regular_file(&first_handle, &same_handle));
        assert!(!same_windows_regular_file(&first_handle, &second_handle));
        assert!(open_regular_nofollow(&first).is_ok());
    }

    /// The property restore's PA18 destination binding rests on: an identity
    /// captured from a path must still match a handle later opened on that same
    /// directory, and must NOT match once a different real directory has taken
    /// over the name.
    #[cfg(windows)]
    #[test]
    fn windows_directory_handle_identity_matches_its_path_and_detects_a_swap() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        fn open_directory(path: &Path) -> File {
            let mut options = OpenOptions::new();
            options
                .read(true)
                .access_mode(FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
            options.open(path).unwrap()
        }

        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("destination");
        let decoy = root.path().join("decoy");
        fs::create_dir(&destination).unwrap();
        fs::create_dir(&decoy).unwrap();

        let captured = real_directory_identity(&destination).unwrap().unwrap();
        {
            let opened = open_directory(&destination);
            assert_eq!(directory_handle_identity(&opened), Some(captured));
        }

        // The swap PA18 exists to catch: the validated name now resolves to a
        // different real directory.
        fs::rename(&destination, root.path().join("moved")).unwrap();
        fs::rename(&decoy, &destination).unwrap();
        let swapped = open_directory(&destination);
        assert_ne!(directory_handle_identity(&swapped), Some(captured));

        // A non-directory carries no directory identity, so callers comparing
        // against `Some(expected)` fail closed rather than matching.
        let regular = root.path().join("regular.bin");
        fs::write(&regular, b"payload").unwrap();
        assert_eq!(
            directory_handle_identity(&File::open(&regular).unwrap()),
            None
        );
    }
}
