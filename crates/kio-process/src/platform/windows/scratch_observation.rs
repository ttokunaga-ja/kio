//! Read-only, fully shared, handle-relative inventory for a live Office job.
use super::*;
use scratch_access::scratch_step;
use std::{io, os::windows::ffi::OsStringExt as _, rc::Rc};
use windows_sys::Wdk::{
    Foundation::OBJECT_ATTRIBUTES,
    Storage::FileSystem::{FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtOpenFile},
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, RtlNtStatusToDosError, STATUS_DELETE_PENDING,
        UNICODE_STRING,
    },
    Storage::FileSystem::SYNCHRONIZE,
    System::IO::IO_STATUS_BLOCK,
};

pub(super) const SHARED: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
pub(super) const METADATA: u32 = FILE_READ_ATTRIBUTES | READ_CONTROL;

// Empty name reopens the exact directory. Non-empty names are a single leaf
// from the kernel directory stream, never an ambient path.
fn open_at(parent: &OwnedHandle, name: &OsStr, access: u32, share: u32) -> io::Result<OwnedHandle> {
    let mut name = wide(name);
    let units = &name[..name.len() - 1];
    if units.iter().any(|unit| matches!(*unit, 0 | 47 | 58 | 92)) {
        return Err(io::Error::other("invalid scratch directory entry name"));
    }
    let size = u16::try_from(units.len().saturating_mul(2))
        .map_err(|_| io::Error::other("scratch directory entry name is too long"))?;
    let mut unicode = UNICODE_STRING {
        Length: size,
        MaximumLength: size,
        Buffer: name.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.0,
        ObjectName: &mut unicode,
        ..Default::default()
    };
    let mut handle = INVALID_HANDLE_VALUE;
    let mut status_block = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtOpenFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            share,
            FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        )
    };
    if status < 0 {
        // Only the fully shared live Office walk treats native delete-pending
        // as missing. Classify it before converting the status to Win32.
        let code = match status {
            STATUS_DELETE_PENDING if share == SHARED => ERROR_FILE_NOT_FOUND,
            _ => unsafe { RtlNtStatusToDosError(status) },
        };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    Ok(OwnedHandle(handle))
}

#[repr(align(8))]
struct EntriesBuffer([u8; 4096]);

fn batch_names(bytes: &[u8], mut visit: impl FnMut(OsString) -> io::Result<()>) -> io::Result<()> {
    let name_offset = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
    let name_length = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength);
    let invalid = || io::Error::other("invalid scratch directory enumeration");
    let mut record = bytes;
    loop {
        if record.len() < name_offset {
            return Err(invalid());
        }
        let next = u32::from_ne_bytes(record[..4].try_into().unwrap()) as usize;
        let length =
            u32::from_ne_bytes(record[name_length..name_length + 4].try_into().unwrap()) as usize;
        let end = name_offset
            .checked_add(length)
            .filter(|end| *end <= record.len())
            .ok_or_else(invalid)?;
        if length == 0
            || !length.is_multiple_of(2)
            || (next != 0 && (!next.is_multiple_of(8) || next < end || next >= record.len()))
        {
            return Err(invalid());
        }
        let units = record[name_offset..end]
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(u16::from_ne_bytes)
            .collect::<Vec<_>>();
        visit(OsString::from_wide(&units))?;
        if next == 0 {
            return Ok(());
        }
        record = &record[next..];
    }
}

fn enumerate(
    listing: &OwnedHandle,
    mut check: impl FnMut() -> io::Result<()>,
    mut visit: impl FnMut(OsString) -> io::Result<()>,
) -> io::Result<()> {
    let mut buffer = EntriesBuffer([0; 4096]);
    loop {
        check()?;
        buffer.0.fill(0);
        if unsafe {
            GetFileInformationByHandleEx(
                listing.0,
                FileIdBothDirectoryInfo,
                buffer.0.as_mut_ptr().cast(),
                buffer.0.len() as u32,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                Ok(())
            } else {
                Err(error)
            };
        }
        batch_names(&buffer.0, &mut visit)?;
    }
}

