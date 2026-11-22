//! Read-only, fully shared, handle-relative inventory for a live Office job.
use super::*;
use std::{io, os::windows::ffi::OsStringExt as _, rc::Rc};
use windows_sys::Wdk::{
    Foundation::OBJECT_ATTRIBUTES,
    Storage::FileSystem::{FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtOpenFile},
};
use windows_sys::Win32::{
    Foundation::{RtlNtStatusToDosError, UNICODE_STRING},
    Storage::FileSystem::SYNCHRONIZE,
    System::IO::IO_STATUS_BLOCK,
};

pub(super) const SHARED: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
pub(super) const METADATA: u32 = FILE_READ_ATTRIBUTES | READ_CONTROL;

// Empty name reopens the exact directory. Non-empty names are a single leaf
// from the kernel directory stream, never an ambient path.
fn open_at(parent: &OwnedHandle, name: &OsStr, access: u32, share: u32) -> io::Result<OwnedHandle> {
    let mut name = name.encode_wide().collect::<Vec<_>>();
    if name.iter().any(|unit| matches!(*unit, 0 | 47 | 58 | 92)) {
        return Err(io::Error::other("invalid scratch directory entry name"));
    }
    let size = u16::try_from(name.len().saturating_mul(2))
        .map_err(|_| io::Error::other("scratch directory entry name is too long"))?;
    let mut unicode = UNICODE_STRING {
        Length: size,
        MaximumLength: size,
        Buffer: if name.is_empty() {
            std::ptr::null_mut()
        } else {
            name.as_mut_ptr()
        },
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
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
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
    Acl { allow_missing: bool },
    Limits,
    LiveOffice,
}

impl ScanMode {
    fn access_and_share(self) -> (u32, u32) {
        match self {
            Self::Acl { .. } => (METADATA, FILE_SHARE_READ | FILE_SHARE_WRITE),
            Self::Limits => (FILE_READ_ATTRIBUTES, FILE_SHARE_READ),
            Self::LiveOffice => (METADATA, SHARED),
        }
    }
}

struct ScratchInventory {
    mode: ScanMode,
    entries: usize,
    bytes: u64,
    limit: u64,
    deadline: Instant,
}

fn check_time(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::other(
            "renderer scratch scan exceeded observation budget",
        ));
    }
    Ok(())
}

impl ScratchInventory {
    fn directory_names(&mut self, listing: &OwnedHandle) -> io::Result<Vec<OsString>> {
        let mut names = Vec::new();
        let deadline = self.deadline;
        enumerate(
            listing,
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
        )?;
        Ok(names)
    }

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
            self.bytes = self
                .bytes
                .checked_add(size)
                .filter(|total| *total <= self.limit)
                .ok_or_else(|| {
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
        let listing = scratch_access::scratch_step(
            Path::new(""),
            path,
            "open retained scratch directory for enumeration",
            open_at(
                &handle,
                OsStr::new(""),
                FILE_LIST_DIRECTORY | metadata,
                share,
            ),
        )?;
        let names = scratch_access::scratch_step(
            Path::new(""),
            path,
            "enumerate retained scratch directory",
            self.directory_names(&listing),
        )?;
        // Strong observations keep the directory pin; live Office drops list
        // access before inspecting any children or calling the verifier.
        let guard = if self.mode == ScanMode::LiveOffice {
            drop(listing);
            handle
        } else {
            Rc::new(listing)
        };
        verify(&guard, info, path)?;
        for name in names {
            check_time(self.deadline)?;
            let child_path = path.join(&name);
            let opened = scratch_access::retry_live_scratch_open(
                (self.mode == ScanMode::LiveOffice).then_some(self.deadline),
                || open_at(&guard, &name, metadata, share | FILE_SHARE_WRITE),
            );
            let child = match opened {
                Ok(child) => child,
                Err(error)
                    if !matches!(
                        self.mode,
                        ScanMode::Acl {
                            allow_missing: false
                        }
                    ) && error.kind() == io::ErrorKind::NotFound =>
                {
                    continue;
                }
                Err(error) => {
                    return scratch_access::scratch_step(
                        Path::new(""),
                        &child_path,
                        "open scratch entry for inspection",
                        Err(error),
                    );
                }
            };
            let info = scratch_access::scratch_step(
                Path::new(""),
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
        bytes: 0,
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
