use std::{
    ffi::{CString, OsString},
    fs::File,
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::MetadataExt,
        },
    },
    path::{Component, Path, PathBuf},
};

use cap_primitives::{ambient_authority, fs as cap_fs};

use crate::Result;
use crate::private_fs::{oversized_file, raced_file, unsafe_file};
use crate::store_dir::StoreDirectory;

#[cfg(target_os = "macos")]
use crate::platform::private_acl as macos_acl;

/// Recognize the raw spelling before `Path::components` can normalize an
/// alias such as `/dev//fd` into an apparent descriptor root.
fn inherited_root_parts(path: &Path) -> Result<Option<(i32, Vec<OsString>)>> {
    let raw = path.as_os_str().as_bytes();
    const PREFIX: &[u8] = b"/dev/fd/";
    if !raw.starts_with(PREFIX) {
        // A component-normalized spelling which names `/dev/fd` is never
        // allowed to fall through to the descriptor capability bridge.
        // Inspect it only after rejecting the raw prefix so normalization
        // cannot grant authority to an alias.
        let mut components = path.components();
        let names_descriptor_root = components.next() == Some(Component::RootDir)
            && matches!(components.next(), Some(Component::Normal(name)) if name.as_bytes() == b"dev")
            && matches!(components.next(), Some(Component::Normal(name)) if name.as_bytes() == b"fd");
        if names_descriptor_root {
            return Err(unsafe_file(
                "inherited private directory path is not canonical",
                Some(path),
            ));
        }
        return Ok(None);
    }
    let remainder = &raw[PREFIX.len()..];
    if remainder.is_empty() || remainder.starts_with(b"/") || remainder.ends_with(b"/") {
        return Err(unsafe_file(
            "inherited private directory path is not canonical",
            Some(path),
        ));
    }
    let mut parts = remainder.split(|byte| *byte == b'/');
    let descriptor = parts.next().expect("nonempty inherited descriptor");
    if descriptor.is_empty()
        || !descriptor.iter().all(u8::is_ascii_digit)
        || (descriptor.len() > 1 && descriptor[0] == b'0')
    {
        return Err(unsafe_file(
            "inherited private directory descriptor is invalid",
            Some(path),
        ));
    }
    let fd = std::str::from_utf8(descriptor)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|fd| *fd >= 0)
        .ok_or_else(|| {
            unsafe_file(
                "inherited private directory descriptor is invalid",
                Some(path),
            )
        })?;
    let mut suffix = Vec::new();
    for component in parts {
        if component.is_empty() || component == b"." || component == b".." {
            return Err(unsafe_file(
                "inherited private directory path is not canonical",
                Some(path),
            ));
        }
        suffix.push(OsString::from_vec(component.to_vec()));
    }
    Ok(Some((fd, suffix)))
}

/// Duplicate the inherited descriptor before treating it as authority.
fn duplicate_inherited_private_root(fd: i32, path: &Path) -> Result<File> {
    // SAFETY: fcntl receives an integer descriptor supplied by the caller;
    // F_DUPFD_CLOEXEC returns a new descriptor or -1 and has no aliasing
    // requirements.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(unsafe_file(
            "cannot duplicate inherited private directory descriptor",
            Some(path),
        ));
    }
    // SAFETY: F_DUPFD_CLOEXEC returned one newly owned descriptor.
    Ok(unsafe { File::from_raw_fd(duplicate) })
}

pub(crate) fn resolve_inherited_private_root(
    path: &Path,
    control: crate::control::ControlHandle,
) -> Result<Option<(StoreDirectory, Vec<OsString>)>> {
    let Some((fd, suffix)) = inherited_root_parts(path)? else {
        return Ok(None);
    };
    let root = duplicate_inherited_private_root(fd, path)?;
    verify_directory(&root, path)?;
    let logical = PathBuf::from(format!("/dev/fd/{fd}"));
    Ok(Some((
        StoreDirectory::from_retained_with_control(root, logical, control)?,
        suffix,
    )))
}

