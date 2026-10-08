use crate::Result;
use crate::store_dir::{
    MAX_DIRECTORY_REMOVAL_DEPTH, MAX_DIRECTORY_REMOVAL_ENTRIES, Publication, StoreEntry, err,
    ioerr, relative_components,
};
use cap_primitives::fs::{DirBuilderExt, OpenOptionsExt};
use cap_primitives::{ambient_authority, fs as cap_fs};
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static TEMP: AtomicU64 = AtomicU64::new(0);

fn full(label: &Path, relative: &Path) -> std::path::PathBuf {
    label.join(relative)
}
pub(crate) fn open(path: &Path) -> Result<File> {
    if !path.is_absolute() {
        return Err(err(path, "store directory path must be absolute"));
    }
    let root = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority())
        .map_err(|e| ioerr(path, e))?;
    let mut current = root;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                current = cap_fs::open_dir_nofollow(&current, Path::new(name)).map_err(|_| {
                    crate::store_dir::err(path, "store directory has a missing or unsafe component")
                })?
            }
            _ => {
                return Err(err(
                    path,
                    "store directory path contains an unsafe component",
                ));
            }
        }
    }
    validate_directory(&current, path)?;
    Ok(current)
}
pub(crate) fn validate_directory(file: &File, label: &Path) -> Result<()> {
    let meta = file.metadata().map_err(|e| ioerr(label, e))?;
    if !meta.is_dir() {
        return Err(err(label, "retained store handle is not a directory"));
    }
    Ok(())
}
pub(crate) fn normalize_directory_handle(file: File, label: &Path) -> Result<File> {
    operation_directory(&file, label)
}
fn directory(root: &File, relative: &Path, label: &Path) -> Result<File> {
    let components = relative_components(relative, true, label)?;
    let mut current =
        cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
    for c in components {
        current = cap_fs::open_dir_nofollow(&current, Path::new(c)).map_err(|_| {
            err(
                &full(label, relative),
                "store directory component is missing or unsafe",
            )
        })?;
    }
    validate_directory(&current, &full(label, relative))?;
    Ok(current)
}
pub(crate) fn open_directory(root: &File, relative: &Path, label: &Path) -> Result<File> {
    directory(root, relative, label)
}
pub(crate) fn create_directory_all(root: &File, relative: &Path, label: &Path) -> Result<File> {
    let components = relative_components(relative, true, label)?;
    let mut current =
        cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
    for c in components {
        match cap_fs::open_dir_nofollow(&current, Path::new(c)) {
            Ok(next) => current = next,
            Err(_) => {
                let mut options = cap_fs::DirOptions::new();
                options.mode(0o700);
                cap_fs::create_dir(&current, Path::new(c), &options)
                    .map_err(|e| ioerr(&full(label, relative), e))?;
                let next = cap_fs::open_dir_nofollow(&current, Path::new(c)).map_err(|_| {
                    err(&full(label, relative), "created store directory is unsafe")
                })?;
                restrict_new_private_directory(&next)?;
                sync_directory(&current, label)?;
                current = next;
            }
        }
        validate_directory(&current, &full(label, relative))?;
    }
    Ok(current)
}
pub(crate) fn create_directory(root: &File, relative: &Path, label: &Path) -> Result<File> {
    let (parent, leaf) = parent(root, relative, label)?;
    let path = full(label, relative);
    let mut options = cap_fs::DirOptions::new();
    options.mode(0o700);
    cap_fs::create_dir(&parent, Path::new(leaf), &options).map_err(|e| ioerr(&path, e))?;
    let created = cap_fs::open_dir_nofollow(&parent, Path::new(leaf))
        .map_err(|_| err(&path, "created store directory is unsafe"))?;
    restrict_new_private_directory(&created)?;
    sync_directory(&parent, label)?;
    Ok(created)
}
fn parent<'a>(
    root: &File,
    relative: &'a Path,
    label: &Path,
) -> Result<(File, &'a std::ffi::OsStr)> {
    let c = relative_components(relative, false, label)?;
    let leaf = *c.last().expect("nonempty");
    let mut parent =
        cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
    for part in &c[..c.len() - 1] {
        parent = cap_fs::open_dir_nofollow(&parent, Path::new(part))
            .map_err(|_| err(&full(label, relative), "store parent is missing or unsafe"))?;
    }
    Ok((parent, leaf))
}
fn open_leaf(
    parent: &File,
    leaf: &std::ffi::OsStr,
    flags: i32,
    mode: i32,
    label: &Path,
) -> Result<File> {
    let name =
        CString::new(leaf.as_encoded_bytes()).map_err(|_| err(label, "store leaf contains NUL"))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            mode,
        )
    };
    if fd < 0 {
        let cause = std::io::Error::last_os_error();
        if matches!(cause.raw_os_error(), Some(libc::ELOOP)) {
            return Err(err(label, "store leaf is a symlink or unsafe link"));
        }
        return Err(ioerr(label, cause));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn open_leaf_optional(parent: &File, leaf: &std::ffi::OsStr, label: &Path) -> Result<Option<File>> {
    let name =
        CString::new(leaf.as_encoded_bytes()).map_err(|_| err(label, "store leaf contains NUL"))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        let cause = std::io::Error::last_os_error();
        return if cause.kind() == std::io::ErrorKind::NotFound
            || matches!(cause.raw_os_error(), Some(libc::ENOENT))
        {
            Ok(None)
        } else if matches!(cause.raw_os_error(), Some(libc::ELOOP)) {
            Err(err(label, "store leaf is a symlink or unsafe link"))
        } else {
            Err(ioerr(label, cause))
        };
    }
    Ok(Some(unsafe { File::from_raw_fd(fd) }))
}
fn parent_optional<'a>(
    root: &File,
    relative: &'a Path,
    label: &Path,
) -> Result<Option<(File, &'a std::ffi::OsStr)>> {
    let c = relative_components(relative, false, label)?;
    let leaf = *c.last().unwrap();
    let mut parent =
        cap_fs::open_dir_nofollow(root, Path::new(".")).map_err(|e| ioerr(label, e))?;
    for part in &c[..c.len() - 1] {
        match cap_fs::open_dir_nofollow(&parent, Path::new(part)) {
            Ok(next) => parent = next,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(err(&full(label, relative), "store parent is unsafe")),
        }
    }
    Ok(Some((parent, leaf)))
}
fn safe_regular(file: &File, label: &Path) -> Result<std::fs::Metadata> {
    let m = file.metadata().map_err(|e| ioerr(label, e))?;
    if !m.is_file() || m.nlink() != 1 {
        return Err(err(label, "store leaf is not a single-link regular file"));
    }
    Ok(m)
}
fn same(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}
fn same_directory_identity(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    before.is_dir() && after.is_dir() && before.dev() == after.dev() && before.ino() == after.ino()
}
pub(crate) fn open_regular_read(
    root: &File,
    relative: &Path,
    max: u64,
    label: &Path,
) -> Result<File> {
    let (parent, leaf) = parent(root, relative, label)?;
    let p = full(label, relative);
    let file = open_leaf(&parent, leaf, libc::O_RDONLY, 0, &p)?;
    let m = safe_regular(&file, &p)?;
    if m.len() > max {
        return Err(err(&p, "store file exceeds byte limit"));
    }
    Ok(file)
}
pub(crate) fn open_regular_for_removal(
    root: &File,
    relative: &Path,
    max: u64,
    label: &Path,
) -> Result<File> {
    open_regular_read(root, relative, max, label)
}
pub(crate) fn read_optional(
    root: &File,
    relative: &Path,
    max: u64,
    label: &Path,
) -> Result<Option<Vec<u8>>> {
    let Some((parent, leaf)) = parent_optional(root, relative, label)? else {
        return Ok(None);
    };
    let p = full(label, relative);
    let Some(mut file) = open_leaf_optional(&parent, leaf, &p)? else {
        return Ok(None);
    };
    let before = safe_regular(&file, &p)?;
    if before.len() > max {
        return Err(err(&p, "store file exceeds byte limit"));
    }
    let mut data = Vec::with_capacity(before.len() as usize);
    Read::by_ref(&mut file)
        .take(max.saturating_add(1))
        .read_to_end(&mut data)
        .map_err(|e| ioerr(&p, e))?;
    let after = safe_regular(&file, &p)?;
    if data.len() as u64 > max || !same(&before, &after) || after.len() != data.len() as u64 {
        return Err(err(&p, "store file changed while it was read"));
    }
    Ok(Some(data))
}
pub(crate) fn create_private_file_raw(root: &File, leaf: &Path, label: &Path) -> Result<File> {
    let leaf = direct_leaf(leaf, label)?;
    let path = label.join(leaf);
    let file = open_leaf(
        root,
        leaf,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        0o600,
        &path,
    )?;
    ensure_private_regular(&file, &path)?;
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
            let file = open_leaf(root, leaf, libc::O_RDWR, 0, &path)?;
            if ensure_private_regular(&file, &path)?.len() != 0 {
                return Err(err(&path, "private gate file is not empty"));
            }
            Ok(file)
        }
        Err(error) => Err(error),
    }
}
fn ensure_private_regular(file: &File, label: &Path) -> Result<std::fs::Metadata> {
    let meta = safe_regular(file, label)?;
    let uid = unsafe { libc::geteuid() };
    if meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(err(label, "store leaf is not owner-private"));
    }
    Ok(meta)
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
    let source = direct_leaf(source_leaf, source_label)?;
    let destination = direct_leaf(destination_leaf, destination_label)?;
    let source_path = source_label.join(source);
    let destination_path = destination_label.join(destination);
    let source_parent_meta = source_parent
        .metadata()
        .map_err(|e| ioerr(source_label, e))?;
    let destination_parent_meta = destination_parent
        .metadata()
        .map_err(|e| ioerr(destination_label, e))?;
    if source_parent_meta.dev() != destination_parent_meta.dev() {
        return Err(err(
            &destination_path,
            "regular rename crosses filesystem volumes",
        ));
    }
    let source_file = open_leaf(source_parent, source, libc::O_RDONLY, 0, &source_path)?;
    let source_meta = safe_regular(&source_file, &source_path)?;
    match publication {
        Publication::CreateOnly => rename_regular_noreplace(
            source_parent,
            source,
            destination_parent,
            destination,
            &destination_path,
        )?,
        Publication::Replace => {
            let destination_file = open_leaf(
                destination_parent,
                destination,
                libc::O_RDONLY,
                0,
                &destination_path,
            )?;
            safe_regular(&destination_file, &destination_path)?;
            rename_regular_replace(
                source_parent,
                source,
                destination_parent,
                destination,
                &destination_path,
            )?;
        }
        Publication::Upsert => match rename_regular_noreplace(
            source_parent,
            source,
            destination_parent,
            destination,
            &destination_path,
        ) {
            Ok(()) => {}
            Err(error) if is_io_kind(&error, "already_exists") => {
                let destination_file = open_leaf(
                    destination_parent,
                    destination,
                    libc::O_RDONLY,
                    0,
                    &destination_path,
                )?;
                safe_regular(&destination_file, &destination_path)?;
                rename_regular_replace(
                    source_parent,
                    source,
                    destination_parent,
                    destination,
                    &destination_path,
                )?;
            }
            Err(error) => return Err(error),
        },
    }
    let published = open_leaf(
        destination_parent,
        destination,
        libc::O_RDONLY,
        0,
        &destination_path,
    )?;
    let published_meta = safe_regular(&published, &destination_path)?;
    if source_meta.dev() != published_meta.dev() || source_meta.ino() != published_meta.ino() {
        return Err(err(
            &destination_path,
            "published regular-file identity changed",
        ));
    }
    sync_directory(source_parent, source_label)?;
    sync_directory(destination_parent, destination_label)
}
fn rename_regular_replace(
    source_parent: &File,
    source: &std::ffi::OsStr,
    destination_parent: &File,
    destination: &std::ffi::OsStr,
    label: &Path,
) -> Result<()> {
    let source = CString::new(source.as_encoded_bytes())
        .map_err(|_| err(label, "store leaf contains NUL"))?;
    let destination = CString::new(destination.as_encoded_bytes())
        .map_err(|_| err(label, "store leaf contains NUL"))?;
    if unsafe {
        libc::renameat(
            source_parent.as_raw_fd(),
            source.as_ptr(),
            destination_parent.as_raw_fd(),
            destination.as_ptr(),
        )
    } == 0
    {
        Ok(())
    } else {
        Err(ioerr(label, std::io::Error::last_os_error()))
    }
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
    let (parent, leaf) = parent(root, relative, label)?;
    let p = full(label, relative);
    let mut f = open_leaf(&parent, leaf, libc::O_WRONLY | libc::O_APPEND, 0, &p)?;
    let _ = safe_regular(&f, &p)?;
    f.write_all(bytes).map_err(|e| ioerr(&p, e))?;
    f.sync_data().map_err(|e| ioerr(&p, e))?;
    Ok(())
}
pub(crate) fn remove_file(root: &File, relative: &Path, label: &Path) -> Result<()> {
    let (parent, leaf) = parent(root, relative, label)?;
    let p = full(label, relative);
    let f = open_leaf(&parent, leaf, libc::O_RDONLY, 0, &p)?;
    let _ = safe_regular(&f, &p)?;
    drop(f);
    cap_fs::remove_file(&parent, Path::new(leaf)).map_err(|e| ioerr(&p, e))?;
    sync_directory(&parent, label)
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
    let source_directory = cap_fs::open_dir_nofollow(&parent, Path::new(source_leaf))
        .map_err(|_| err(&source_path, "source directory is missing or unsafe"))?;
    validate_directory(&source_directory, &source_path)?;
    rename_directory_noreplace(
        &parent,
        source_leaf,
        &parent,
        destination_leaf,
        &source_path,
    )?;
    let published =
        cap_fs::open_dir_nofollow(&parent, Path::new(destination_leaf)).map_err(|_| {
            err(
                &destination_path,
                "published directory is missing or unsafe",
            )
        })?;
    if !same_directory_identity(
        &source_directory
            .metadata()
            .map_err(|error| ioerr(&source_path, error))?,
        &published
            .metadata()
            .map_err(|error| ioerr(&destination_path, error))?,
    ) {
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
    validate_directory(source_parent, source_label)?;
    validate_directory(destination_parent, destination_label)?;
    let source_parent_meta = source_parent
        .metadata()
        .map_err(|e| ioerr(source_label, e))?;
    let destination_parent_meta = destination_parent
        .metadata()
        .map_err(|e| ioerr(destination_label, e))?;
    if source_parent_meta.dev() != destination_parent_meta.dev() {
        return Err(err(
            destination_label,
            "directory rename crosses filesystem volumes",
        ));
    }
    let source_directory = cap_fs::open_dir_nofollow(source_parent, Path::new(source))
        .map_err(|_| err(source_label, "source directory is missing or unsafe"))?;
    validate_directory(&source_directory, source_label)?;
    rename_directory_noreplace(
        source_parent,
        source,
        destination_parent,
        destination,
        source_label,
    )?;
    let published =
        cap_fs::open_dir_nofollow(destination_parent, Path::new(destination)).map_err(|_| {
            err(
                destination_label,
                "published directory is missing or unsafe",
            )
        })?;
    if !same_directory_identity(
        &source_directory
            .metadata()
            .map_err(|e| ioerr(source_label, e))?,
        &published
            .metadata()
            .map_err(|e| ioerr(destination_label, e))?,
    ) {
        return Err(err(
            destination_label,
            "published directory identity changed",
        ));
    }
    sync_directory(source_parent, source_label)?;
    sync_directory(destination_parent, destination_label)
}
fn direct_leaf<'a>(path: &'a Path, label: &Path) -> Result<&'a std::ffi::OsStr> {
    let components = relative_components(path, false, label)?;
    if components.len() != 1 {
        return Err(err(label, "directory rename requires one direct leaf"));
    }
    Ok(components[0])
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
                let file = open_leaf(directory, &name, libc::O_RDONLY, 0, &child_label)?;
                safe_regular(&file, &child_label)?;
                drop(file);
                cap_fs::remove_file(directory, Path::new(&name))
                    .map_err(|error| ioerr(&child_label, error))?;
            }
        }
    }
    sync_directory(directory, label)
}
#[cfg(target_os = "macos")]
pub(crate) fn rename_regular_noreplace(
    source_parent: &File,
    source: &std::ffi::OsStr,
    destination_parent: &File,
    destination: &std::ffi::OsStr,
    label: &Path,
) -> Result<()> {
    let source = CString::new(source.as_encoded_bytes())
        .map_err(|_| err(label, "store leaf contains NUL"))?;
    let destination = CString::new(destination.as_encoded_bytes())
        .map_err(|_| err(label, "store leaf contains NUL"))?;
    unsafe extern "C" {
        fn renameatx_np(
            fromfd: libc::c_int,
            from: *const libc::c_char,
            tofd: libc::c_int,
            to: *const libc::c_char,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }
    if unsafe {
        renameatx_np(
            source_parent.as_raw_fd(),
            source.as_ptr(),
            destination_parent.as_raw_fd(),
            destination.as_ptr(),
            0x0000_0004,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(ioerr(label, std::io::Error::last_os_error()))
    }
}
#[cfg(target_os = "linux")]
pub(crate) fn rename_regular_noreplace(
    source_parent: &File,
    source: &std::ffi::OsStr,
    destination_parent: &File,
    destination: &std::ffi::OsStr,
    label: &Path,
) -> Result<()> {
    let source = CString::new(source.as_encoded_bytes())
        .map_err(|_| err(label, "store leaf contains NUL"))?;
    let destination = CString::new(destination.as_encoded_bytes())
        .map_err(|_| err(label, "store leaf contains NUL"))?;
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_parent.as_raw_fd(),
            source.as_ptr(),
            destination_parent.as_raw_fd(),
            destination.as_ptr(),
            1_u32,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(ioerr(label, std::io::Error::last_os_error()))
    }
}
#[cfg(target_os = "macos")]
fn rename_directory_noreplace(
    source_parent: &File,
    source: &std::ffi::OsStr,
    destination_parent: &File,
    destination: &std::ffi::OsStr,
    label: &Path,
) -> Result<()> {
    let source = CString::new(source.as_encoded_bytes())
        .map_err(|_| err(label, "directory name contains NUL"))?;
    let destination = CString::new(destination.as_encoded_bytes())
        .map_err(|_| err(label, "directory name contains NUL"))?;
    unsafe extern "C" {
        fn renameatx_np(
            fromfd: libc::c_int,
            from: *const libc::c_char,
            tofd: libc::c_int,
            to: *const libc::c_char,
            flags: libc::c_uint,
        ) -> libc::c_int;
    }
    if unsafe {
        renameatx_np(
            source_parent.as_raw_fd(),
            source.as_ptr(),
            destination_parent.as_raw_fd(),
            destination.as_ptr(),
            0x0000_0004,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(ioerr(label, std::io::Error::last_os_error()))
    }
}
#[cfg(target_os = "linux")]
fn rename_directory_noreplace(
    source_parent: &File,
    source: &std::ffi::OsStr,
    destination_parent: &File,
    destination: &std::ffi::OsStr,
    label: &Path,
) -> Result<()> {
    let source = CString::new(source.as_encoded_bytes())
        .map_err(|_| err(label, "directory name contains NUL"))?;
    let destination = CString::new(destination.as_encoded_bytes())
        .map_err(|_| err(label, "directory name contains NUL"))?;
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_parent.as_raw_fd(),
            source.as_ptr(),
            destination_parent.as_raw_fd(),
            destination.as_ptr(),
            1_u32,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(ioerr(label, std::io::Error::last_os_error()))
    }
}
pub(crate) fn ensure_owner_private(root: &File, relative: &Path, label: &Path) -> Result<()> {
    let (parent, leaf) = parent(root, relative, label)?;
    let p = full(label, relative);
    let f = open_leaf(&parent, leaf, libc::O_RDONLY, 0, &p)?;
    let m = safe_regular(&f, &p)?;
    let uid = unsafe { libc::geteuid() };
    if m.uid() != uid || m.mode() & 0o077 != 0 {
        return Err(err(&p, "store leaf is not owner-private"));
    }
    Ok(())
}
pub(crate) fn entries(root: &File, relative: &Path, label: &Path) -> Result<Vec<StoreEntry>> {
    let d = directory(root, relative, label)?;
    let mut v = Vec::new();
    for entry in cap_fs::read_dir(&d, Path::new(".")).map_err(|e| ioerr(label, e))? {
        let entry = entry.map_err(|e| ioerr(label, e))?;
        let name = entry.file_name();
        let mut opts = cap_fs::OpenOptions::new();
        opts.read(true)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        opts.custom_flags(libc::O_NONBLOCK);
        let child = cap_fs::open(&d, Path::new(&name), &opts)
            .map_err(|_| err(label, "store entry is unsafe"))?;
        let m = child.metadata().map_err(|e| ioerr(label, e))?;
        if !(m.is_dir() || (m.is_file() && m.nlink() == 1)) {
            return Err(err(label, "store entry is unsafe"));
        }
        v.push(StoreEntry {
            name,
            is_directory: m.is_dir(),
            is_regular_file: m.is_file(),
        });
    }
    v.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(v)
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
            Err(_) => return Err(err(&full(label, relative), "store directory is unsafe")),
        }
    }
    entries(root, relative, label).map(Some)
}
pub(crate) fn sync_directory(d: &File, label: &Path) -> Result<()> {
    let operation = operation_directory(d, label)?;
    operation.sync_all().map_err(|e| ioerr(label, e))
}
pub(crate) fn restrict_new_private_directory(d: &File) -> Result<()> {
    let operation = operation_directory(d, Path::new("<retained>"))?;
    let m = operation
        .metadata()
        .map_err(|e| ioerr(Path::new("<retained>"), e))?;
    if !m.is_dir() {
        return Err(err(
            Path::new("<retained>"),
            "new private object is not a directory",
        ));
    }
    let mut perms = m.permissions();
    perms.set_mode(0o700);
    operation
        .set_permissions(perms)
        .map_err(|e| ioerr(Path::new("<retained>"), e))
}

