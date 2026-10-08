use crate::Result;
use crate::store_dir::{
    MAX_DIRECTORY_REMOVAL_DEPTH, MAX_DIRECTORY_REMOVAL_ENTRIES, Publication, StoreDirectory,
    StoreEntry, err, ioerr, relative_components,
};
use cap_primitives::fs::OpenOptionsExt;
use cap_primitives::{ambient_authority, fs as cap_fs};
use std::{
    fs::File,
    io::{Read, Write},
    mem,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr,
    sync::atomic::{AtomicU64, Ordering},
};
use windows_sys::Wdk::{
    Foundation::OBJECT_ATTRIBUTES,
    Storage::FileSystem::{
        FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION,
        FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation, NtCreateFile, NtSetInformationFile,
    },
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
        OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileBasicInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, READ_CONTROL,
        SYNCHRONIZE, WRITE_DAC,
    },
    System::IO::IO_STATUS_BLOCK,
};
static TEMP: AtomicU64 = AtomicU64::new(0);
fn full(label: &Path, relative: &Path) -> std::path::PathBuf {
    label.join(relative)
}
pub(crate) fn open(path: &Path) -> Result<File> {
    let (root_path, components) = absolute_components(path)?;
    let mut f = cap_fs::open_ambient_dir(&root_path, ambient_authority())
        .map_err(|_| err(path, "cannot open Windows store root"))?;
    validate_directory(&f, path)?;
    for component in components {
        f = cap_fs::open_dir_nofollow(&f, Path::new(component))
            .map_err(|_| err(path, "store path has a missing or reparse component"))?;
        validate_directory(&f, path)?;
    }
    Ok(f)
}
fn absolute_components(path: &Path) -> Result<(PathBuf, Vec<&std::ffi::OsStr>)> {
    let mut iter = path.components();
    let Some(Component::Prefix(prefix)) = iter.next() else {
        return Err(err(path, "Windows store path must be absolute drive path"));
    };
    if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        || !matches!(iter.next(), Some(Component::RootDir))
    {
        return Err(err(
            path,
            "Windows store path must be an absolute drive path",
        ));
    }
    let mut root = PathBuf::from(prefix.as_os_str());
    root.push(Path::new(r"\"));
    let mut components = Vec::new();
    for c in iter {
        match c {
            Component::Normal(v) => components.push(v),
            _ => return Err(err(path, "Windows store path contains unsafe component")),
        }
    }
    if components.is_empty() {
        return Err(err(path, "Windows store path requires a directory leaf"));
    }
    Ok((root, components))
}
fn change_time(file: &File, label: &Path) -> Result<i64> {
    let mut basic = FILE_BASIC_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileBasicInfo,
            (&mut basic as *mut FILE_BASIC_INFO).cast(),
            mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(ioerr(label, std::io::Error::last_os_error()));
    }
    Ok(basic.ChangeTime)
}
pub(crate) fn volume_serial(file: &File, label: &Path) -> Result<u32> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut information) } == 0
    {
        return Err(ioerr(label, std::io::Error::last_os_error()));
    }
    Ok(information.dwVolumeSerialNumber)
}
pub(crate) fn validate_directory(file: &File, label: &Path) -> Result<()> {
    if crate::platform::directory_handle_identity(file).is_none() {
        Err(err(label, "retained store handle is not a real directory"))
    } else {
        Ok(())
    }
}
fn operation_directory(directory: &File, label: &Path) -> Result<File> {
    reopen_retained_directory(
        directory,
        label,
        GENERIC_READ | GENERIC_WRITE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
    )
}
fn reopen_retained_directory(directory: &File, label: &Path, desired_access: u32) -> Result<File> {
    let identity = crate::platform::directory_handle_identity(directory)
        .ok_or_else(|| err(label, "retained store handle is not a real directory"))?;
    // Windows rejects a retained-relative `.` open for a directory.  An
    // empty native name denotes the retained directory itself without
    // resolving its diagnostic path or adopting an ambient namespace.
    let mut empty = UNICODE_STRING {
        Length: 0,
        MaximumLength: 0,
        Buffer: ptr::null_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: directory.as_raw_handle() as HANDLE,
        ObjectName: &mut empty,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: ptr::null_mut(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut handle = INVALID_HANDLE_VALUE;
    let mut status_block = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access,
            &attributes,
            &mut status_block,
            ptr::null(),
            windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
            ptr::null(),
            0,
        )
    };
    if status != 0 || unsafe { status_block.Anonymous.Status } != 0 {
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(handle) };
        }
        return Err(ioerr(
            label,
            nt_status_error(if status != 0 {
                status
            } else {
                unsafe { status_block.Anonymous.Status }
            }),
        ));
    }
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(err(
            label,
            "native retained directory open returned an invalid handle",
        ));
    }
    let operation = unsafe { File::from_raw_handle(handle as _) };
    if crate::platform::directory_handle_identity(&operation) != Some(identity) {
        return Err(err(label, "retained store directory identity changed"));
    }
    Ok(operation)
}
fn nt_status_error(status: i32) -> std::io::Error {
    std::io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) as i32 })
}
pub(crate) fn normalize_directory_handle(file: File, label: &Path) -> Result<File> {
    use windows_sys::Win32::Storage::FileSystem::{
        ExtendedFileIdType, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0,
        FILE_ID_INFO, FileIdInfo, OpenFileById,
    };
    let full_identity = |directory: &File| -> Result<FILE_ID_INFO> {
        validate_directory(directory, label)?;
        let mut identity = FILE_ID_INFO::default();
        if unsafe {
            GetFileInformationByHandleEx(
                directory.as_raw_handle() as HANDLE,
                FileIdInfo,
                (&mut identity as *mut FILE_ID_INFO).cast(),
                mem::size_of::<FILE_ID_INFO>() as u32,
            )
        } == 0
        {
            return Err(ioerr(label, std::io::Error::last_os_error()));
        }
        Ok(identity)
    };
    let before = full_identity(&file)?;
    // Delete sharing permits a source-directory rename, but named child
    // directory handles still pin ancestor links on Windows. Persistent
    // capabilities must open by ID; empty-name reopens remain transient
    // operation handles. The retained original supplies the volume hint,
    // so no diagnostic path or ambient volume name is resolved here.
    let descriptor = FILE_ID_DESCRIPTOR {
        dwSize: mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: ExtendedFileIdType,
        Anonymous: FILE_ID_DESCRIPTOR_0 {
            ExtendedFileId: before.FileId,
        },
    };
    let handle = unsafe {
        OpenFileById(
            file.as_raw_handle() as HANDLE,
            &descriptor,
            GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // Unsupported filesystems or file-ID opens fail closed. Falling
        // back to a named open would restore the ancestor-link pin.
        return Err(ioerr(label, std::io::Error::last_os_error()));
    }
    if handle.is_null() {
        return Err(err(
            label,
            "file-ID directory open returned an invalid handle",
        ));
    }
    let normalized = unsafe { File::from_raw_handle(handle as _) };
    let after = full_identity(&normalized)?;
    if before.VolumeSerialNumber != after.VolumeSerialNumber
        || before.FileId.Identifier != after.FileId.Identifier
    {
        return Err(err(label, "retained store directory identity changed"));
    }
    drop(file);
    Ok(normalized)
}
fn directory(root: &File, relative: &Path, label: &Path) -> Result<File> {
    let mut d = cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
    for c in relative_components(relative, true, label)? {
        d = cap_fs::open_dir_nofollow(&d, Path::new(c)).map_err(|_| {
            err(
                &full(label, relative),
                "store directory is missing or reparse",
            )
        })?;
    }
    validate_directory(&d, &full(label, relative))?;
    Ok(d)
}
fn parent<'a>(
    root: &File,
    relative: &'a Path,
    label: &Path,
) -> Result<(File, &'a std::ffi::OsStr)> {
    let c = relative_components(relative, false, label)?;
    let leaf = *c.last().unwrap();
    let mut d = cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
    for part in &c[..c.len() - 1] {
        d = cap_fs::open_dir_nofollow(&d, Path::new(part))
            .map_err(|_| err(label, "store parent is missing or reparse"))?;
    }
    Ok((d, leaf))
}
fn leaf(
    parent: &File,
    name: &std::ffi::OsStr,
    read: bool,
    write: bool,
    append: bool,
    label: &Path,
) -> Result<File> {
    let mut o = cap_fs::OpenOptions::new();
    o.read(read)
        .write(write)
        .append(append)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    cap_fs::open(parent, Path::new(name), &o).map_err(|e| ioerr(label, e))
}
fn parent_optional<'a>(
    root: &File,
    relative: &'a Path,
    label: &Path,
) -> Result<Option<(File, &'a std::ffi::OsStr)>> {
    let c = relative_components(relative, false, label)?;
    let leaf = *c.last().unwrap();
    let mut d = cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
    for part in &c[..c.len() - 1] {
        match cap_fs::open_dir_nofollow(&d, Path::new(part)) {
            Ok(next) => d = next,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(err(label, "store parent is unsafe")),
        }
    }
    Ok(Some((d, leaf)))
}
fn leaf_optional(parent: &File, name: &std::ffi::OsStr, label: &Path) -> Result<Option<File>> {
    let mut o = cap_fs::OpenOptions::new();
    o.read(true)._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    match cap_fs::open(parent, Path::new(name), &o) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ioerr(label, e)),
    }
}
fn regular(file: &File, label: &Path) -> Result<std::fs::Metadata> {
    if crate::platform::regular_file_handle_identity(file).is_none() {
        return Err(err(
            label,
            "store leaf is reparse, nonregular, or hardlinked",
        ));
    }
    file.metadata().map_err(|e| ioerr(label, e))
}
pub(crate) fn open_directory(root: &File, relative: &Path, label: &Path) -> Result<File> {
    directory(root, relative, label)
}
pub(crate) fn create_directory_all(root: &File, relative: &Path, label: &Path) -> Result<File> {
    let mut d = cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
    for c in relative_components(relative, true, label)? {
        match cap_fs::open_dir_nofollow(&d, Path::new(c)) {
            Ok(next) => d = next,
            Err(_) => {
                d = crate::private_fs::create_new_private_at(
                    &d,
                    c,
                    crate::private_fs::NewPrivateObjectKind::Directory,
                )
                .map_err(|e| ioerr(label, e))?;
                validate_directory(&d, label)?;
                crate::private_fs::verify_owner_private_handle(&d)?;
                sync_directory(&d, label)?;
            }
        }
    }
    Ok(d)
}
pub(crate) fn create_directory(root: &File, relative: &Path, label: &Path) -> Result<File> {
    let (parent, leaf) = parent(root, relative, label)?;
    let path = full(label, relative);
    let created = crate::private_fs::create_new_private_at(
        &parent,
        leaf,
        crate::private_fs::NewPrivateObjectKind::Directory,
    )
    .map_err(|e| ioerr(&path, e))?;
    validate_directory(&created, &path)?;
    crate::private_fs::verify_owner_private_handle(&created)?;
    sync_directory(&parent, label)?;
    Ok(created)
}
pub(crate) fn open_regular_read(
    root: &File,
    relative: &Path,
    max: u64,
    label: &Path,
) -> Result<File> {
    let (p, n) = parent(root, relative, label)?;
    let path = full(label, relative);
    let f = leaf(&p, n, true, false, false, &path)?;
    if regular(&f, &path)?.len() > max {
        return Err(err(&path, "store file exceeds byte limit"));
    }
    Ok(f)
}
pub(crate) fn open_regular_for_removal(
    root: &File,
    relative: &Path,
    max: u64,
    label: &Path,
) -> Result<File> {
    let (parent, leaf) = parent(root, relative, label)?;
    let path = full(label, relative);
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .access_mode(
            FILE_READ_DATA | READ_CONTROL | WRITE_DAC | DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        )
        .share_mode(FILE_SHARE_READ)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(&parent, Path::new(leaf), &options).map_err(|e| ioerr(&path, e))?;
    if regular(&file, &path)?.len() > max {
        return Err(err(&path, "store file exceeds byte limit"));
    }
    Ok(file)
}
/// Open one GC mutation leaf with read/write/delete rights and read-only sharing.
pub fn open_gc_mutation(directory: &StoreDirectory, leaf: &Path, max_bytes: u64) -> Result<File> {
    direct_leaf(leaf, directory.path())?;
    open_gc_mutation_retained(&directory.root_handle(), leaf, max_bytes, directory.path())
}