pub(crate) fn verify_private_directory_handle(directory: &StoreDirectory) -> Result<()> {
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    let handle = directory.root_handle();
    verify_private_parent(&handle, uid, directory.path())
}

pub(crate) fn verify_private_directory(
    path: &Path,
    control: crate::control::ControlHandle,
) -> Result<StoreDirectory> {
    if let Some((root, suffix)) = resolve_inherited_private_root(path, control.clone())? {
        return inherited_private_directory(root, &suffix, path, false);
    }
    let components = path_components(path)?;
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    let root = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority()).map_err(|_| {
        unsafe_file(
            "cannot open filesystem root for private directory",
            Some(path),
        )
    })?;
    let mut directory = root;
    for component in &components {
        let next = cap_fs::open_dir_nofollow(&directory, Path::new(component)).map_err(|_| {
            unsafe_file(
                "private directory has a missing, symlinked, or unsafe component",
                Some(path),
            )
        })?;
        directory = next;
    }
    verify_private_parent(&directory, uid, path)?;
    StoreDirectory::from_retained_with_control(directory, path.to_path_buf(), control)
}

pub(crate) fn verify_private_creation_parent(
    path: &Path,
    control: crate::control::ControlHandle,
) -> Result<StoreDirectory> {
    if let Some((root, suffix)) = resolve_inherited_private_root(path, control.clone())? {
        return inherited_private_directory(root, &suffix, path, true);
    }
    let components = path_components(path)?;
    let root = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority()).map_err(|_| {
        unsafe_file(
            "cannot open filesystem root for private directory creation",
            Some(path),
        )
    })?;
    let mut directory = root;
    for component in &components {
        let next = cap_fs::open_dir_nofollow(&directory, Path::new(component)).map_err(|_| {
            unsafe_file(
                "private directory creation parent has a missing, symlinked, or unsafe component",
                Some(path),
            )
        })?;
        directory = next;
    }
    verify_directory(&directory, path)?;
    StoreDirectory::from_retained_with_control(directory, path.to_path_buf(), control)
}

pub(crate) fn read_private_file(
    path: &Path,
    max_bytes: u64,
    control: crate::control::ControlHandle,
) -> Result<Vec<u8>> {
    if let Some((root, suffix)) = resolve_inherited_private_root(path, control.clone())? {
        return read_inherited_private_file(&root, &suffix, path, max_bytes);
    }
    let components = path_components(path)?;
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    let root = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority()).map_err(|_| {
        unsafe_file(
            "cannot open filesystem root for private trust file",
            Some(path),
        )
    })?;

    let mut parent = root;
    for component in &components[..components.len() - 1] {
        let next = cap_fs::open_dir_nofollow(&parent, Path::new(component)).map_err(|_| {
            unsafe_file(
                "private trust file has a missing, symlinked, or unsafe ancestor",
                Some(path),
            )
        })?;
        parent = next;
    }
    read_leaf(
        &parent,
        components.last().expect("nonempty components"),
        path,
        max_bytes,
        uid,
    )
}

pub(crate) fn read_private_file_at(
    parent: &StoreDirectory,
    name: &str,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    let leaf = direct_leaf(name, parent.path())?;
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    let handle = parent.root_handle();
    verify_directory(&handle, parent.path())?;
    let label = parent.path().join(leaf);
    read_leaf(&handle, leaf, &label, max_bytes, uid)
}

fn inherited_private_directory(
    root: StoreDirectory,
    suffix: &[OsString],
    path: &Path,
    creation_parent: bool,
) -> Result<StoreDirectory> {
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut directory = root.root_handle();
    for component in suffix {
        let next = cap_fs::open_dir_nofollow(&directory, Path::new(component)).map_err(|_| {
            unsafe_file(
                "inherited private directory has a missing, symlinked, or unsafe component",
                Some(path),
            )
        })?;
        directory = std::sync::Arc::new(next);
    }
    if creation_parent {
        verify_directory(&directory, path)?;
    } else {
        verify_private_parent(&directory, uid, path)?;
    }
    let logical = path.to_path_buf();
    let directory = duplicate_inherited_private_root(directory.as_raw_fd(), path)?;
    StoreDirectory::from_retained_with_control(directory, logical, root.control().clone())
}