pub(super) fn require_empty(directory: &OwnedHandle) -> io::Result<()> {
    let mut dot = false;
    let mut dot_dot = false;
    enumerate(
        directory,
        || Ok(()),
        |name| {
            let seen = if name == "." {
                &mut dot
            } else if name == ".." {
                &mut dot_dot
            } else {
                return Err(io::Error::other("scratch must be empty before staging"));
            };
            if std::mem::replace(seen, true) {
                return Err(io::Error::other("invalid scratch directory enumeration"));
            }
            Ok(())
        },
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ScanMode {
    Acl(bool),
    Limits(bool),
    LiveOffice,
}

impl ScanMode {
    fn access_and_share(self) -> (u32, u32) {
        match self {
            Self::Acl(_) => (METADATA, FILE_SHARE_READ | FILE_SHARE_WRITE),
            Self::Limits(_) => (FILE_READ_ATTRIBUTES, FILE_SHARE_READ),
            Self::LiveOffice => (METADATA, SHARED),
        }
    }

    fn allows_missing(self) -> bool {
        match self {
            Self::Acl(allow_missing) | Self::Limits(allow_missing) => allow_missing,
            Self::LiveOffice => true,
        }
    }
}

fn reobserve_denied<T>(
    deadline: Instant,
    mut open: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    match open() {
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            check_time(deadline)?;
            open()
        }
        result => result,
    }
}

struct ScratchInventory {
    mode: ScanMode,
    entries: usize,
    remaining_bytes: u64,
    limit: u64,
    deadline: Instant,
}

fn check_time(deadline: Instant) -> io::Result<()> {
    (Instant::now() < deadline)
        .then_some(())
        .ok_or_else(|| io::Error::other("renderer scratch scan exceeded observation budget"))
}

impl ScratchInventory {
    fn visit(
        &mut self,
        handle: Rc<OwnedHandle>,
        info: &BY_HANDLE_FILE_INFORMATION,
        path: &Path,
        verify: &mut impl FnMut(&Rc<OwnedHandle>, &BY_HANDLE_FILE_INFORMATION, &Path) -> io::Result<()>,
    ) -> io::Result<()> {
        check_time(self.deadline)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            verify(&handle, info, path)?;
            let size = (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow);
            if size > self.limit {
                return Err(io::Error::other(
                    "renderer scratch file exceeds configured limit",
                ));
            }
            let remaining = self.remaining_bytes.checked_sub(size);
            self.remaining_bytes = remaining.ok_or_else(|| {
                io::Error::other("renderer scratch total exceeds configured limit")
            })?;
            return Ok(());
        }
        if path.components().count() > MAX_SCRATCH_SCAN_DEPTH {
            return Err(io::Error::other(
                "renderer scratch scan exceeded depth limit",
            ));
        }
        let (metadata, share) = self.mode.access_and_share();
        let live = self.mode == ScanMode::LiveOffice;
        let deadline = self.deadline;
        let open = |parent: &OwnedHandle, name: &OsStr, access, share| {
            let attempt = || open_at(parent, name, access, share);
            if live {
                reobserve_denied(deadline, attempt)
            } else {
                attempt()
            }
        };
        let listing = match open(
            &handle,
            OsStr::new(""),
            FILE_LIST_DIRECTORY | metadata,
            share,
        ) {
            Err(error)
                if live
                    && error.kind() == io::ErrorKind::NotFound
                    && !path.as_os_str().is_empty() =>
            {
                return Ok(());
            }
            result => scratch_step(path, "open scratch directory for enumeration", result)?,
        };
        let mut names = Vec::new();
        let enumeration = enumerate(
            &listing,
            || check_time(deadline),
            |name| {
                if name != "." && name != ".." {
                    self.entries += 1;
                    if self.entries > MAX_SCRATCH_SCAN_ENTRIES {
                        return Err(io::Error::other(
                            "renderer scratch scan exceeded entry limit",
                        ));
                    }
                    names.push(name);
                }
                Ok(())
            },
        );
        scratch_step(path, "enumerate retained scratch directory", enumeration)?;
        // Strong observations keep the directory pin; live Office drops list
        // access before inspecting any children or calling the verifier.
        let guard = if live {
            drop(listing);
            handle
        } else {
            Rc::new(listing)
        };
        verify(&guard, info, path)?;
        for name in names {
            check_time(self.deadline)?;
            let child_path = path.join(&name);
            let child = match open(&guard, &name, metadata, share | FILE_SHARE_WRITE) {
                Err(error)
                    if self.mode.allows_missing() && error.kind() == io::ErrorKind::NotFound =>
                {
                    continue;
                }
                result => scratch_step(&child_path, "open scratch entry for inspection", result)?,
            };
            let info = scratch_step(
                &child_path,
                "check scratch entry type",
                scratch_entry_information(&child),
            )?;
            self.visit(Rc::new(child), &info, &child_path, verify)?;
        }
        Ok(())
    }
}