/// Delete this exact single-link file and verify zero links before reclaiming bytes.
pub fn retire_gc_handle(directory: &StoreDirectory, file: &File) -> Result<()> {
    retire_gc_handle_retained(file, directory.path())
}

/// Move a Windows regular file by its already-validated retained handle.
/// The source name is never reopened by this primitive.
pub fn move_verified_regular_handle_to_raw(
    directory: &StoreDirectory,
    source: &File,
    destination_parent: &StoreDirectory,
    destination_leaf: &Path,
) -> Result<()> {
    move_verified_regular_handle_to_raw_retained(
        &directory.root_handle(),
        source,
        &destination_parent.root_handle(),
        destination_leaf,
        directory.path(),
        destination_parent.path(),
    )
}

fn open_gc_mutation_retained(root: &File, relative: &Path, max: u64, label: &Path) -> Result<File> {
    let (parent, leaf) = parent(root, relative, label)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_READ)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(&parent, Path::new(leaf), &options).map_err(|e| ioerr(label, e))?;
    if regular(&file, label)?.len() > max {
        return Err(err(label, "GC mutation file exceeds byte limit"));
    }
    Ok(file)
}

pub(crate) fn retire_gc_handle_retained(file: &File, label: &Path) -> Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX, FileDispositionInfoEx, SetFileInformationByHandle,
    };
    let expected = crate::platform::regular_file_handle_identity(file)
        .ok_or_else(|| err(label, "GC retirement requires a single-link regular handle"))?
        .atomic_recovery_components();
    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as _,
            FileDispositionInfoEx,
            &disposition as *const _ as _,
            std::mem::size_of_val(&disposition) as u32,
        )
    } == 0
    {
        return Err(ioerr(label, std::io::Error::last_os_error()));
    }
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut information) } == 0 {
        return Err(ioerr(label, std::io::Error::last_os_error()));
    }
    let actual = (
        information.dwVolumeSerialNumber,
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    );
    if actual != expected || information.nNumberOfLinks != 0 {
        return Err(err(
            label,
            "GC retirement did not preserve identity with zero links",
        ));
    }
    file.set_len(0).map_err(|e| ioerr(label, e))?;
    file.sync_all().map_err(|e| ioerr(label, e))?;
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut information) } == 0 {
        return Err(ioerr(label, std::io::Error::last_os_error()));
    }
    let after = (
        information.dwVolumeSerialNumber,
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    );
    if after != expected
        || information.nNumberOfLinks != 0
        || information.nFileSizeHigh != 0
        || information.nFileSizeLow != 0
    {
        return Err(err(
            label,
            "GC retirement postcondition failed after reclamation",
        ));
    }
    Ok(())
}
pub(crate) fn read_optional(
    root: &File,
    relative: &Path,
    max: u64,
    label: &Path,
) -> Result<Option<Vec<u8>>> {
    let Some((p, n)) = parent_optional(root, relative, label)? else {
        return Ok(None);
    };
    let path = full(label, relative);
    let Some(mut f) = leaf_optional(&p, n, &path)? else {
        return Ok(None);
    };
    let before = regular(&f, &path)?;
    let before_identity = crate::platform::regular_file_handle_identity(&f)
        .ok_or_else(|| err(&path, "store leaf changed type while being read"))?;
    let before_change = change_time(&f, &path)?;
    if before.len() > max {
        return Err(err(&path, "store file exceeds byte limit"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut f)
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| ioerr(&path, e))?;
    let after = regular(&f, &path)?;
    let after_identity = crate::platform::regular_file_handle_identity(&f)
        .ok_or_else(|| err(&path, "store leaf changed type while being read"))?;
    let after_change = change_time(&f, &path)?;
    if bytes.len() as u64 > max
        || before_identity != after_identity
        || before.len() != after.len()
        || before_change != after_change
        || after.len() != bytes.len() as u64
    {
        return Err(err(&path, "store file changed while it was read"));
    }
    Ok(Some(bytes))
}
pub(crate) fn create_private_file_raw(root: &File, leaf: &Path, label: &Path) -> Result<File> {
    let leaf = direct_leaf(leaf, label)?;
    let path = label.join(leaf);
    let file = crate::private_fs::create_new_private_at(
        root,
        leaf,
        crate::private_fs::NewPrivateObjectKind::RegularFile,
    )
    .map_err(|e| ioerr(&path, e))?;
    regular(&file, &path)?;
    crate::private_fs::verify_owner_private_handle(&file)?;
    Ok(file)
}
pub(crate) fn open_private_gate_raw(root: &File, leaf: &Path, label: &Path) -> Result<File> {
    let leaf = direct_leaf(leaf, label)?;
    let path = label.join(leaf);
    match create_private_file_raw(root, Path::new(leaf), label) {
        Ok(file) => {
            if file.metadata().map_err(|e| ioerr(&path, e))?.len() != 0 {
                return Err(err(&path, "private gate file is not empty"));
            }
            Ok(file)
        }
        Err(error) if is_io_kind(&error, "already_exists") => {
            let mut options = cap_fs::OpenOptions::new();
            options
                .read(true)
                .write(true)
                ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
            let file =
                cap_fs::open(root, Path::new(leaf), &options).map_err(|e| ioerr(&path, e))?;
            if regular(&file, &path)?.len() != 0 {
                return Err(err(&path, "private gate file is not empty"));
            }
            crate::private_fs::verify_owner_private_handle(&file)?;
            Ok(file)
        }
        Err(error) => Err(error),
    }
}
pub(crate) fn rename_regular_between_raw(
    source_parent: &File,
    source_leaf: &Path,
    destination_parent: &File,
    destination_leaf: &Path,
    publication: Publication,
    source_label: &Path,
    destination_label: &Path,
) -> Result<()> {
    use windows_sys::Win32::Foundation::GENERIC_READ;

    let source = direct_leaf(source_leaf, source_label)?;
    let destination = direct_leaf(destination_leaf, destination_label)?;
    let source_path = source_label.join(source);
    let destination_path = destination_label.join(destination);
    validate_directory(source_parent, source_label)?;
    validate_directory(destination_parent, destination_label)?;
    if volume_serial(source_parent, source_label)?
        != volume_serial(destination_parent, destination_label)?
    {
        return Err(err(
            &destination_path,
            "regular rename crosses filesystem volumes",
        ));
    }
    let mut source_options = cap_fs::OpenOptions::new();
    source_options
        .read(true)
        .access_mode(GENERIC_READ | DELETE | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let source_file = cap_fs::open(source_parent, Path::new(source), &source_options)
        .map_err(|e| ioerr(&source_path, e))?;
    let source_identity = crate::platform::regular_file_handle_identity(&source_file)
        .ok_or_else(|| err(&source_path, "source is not a single-link regular file"))?;
    match publication {
        Publication::CreateOnly => rename_regular_handle(
            &source_file,
            destination_parent,
            destination,
            false,
            &destination_path,
        )?,
        Publication::Replace => {
            let old = leaf(
                destination_parent,
                destination,
                true,
                false,
                false,
                &destination_path,
            )?;
            regular(&old, &destination_path)?;
            drop(old);
            rename_regular_handle(
                &source_file,
                destination_parent,
                destination,
                true,
                &destination_path,
            )?;
        }
        Publication::Upsert => match rename_regular_handle(
            &source_file,
            destination_parent,
            destination,
            false,
            &destination_path,
        ) {
            Ok(()) => {}
            Err(error) if is_io_kind(&error, "already_exists") => {
                let old = leaf(
                    destination_parent,
                    destination,
                    true,
                    false,
                    false,
                    &destination_path,
                )?;
                regular(&old, &destination_path)?;
                drop(old);
                rename_regular_handle(
                    &source_file,
                    destination_parent,
                    destination,
                    true,
                    &destination_path,
                )?;
            }
            Err(error) => return Err(error),
        },
    }
    let published = leaf(
        destination_parent,
        destination,
        true,
        false,
        false,
        &destination_path,
    )?;
    if crate::platform::regular_file_handle_identity(&published) != Some(source_identity) {
        return Err(err(
            &destination_path,
            "published regular-file identity changed",
        ));
    }
    sync_directory(source_parent, source_label)?;
    sync_directory(destination_parent, destination_label)
}
fn move_verified_regular_handle_to_raw_retained(
    source_parent: &File,
    source: &File,
    destination_parent: &File,
    destination_leaf: &Path,
    source_label: &Path,
    destination_label: &Path,
) -> Result<()> {
    let destination = direct_leaf(destination_leaf, destination_label)?;
    let destination_path = destination_label.join(destination);
    validate_directory(source_parent, source_label)?;
    validate_directory(destination_parent, destination_label)?;
    if volume_serial(source_parent, source_label)?
        != volume_serial(destination_parent, destination_label)?
    {
        return Err(err(
            &destination_path,
            "regular rename crosses filesystem volumes",
        ));
    }
    let source_identity = crate::platform::regular_file_handle_identity(source)
        .ok_or_else(|| err(source_label, "source is not a single-link regular file"))?;
    rename_regular_handle(
        source,
        destination_parent,
        destination,
        false,
        &destination_path,
    )?;
    if crate::platform::regular_file_handle_identity(source) != Some(source_identity) {
        return Err(err(
            &destination_path,
            "moved regular-file handle identity changed",
        ));
    }
    sync_directory(source_parent, source_label)?;
    sync_directory(destination_parent, destination_label)
}
fn rename_regular_handle(
    source: &File,
    destination_parent: &File,
    destination: &std::ffi::OsStr,
    replace: bool,
    label: &Path,
) -> Result<()> {
    rename_retained_handle(source, destination_parent, destination, replace, label)
}
pub(crate) fn rename_retained_handle(
    source: &File,
    destination_parent: &File,
    destination: &std::ffi::OsStr,
    replace: bool,
    label: &Path,
) -> Result<()> {
    let name = destination.encode_wide().collect::<Vec<_>>();
    if name.is_empty() || name.contains(&0) {
        return Err(err(label, "retained rename destination is invalid"));
    }
    let name_bytes = name
        .len()
        .checked_mul(mem::size_of::<u16>())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| err(label, "retained rename destination is too long"))?;
    // FILE_RENAME_INFORMATION has one trailing WCHAR and may contain
    // alignment padding after it.  Allocate from the real FileName offset,
    // include a terminating zero, and retain usize alignment for the native
    // structure.  The source and destination capabilities remain live for
    // the synchronous NtSetInformationFile call.
    let total = mem::offset_of!(FILE_RENAME_INFORMATION, FileName)
        .checked_add(name_bytes as usize)
        .and_then(|value| value.checked_add(mem::size_of::<u16>()))
        .ok_or_else(|| err(label, "retained rename buffer overflow"))?;
    let words = total
        .checked_add(mem::size_of::<usize>() - 1)
        .ok_or_else(|| err(label, "retained rename buffer overflow"))?
        / mem::size_of::<usize>();
    let mut buffer = vec![0_usize; words];
    let mut status_block = IO_STATUS_BLOCK::default();
    let status = unsafe {
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        (*info).Anonymous.ReplaceIfExists = replace;
        (*info).RootDirectory = destination_parent.as_raw_handle() as HANDLE;
        (*info).FileNameLength = name_bytes;
        ptr::copy_nonoverlapping(
            name.as_ptr(),
            ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
        NtSetInformationFile(
            source.as_raw_handle() as HANDLE,
            &mut status_block,
            buffer.as_ptr().cast(),
            u32::try_from(total).map_err(|_| err(label, "retained rename buffer overflow"))?,
            FileRenameInformation,
        )
    };
    if status != 0 || unsafe { status_block.Anonymous.Status } != 0 {
        return Err(ioerr(
            label,
            nt_status_error(if status != 0 {
                status
            } else {
                unsafe { status_block.Anonymous.Status }
            }),
        ));
    }
    Ok(())
}
fn is_io_kind(error: &crate::KioError, kind: &str) -> bool {
    error.error_code() == "KIO-E-STORE-IO-001"
        && error
            .context()
            .get("io_error_kind")
            .and_then(serde_json::Value::as_str)
            == Some(kind)
}
pub(crate) fn append(root: &File, relative: &Path, bytes: &[u8], label: &Path) -> Result<()> {
    let (p, n) = parent(root, relative, label)?;
    let path = full(label, relative);
    let mut f = leaf(&p, n, false, true, true, &path)?;
    regular(&f, &path)?;
    f.write_all(bytes).map_err(|e| ioerr(&path, e))?;
    f.sync_data().map_err(|e| ioerr(&path, e))
}
pub(crate) fn remove_file(root: &File, relative: &Path, label: &Path) -> Result<()> {
    let (p, n) = parent(root, relative, label)?;
    let path = full(label, relative);
    let f = leaf(&p, n, true, false, false, &path)?;
    regular(&f, &path)?;
    drop(f);
    cap_fs::remove_file(&p, Path::new(n)).map_err(|e| ioerr(&path, e))?;
    sync_directory(&p, label)
}
pub(crate) fn rename_directory_create_only(
    root: &File,
    source: &Path,
    destination: &Path,
    label: &Path,
) -> Result<()> {
    let (parent, source_leaf, destination_leaf) = same_parent(root, source, destination, label)?;
    let source_path = full(label, source);
    let destination_path = full(label, destination);
    let mut source_options = cap_fs::OpenOptions::new();
    source_options
        .access_mode(DELETE | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let source_directory = cap_fs::open(&parent, Path::new(source_leaf), &source_options)
        .map_err(|_| err(&source_path, "source directory is missing or unsafe"))?;
    validate_directory(&source_directory, &source_path)?;
    rename_directory_noreplace(&source_directory, &parent, destination_leaf, &source_path)?;
    let published = open_published_directory(&parent, destination_leaf, &destination_path)?;
    if crate::platform::directory_handle_identity(&source_directory)
        != crate::platform::directory_handle_identity(&published)
    {
        return Err(err(
            &destination_path,
            "published directory identity changed",
        ));
    }
    sync_directory(&parent, label)
}
pub(crate) fn rename_directory_between_create_only(
    source_parent: &File,
    source_leaf: &Path,
    destination_parent: &File,
    destination_leaf: &Path,
    source_label: &Path,
    destination_label: &Path,
) -> Result<()> {
    let source = direct_leaf(source_leaf, source_label)?;
    let destination = direct_leaf(destination_leaf, destination_label)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .access_mode(DELETE | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let held = cap_fs::open(source_parent, Path::new(source), &options)
        .map_err(|_| err(source_label, "source directory is missing or unsafe"))?;
    validate_directory(&held, source_label)?;
    rename_directory_noreplace(&held, destination_parent, destination, source_label)?;
    let published = open_published_directory(destination_parent, destination, destination_label)?;
    if crate::platform::directory_handle_identity(&held)
        != crate::platform::directory_handle_identity(&published)
    {
        return Err(err(
            destination_label,
            "published directory identity changed",
        ));
    }
    sync_directory(source_parent, source_label)?;
    sync_directory(destination_parent, destination_label)
}
fn open_published_directory(parent: &File, leaf: &std::ffi::OsStr, label: &Path) -> Result<File> {
    // Keep delete sharing while the source's DELETE handle remains alive
    // for the identity check. cap-primitives directory options omit it.
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let published = cap_fs::open(parent, Path::new(leaf), &options)
        .map_err(|_| err(label, "published directory is missing or unsafe"))?;
    validate_directory(&published, label)?;
    Ok(published)
}
pub(crate) fn direct_leaf<'a>(path: &'a Path, label: &Path) -> Result<&'a std::ffi::OsStr> {
    let c = relative_components(path, false, label)?;
    if c.len() != 1 {
        return Err(err(label, "directory rename requires one direct leaf"));
    }
    Ok(c[0])
}
pub(crate) fn quarantine_directory(root: &File, relative: &Path, label: &Path) -> Result<PathBuf> {
    let components = relative_components(relative, false, label)?;
    let mut destination = PathBuf::new();
    for component in &components[..components.len() - 1] {
        destination.push(component);
    }
    destination.push(format!(
        ".kio-directory-quarantine-{}-{}",
        std::process::id(),
        TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    rename_directory_create_only(root, relative, &destination, label)?;
    Ok(destination)
}
pub(crate) fn remove_directory_all(root: &File, relative: &Path, label: &Path) -> Result<()> {
    let (parent, leaf) = parent(root, relative, label)?;
    let path = full(label, relative);
    let directory = cap_fs::open_dir_nofollow(&parent, Path::new(leaf))
        .map_err(|_| err(&path, "directory removal target is missing or unsafe"))?;
    validate_directory(&directory, &path)?;
    let mut remaining = MAX_DIRECTORY_REMOVAL_ENTRIES;
    remove_directory_contents(&directory, &path, 0, &mut remaining)?;
    drop(directory);
    cap_fs::remove_dir(&parent, Path::new(leaf)).map_err(|error| ioerr(&path, error))?;
    sync_directory(&parent, label)
}
fn same_parent<'a>(
    root: &File,
    source: &'a Path,
    destination: &'a Path,
    label: &Path,
) -> Result<(File, &'a std::ffi::OsStr, &'a std::ffi::OsStr)> {
    let source_components = relative_components(source, false, label)?;
    let destination_components = relative_components(destination, false, label)?;
    if source_components.len() != destination_components.len()
        || source_components[..source_components.len() - 1]
            != destination_components[..destination_components.len() - 1]
    {
        return Err(err(label, "directory rename requires the same parent"));
    }
    let (parent, source_leaf) = parent(root, source, label)?;
    Ok((
        parent,
        source_leaf,
        *destination_components.last().expect("nonempty destination"),
    ))
}
fn remove_directory_contents(
    directory: &File,
    label: &Path,
    depth: usize,
    remaining: &mut usize,
) -> Result<()> {
    if depth >= MAX_DIRECTORY_REMOVAL_DEPTH {
        return Err(err(label, "directory removal exceeds maximum depth"));
    }
    for entry in cap_fs::read_dir(directory, Path::new(".")).map_err(|error| ioerr(label, error))? {
        let entry = entry.map_err(|error| ioerr(label, error))?;
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| err(label, "directory removal exceeds entry limit"))?;
        let name = entry.file_name();
        let child_label = label.join(&name);
        match cap_fs::open_dir_nofollow(directory, Path::new(&name)) {
            Ok(child) => {
                validate_directory(&child, &child_label)?;
                remove_directory_contents(&child, &child_label, depth + 1, remaining)?;
                drop(child);
                cap_fs::remove_dir(directory, Path::new(&name))
                    .map_err(|error| ioerr(&child_label, error))?;
            }
            Err(_) => {
                let mut options = cap_fs::OpenOptions::new();
                options
                    .read(true)
                    ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
                let file = cap_fs::open(directory, Path::new(&name), &options)
                    .map_err(|_| err(&child_label, "directory removal entry is unsafe"))?;
                regular(&file, &child_label)?;
                drop(file);
                cap_fs::remove_file(directory, Path::new(&name))
                    .map_err(|error| ioerr(&child_label, error))?;
            }
        }
    }
    sync_directory(directory, label)
}
fn rename_directory_noreplace(
    source: &File,
    parent: &File,
    destination: &std::ffi::OsStr,
    label: &Path,
) -> Result<()> {
    rename_retained_handle(source, parent, destination, false, label)
}
pub(crate) fn ensure_owner_private(root: &File, relative: &Path, label: &Path) -> Result<()> {
    let f = open_regular_read(root, relative, u64::MAX, label)?;
    regular(&f, &full(label, relative))?;
    crate::private_fs::verify_owner_private_handle(&f)
}
pub(crate) fn entries(root: &File, relative: &Path, label: &Path) -> Result<Vec<StoreEntry>> {
    let d = directory(root, relative, label)?;
    let mut out = Vec::new();
    for entry in cap_fs::read_dir(&d, Path::new(".")).map_err(|e| ioerr(label, e))? {
        let entry = entry.map_err(|e| ioerr(label, e))?;
        let name = entry.file_name();
        let child_label = full(label, relative).join(&name);
        let mut o = cap_fs::OpenOptions::new();
        // Windows requires directory-open semantics even when an entry
        // may also be a regular file. Keep the same retained parent and
        // no-follow policy for both kinds of entry.
        o.read(true)
            ._cap_fs_ext_maybe_dir(true)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let child = cap_fs::open(&d, Path::new(&name), &o).map_err(|_| {
            err(
                &child_label,
                "store entry cannot be opened without following links",
            )
        })?;
        let meta = child.metadata().map_err(|e| ioerr(&child_label, e))?;
        if meta.is_dir() {
            validate_directory(&child, &child_label)?;
        } else {
            regular(&child, &child_label)?;
        }
        out.push(StoreEntry {
            name,
            is_directory: meta.is_dir(),
            is_regular_file: meta.is_file(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}
pub(crate) fn entries_optional(
    root: &File,
    relative: &Path,
    label: &Path,
) -> Result<Option<Vec<StoreEntry>>> {
    let components = relative_components(relative, true, label)?;
    let mut d = cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
    for c in components {
        match cap_fs::open_dir_nofollow(&d, Path::new(c)) {
            Ok(next) => d = next,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(err(label, "store directory is unsafe")),
        }
    }
    entries(root, relative, label).map(Some)
}
pub(crate) fn sync_directory(directory: &File, label: &Path) -> Result<()> {
    let operation = operation_directory(directory, label)?;
    operation.sync_all().map_err(|e| ioerr(label, e))
}
pub(crate) fn restrict_new_private_directory(directory: &File) -> Result<()> {
    validate_directory(directory, Path::new("<retained>"))?;
    crate::private_fs::verify_owner_private_handle(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_dir::StoreDirectory;
    use std::{
        fs::{self, OpenOptions},
        os::windows::fs::OpenOptionsExt as _,
    };

    fn directory(path: &Path) -> StoreDirectory {
        fs::create_dir(path).expect("directory fixture");
        StoreDirectory::open(path).expect("retained directory")
    }

    #[test]
    fn verified_handle_move_does_not_adopt_a_replaced_source_name() {
        let root = tempfile::tempdir().expect("fixture");
        let source_parent = directory(&root.path().join("source"));
        let destination_parent = directory(&root.path().join("destination"));
        let source_path = source_parent.path().join("leaf");
        fs::write(&source_path, b"validated").expect("source body");
        let source = OpenOptions::new()
            .read(true)
            .access_mode(FILE_READ_DATA | FILE_READ_ATTRIBUTES | DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&source_path)
            .expect("share-all source handle");

        fs::rename(&source_path, source_parent.path().join("displaced"))
            .expect("replace namespace source");
        fs::write(&source_path, b"replacement").expect("replacement body");
        let source_handle = source_parent.root_handle();
        let destination_handle = destination_parent.root_handle();
        move_verified_regular_handle_to_raw_retained(
            &source_handle,
            &source,
            &destination_handle,
            Path::new("removed"),
            source_parent.path(),
            destination_parent.path(),
        )
        .expect("move retained source handle");

        assert_eq!(
            fs::read(destination_parent.path().join("removed")).unwrap(),
            b"validated"
        );
        assert_eq!(fs::read(&source_path).unwrap(), b"replacement");
    }

    #[test]
    fn gc_retirement_preserves_identity_and_reclaims_a_retained_reader() {
        use std::io::Read as _;
        let root = tempfile::tempdir().unwrap();
        let parent = directory(&root.path().join("retire"));
        let path = parent.path().join("leaf");
        fs::write(&path, b"reclaim these bytes").unwrap();
        let mut reader = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&path)
            .unwrap();
        let mutator = open_gc_mutation(&parent, Path::new("leaf"), 64).unwrap();
        let before = crate::platform::regular_file_handle_identity(&mutator)
            .unwrap()
            .atomic_recovery_components();
        retire_gc_handle(&parent, &mutator).unwrap();
        let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        assert_ne!(
            unsafe { GetFileInformationByHandle(mutator.as_raw_handle() as _, &mut information) },
            0
        );
        assert_eq!(
            (
                information.dwVolumeSerialNumber,
                (u64::from(information.nFileIndexHigh) << 32)
                    | u64::from(information.nFileIndexLow)
            ),
            before
        );
        assert_eq!(information.nNumberOfLinks, 0);
        assert_eq!(mutator.metadata().unwrap().len(), 0);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert!(bytes.is_empty());
        drop(mutator);
        assert!(!path.exists());
    }

    #[test]
    fn removal_open_blocks_new_writers_and_renames() {
        let root = tempfile::tempdir().expect("fixture");
        let directory = directory(&root.path().join("source"));
        let source_path = directory.path().join("leaf");
        fs::write(&source_path, b"source").expect("source body");
        let root_handle = directory.root_handle();
        let held = open_regular_for_removal(&root_handle, Path::new("leaf"), 64, directory.path())
            .expect("retained removal handle");

        assert!(fs::rename(&source_path, directory.path().join("renamed")).is_err());
        assert!(OpenOptions::new().write(true).open(&source_path).is_err());
        drop(held);
    }
}