fn read_inherited_private_file(
    root: &StoreDirectory,
    suffix: &[OsString],
    path: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    let (leaf, parents) = suffix
        .split_last()
        .ok_or_else(|| unsafe_file("inherited private trust file has no leaf", Some(path)))?;
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut parent = root.root_handle();
    for component in parents {
        let next = cap_fs::open_dir_nofollow(&parent, Path::new(component)).map_err(|_| {
            unsafe_file(
                "inherited private trust file has a missing, symlinked, or unsafe ancestor",
                Some(path),
            )
        })?;
        parent = std::sync::Arc::new(next);
    }
    read_leaf(&parent, leaf, path, max_bytes, uid)
}

fn direct_leaf<'a>(name: &'a str, label: &Path) -> Result<&'a std::ffi::OsStr> {
    let path = Path::new(name);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(leaf)), None) => Ok(leaf),
        _ => Err(unsafe_file(
            "private trust file leaf must be one normal component",
            Some(label),
        )),
    }
}

fn metadata(file: &File, path: &Path) -> Result<std::fs::Metadata> {
    file.metadata()
        .map_err(|_| unsafe_file("cannot inspect private trust file component", Some(path)))
}

fn verify_directory(file: &File, path: &Path) -> Result<()> {
    if !metadata(file, path)?.is_dir() {
        return Err(unsafe_file(
            "private path component is not a directory",
            Some(path),
        ));
    }
    Ok(())
}

fn verify_private_parent(file: &File, uid: u32, path: &Path) -> Result<()> {
    let meta = metadata(file, path)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(unsafe_file(
            "private trust file parent is not owner-only",
            Some(path),
        ));
    }
    #[cfg(target_os = "macos")]
    macos_acl::verify(file, uid, macos_acl::Scope::PrivateParent, path)?;
    Ok(())
}

fn read_leaf(
    parent: &File,
    leaf: &std::ffi::OsStr,
    path: &Path,
    max_bytes: u64,
    uid: u32,
) -> Result<Vec<u8>> {
    let leaf = CString::new(leaf.as_encoded_bytes())
        .map_err(|_| unsafe_file("private trust file leaf contains NUL", Some(path)))?;
    // SAFETY: `parent` is a retained directory capability and `leaf` is a
    // single NUL-free path component. O_NOFOLLOW prevents the final leaf
    // from being resolved as a symlink.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(unsafe_file("cannot open private trust file", Some(path)));
    }
    // SAFETY: successful openat transferred exactly one owned descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let before = file
        .metadata()
        .map_err(|_| unsafe_file("cannot inspect private trust file", Some(path)))?;
    verify_private_leaf(&before, uid, path)?;
    #[cfg(target_os = "macos")]
    macos_acl::verify(&file, uid, macos_acl::Scope::PrivateLeaf, path)?;
    if before.len() > max_bytes {
        return Err(oversized_file(max_bytes, before.len(), path));
    }
    let capacity =
        usize::try_from(before.len()).map_err(|_| oversized_file(max_bytes, before.len(), path))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| unsafe_file("cannot read private trust file", Some(path)))?;
    if bytes.len() as u64 > max_bytes {
        return Err(oversized_file(max_bytes, bytes.len() as u64, path));
    }
    let after = file
        .metadata()
        .map_err(|_| unsafe_file("cannot recheck private trust file", Some(path)))?;
    verify_private_leaf(&after, uid, path)?;
    #[cfg(target_os = "macos")]
    macos_acl::verify(&file, uid, macos_acl::Scope::PrivateLeaf, path)?;
    if !same_file(&before, &after) || after.len() != bytes.len() as u64 {
        return Err(raced_file(path));
    }
    Ok(bytes)
}

pub(crate) fn verify_private_regular_file_handle(file: &File) -> Result<()> {
    verify_owner_private_regular_handle(file, Path::new("<retained>"))
}