pub(super) fn observe(
    root: Rc<OwnedHandle>,
    mode: ScanMode,
    deadline: Instant,
    max_bytes: u64,
    mut verify: impl FnMut(&Rc<OwnedHandle>, &BY_HANDLE_FILE_INFORMATION, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut inventory = ScratchInventory {
        mode,
        entries: 0,
        remaining_bytes: max_bytes,
        limit: max_bytes,
        deadline,
    };
    let info = scratch_entry_information(&root)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(io::Error::other("renderer scratch root is not a directory"));
    }
    inventory.visit(root, &info, Path::new(""), &mut verify)?;
    check_time(deadline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;

    #[test]
    fn reobservation_requires_evidence_before_disregarding_an_access_denial() {
        let deadline = Instant::now() + Duration::from_secs(5);
        for second in [io::ErrorKind::PermissionDenied, io::ErrorKind::NotFound] {
            let mut attempts = 0;
            let error = reobserve_denied(deadline, || {
                attempts += 1;
                Err::<(), _>(if attempts == 1 {
                    io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)
                } else {
                    second.into()
                })
            })
            .unwrap_err();
            assert_eq!(attempts, 2);
            assert_eq!(error.kind(), second);
        }
    }

    #[test]
    fn live_office_skips_a_directory_marked_delete_pending_after_enumeration() {
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
        };

        for mode in [ScanMode::LiveOffice, ScanMode::Acl(false)] {
            let scratch = tempfile::tempdir().unwrap();
            let child = scratch.path().join("registry");
            std::fs::create_dir(&child).unwrap();
            let pending = std::fs::OpenOptions::new()
                .access_mode(DELETE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .share_mode(SHARED)
                .open(&child)
                .unwrap();
            let (root, _) = open_scratch_entry(scratch.path(), METADATA, SHARED).unwrap();
            let result = observe(
                Rc::new(root),
                mode,
                Instant::now() + Duration::from_secs(5),
                0,
                |_, _, path| {
                    assert_eq!(path, Path::new(""), "delete-pending entry reached verifier");
                    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
                    assert_ne!(
                        unsafe {
                            SetFileInformationByHandle(
                                pending.as_raw_handle(),
                                FileDispositionInfo,
                                (&raw const disposition).cast(),
                                std::mem::size_of_val(&disposition) as u32,
                            )
                        },
                        0,
                        "{}",
                        io::Error::last_os_error()
                    );
                    Ok(())
                },
            );
            assert_eq!(result.is_ok(), mode == ScanMode::LiveOffice, "{result:?}");
            drop(pending);
            assert!(!child.exists());
        }
    }

    #[test]
    fn live_observation_allows_mutation_and_keeps_traversal_on_the_open_directory() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let moved = root.path().join("moved");
        std::fs::create_dir(&profile).unwrap();
        std::fs::write(profile.join("original"), b"fixture").unwrap();
        let (handle, _) = open_scratch_entry(root.path(), METADATA, SHARED).unwrap();
        let mut checked_file = false;
        observe(
            Rc::new(handle),
            ScanMode::LiveOffice,
            Instant::now() + Duration::from_secs(5),
            1024,
            |_, _, path| {
                if path == Path::new("profile") {
                    // Rename requires delete sharing. Replacing the original name
                    // must not redirect enumeration into the new directory.
                    std::fs::rename(&profile, &moved).unwrap();
                    std::fs::create_dir(&profile).unwrap();
                    std::fs::write(profile.join("replacement"), b"wrong object").unwrap();
                } else if !path.as_os_str().is_empty() {
                    assert_eq!(path, Path::new(r"profile\original"));
                    std::fs::OpenOptions::new()
                        .write(true)
                        .share_mode(0)
                        .open(moved.join("original"))
                        .unwrap();
                    checked_file = true;
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(checked_file);
    }
}