/// Obtain an operation-capable duplicate of a retained directory without
/// resolving its diagnostic pathname. Linux may expose `open_dir_nofollow`
/// handles as O_PATH, which can be inspected but cannot be chmod'ed or
/// fsync'ed. Opening `.` relative to that descriptor retains the same
/// namespace authority while yielding a normal directory descriptor.
fn operation_directory(held: &File, label: &Path) -> Result<File> {
    let before = held.metadata().map_err(|e| ioerr(label, e))?;
    if !before.is_dir() {
        return Err(err(label, "retained store handle is not a directory"));
    }
    let dot = CString::new(".").expect("fixed directory component");
    // SAFETY: `held` is retained by the caller and `dot` is a fixed,
    // NUL-terminated single component. O_NOFOLLOW prevents a substitution
    // through a link while O_DIRECTORY requires a directory result.
    let fd = unsafe {
        libc::openat(
            held.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(ioerr(label, std::io::Error::last_os_error()));
    }
    // SAFETY: successful openat returned exactly one owned descriptor.
    let operation = unsafe { File::from_raw_fd(fd) };
    let after = operation.metadata().map_err(|e| ioerr(label, e))?;
    if !same_directory_identity(&before, &after) {
        return Err(err(label, "retained store directory identity changed"));
    }
    Ok(operation)
}

#[cfg(test)]
mod tests {
    use super::same_directory_identity;
    use std::fs;

    #[test]
    fn directory_identity_ignores_namespace_metadata_but_rejects_other_objects() {
        let first = tempfile::tempdir().unwrap();
        let before = fs::metadata(first.path()).unwrap();
        fs::write(first.path().join("child"), b"namespace mutation").unwrap();
        let after = fs::metadata(first.path()).unwrap();
        assert!(same_directory_identity(&before, &after));

        let second = tempfile::tempdir().unwrap();
        let other_directory = fs::metadata(second.path()).unwrap();
        assert!(!same_directory_identity(&before, &other_directory));

        let file_path = first.path().join("regular");
        fs::write(&file_path, b"regular").unwrap();
        let regular = fs::metadata(file_path).unwrap();
        assert!(!same_directory_identity(&before, &regular));
    }
}