pub(super) fn verify_owner_private_regular_handle(file: &File, path: &Path) -> Result<()> {
    let uid = unsafe { libc::geteuid() };
    verify_private_leaf(&metadata(file, path)?, uid, path)?;
    #[cfg(target_os = "macos")]
    macos_acl::verify(file, uid, macos_acl::Scope::PrivateLeaf, path)?;
    Ok(())
}

fn verify_private_leaf(meta: &std::fs::Metadata, uid: u32, path: &Path) -> Result<()> {
    if !meta.is_file() || meta.nlink() != 1 || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(unsafe_file(
            "private trust file is not an owner-only single-link regular file",
            Some(path),
        ));
    }
    Ok(())
}

fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

pub(crate) fn verify_trusted_owned_regular_file_handle(file: &File) -> Result<()> {
    let path = Path::new("<retained-native-definition>");
    let uid = unsafe { libc::geteuid() };
    let meta = metadata(file, path)?;
    if !meta.is_file() || meta.nlink() != 1 || meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err(unsafe_file(
            "trusted leaf is not an owner-controlled single-link regular file",
            Some(path),
        ));
    }
    #[cfg(target_os = "macos")]
    macos_acl::verify(file, uid, macos_acl::Scope::ExecutableLeaf, path)?;
    Ok(())
}

pub(crate) fn verify_trusted_executable(path: &Path) -> Result<()> {
    let uid = unsafe { libc::geteuid() };
    let components = path_components(path)?;
    let root = cap_fs::open_ambient_dir(Path::new("/"), ambient_authority())
        .map_err(|_| unsafe_file("cannot open scheduler executable root", Some(path)))?;
    verify_executable_ancestor(&root, uid, path)?;
    let mut parent = root;
    for component in &components[..components.len() - 1] {
        let next = cap_fs::open_dir_nofollow(&parent, Path::new(component)).map_err(|_| {
            unsafe_file(
                "scheduler executable has a missing, symlinked, or unsafe ancestor",
                Some(path),
            )
        })?;
        verify_executable_ancestor(&next, uid, path)?;
        parent = next;
    }
    let leaf = CString::new(
        components
            .last()
            .expect("nonempty components")
            .as_encoded_bytes(),
    )
    .map_err(|_| unsafe_file("scheduler executable leaf contains NUL", Some(path)))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(unsafe_file("cannot open scheduler executable", Some(path)));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = metadata(&file, path)?;
    if !metadata.is_file()
        || (metadata.uid() != uid && metadata.uid() != 0)
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o111 == 0
    {
        return Err(unsafe_file(
            "scheduler executable is not a trusted executable regular file",
            Some(path),
        ));
    }
    #[cfg(target_os = "macos")]
    macos_acl::verify(&file, uid, macos_acl::Scope::ExecutableLeaf, path)?;
    Ok(())
}

fn verify_executable_ancestor(file: &File, uid: u32, path: &Path) -> Result<()> {
    let metadata = metadata(file, path)?;
    let safe_root_sticky = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
    if !metadata.is_dir()
        || (metadata.uid() != uid && metadata.uid() != 0)
        || (metadata.mode() & 0o022 != 0 && !safe_root_sticky)
    {
        return Err(unsafe_file(
            "scheduler executable ancestor is writable by an untrusted principal",
            Some(path),
        ));
    }
    #[cfg(target_os = "macos")]
    macos_acl::verify(file, uid, macos_acl::Scope::ExecutableAncestor, path)?;
    Ok(())
}

fn path_components(path: &Path) -> Result<Vec<&std::ffi::OsStr>> {
    if !path.is_absolute() {
        return Err(unsafe_file(
            "private trust file path must be absolute",
            Some(path),
        ));
    }
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => components.push(value),
            // Prefixes are handled by the Windows implementation before it
            // calls this helper. On Unix these components cannot occur in an
            // absolute path, and accepting `.` or `..` would defeat the
            // component-by-component security policy.
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => {
                return Err(unsafe_file(
                    "private trust file path contains an unsafe component",
                    Some(path),
                ));
            }
        }
    }
    if components.is_empty() {
        return Err(unsafe_file(
            "private trust file path has no file leaf",
            Some(path),
        ));
    }
    Ok(components)
}
