//! Strict, create-only publication and consumption of persona artifacts.
//!
//! This module deliberately retains the directory descriptor used to bind an
//! artifact.  Public paths are rechecked as diagnostics only; all file I/O is
//! descriptor-relative and no-follow.

use cap_primitives::{ambient_authority, fs as cap_fs};
use kio_core::cas::hash_bytes;
use std::{
    fs,
    io::{Read, Seek, Write},
    path::{Component, Path, PathBuf},
};
use thiserror::Error;

const TEMP_PREFIX: &str = ".kio-persona-artifact-";
const MAX_PATH_COMPONENTS: usize = 64;
const MAX_PATH_COMPONENT_BYTES: usize = 255;

#[derive(Debug, Error)]
pub enum PersonaArtifactError {
    #[error("unsafe persona artifact: {0}")]
    Unsafe(String),
    #[error("persona artifact I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("persona artifact publication is indeterminate: {0}")]
    Indeterminate(String),
    #[error("persona artifact output already exists: {0}")]
    AlreadyExists(PathBuf),
}

#[derive(Debug)]
struct BoundParent {
    handle: fs::File,
    identity: cap_fs::Metadata,
    public: PathBuf,
    leaf: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Observation {
    identity: FileIdentity,
    len: u64,
    hash: String,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    nlink: u64,
    #[cfg(unix)]
    mtime: i64,
    #[cfg(unix)]
    mtime_nsec: i64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
}

/// A descriptor-bound observation of one strict artifact file.
///
/// It retains the parent directory descriptor and initial file identity, so a
/// later recheck rejects replacement even when replacement bytes are identical.
#[derive(Debug)]
pub struct StrictArtifact {
    parent: BoundParent,
    file: fs::File,
    path: PathBuf,
    maximum: usize,
    bytes: Vec<u8>,
    observation: Observation,
}

impl StrictArtifact {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Re-read through the retained descriptor and require the original file
    /// identity, length, hash, and bytes to remain exact.
    pub fn recheck(&self) -> Result<(), PersonaArtifactError> {
        recheck_parent(&self.parent)?;
        let (retained_bytes, retained_observation) =
            observe_open_file(&self.file, self.maximum, &self.path)?;
        if retained_bytes != self.bytes || retained_observation != self.observation {
            return unsafe_state("retained artifact descriptor changed after initial observation");
        }
        let (_file, bytes, observation) = observe_regular(
            &self.parent.handle,
            &self.parent.leaf,
            self.maximum,
            &self.path,
        )?;
        if bytes != self.bytes || observation != self.observation {
            return unsafe_state("artifact changed after initial observation");
        }
        recheck_parent(&self.parent)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    volume: u32,
    #[cfg(windows)]
    index: u64,
}

/// Read a bounded, single-link UTF-8 regular artifact through a retained,
/// no-follow path walk.  Inputs must be absolute and lexically normalized.
pub fn read_strict(path: &Path, maximum: usize) -> Result<Vec<u8>, PersonaArtifactError> {
    Ok(bind_strict(path, maximum)?.bytes)
}

/// Bind a strict artifact for an exact identity recheck before later mutation.
pub fn bind_strict(path: &Path, maximum: usize) -> Result<StrictArtifact, PersonaArtifactError> {
    let parent = bind_parent(path)?;
    recheck_parent(&parent)?;
    let (file, bytes, observation) = observe_regular(&parent.handle, &parent.leaf, maximum, path)?;
    recheck_parent(&parent)?;
    Ok(StrictArtifact {
        parent,
        file,
        path: path.to_owned(),
        maximum,
        bytes,
        observation,
    })
}

/// Bind a strict leaf below an already-retained, no-follow directory.
///
/// Callers which need a multi-file transaction use this to ensure every leaf
/// is observed beneath one root descriptor, rather than independently walking
/// public paths that could be swapped between observations. `root_path` is the
/// canonical public spelling of that retained directory and is used solely for
/// the diagnostic rewalk performed by [`StrictArtifact::recheck`].
pub fn bind_strict_at(
    root: &fs::File,
    root_path: &Path,
    leaf: &str,
    maximum: usize,
) -> Result<StrictArtifact, PersonaArtifactError> {
    if !root_path.is_absolute()
        || root_path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || !safe_leaf(leaf)
    {
        return unsafe_state("strict artifact root or leaf is unsafe");
    }
    let public = normalize_persona_path(root_path)?;
    if public != root_path {
        return unsafe_state("strict artifact root must use canonical alias spelling");
    }
    let identity = cap_fs::Metadata::from_file(root)?;
    if !identity.is_dir() || identity.file_type().is_symlink() {
        return unsafe_state("strict artifact root is not a real directory");
    }
    let parent = BoundParent {
        handle: root.try_clone()?,
        identity,
        public,
        leaf: leaf.to_owned(),
    };
    recheck_parent(&parent)?;
    let path = parent.public.join(leaf);
    let (file, bytes, observation) = observe_regular(&parent.handle, leaf, maximum, &path)?;
    recheck_parent(&parent)?;
    Ok(StrictArtifact {
        parent,
        file,
        path,
        maximum,
        bytes,
        observation,
    })
}

/// Publish canonical bytes once.  The destination is never overwritten and
/// success is returned only after an exact descriptor-bound readback.
pub fn publish_create_only(
    path: &Path,
    bytes: &[u8],
    maximum: usize,
) -> Result<PathBuf, PersonaArtifactError> {
    prepare_create_only(path, bytes, maximum)?.publish()
}

/// A staged, fsynced artifact whose no-replace publication has not begun.
/// Dropping it removes only the exact retained temporary inode.
pub(crate) struct PreparedArtifact<'a> {
    parent: BoundParent,
    temp: TempArtifact,
    staged: Observation,
    path: PathBuf,
    bytes: &'a [u8],
    maximum: usize,
}

/// Prepare a create-only artifact without making the destination visible.
/// Security boundaries which retain other input descriptors may perform their
/// final input rechecks after this returns and call [`PreparedArtifact::publish`]
/// only when those checks still succeed.
pub(crate) fn prepare_create_only<'a>(
    path: &Path,
    bytes: &'a [u8],
    maximum: usize,
) -> Result<PreparedArtifact<'a>, PersonaArtifactError> {
    if bytes.len() > maximum {
        return unsafe_state("artifact exceeds byte limit");
    }
    let parent = bind_parent(path)?;
    recheck_parent(&parent)?;
    validate_publication_parent(&parent)?;
    absent(&parent)?;

    let mut temp = TempArtifact::create(&parent)?;
    temp.file.write_all(bytes)?;
    temp.file.sync_all()?;
    let (_, _, staged) = observe_regular(&parent.handle, &temp.name, maximum, path)?;
    if staged.len != bytes.len() as u64 || staged.hash != hash_bytes(bytes) {
        return unsafe_state("staged artifact differs from requested bytes");
    }
    recheck_parent(&parent)?;
    Ok(PreparedArtifact {
        parent,
        temp,
        staged,
        path: path.to_owned(),
        bytes,
        maximum,
    })
}

impl PreparedArtifact<'_> {
    pub(crate) fn publish(mut self) -> Result<PathBuf, PersonaArtifactError> {
        recheck_parent(&self.parent)?;
        crate::scale_fixture::rename_noreplace(
            &self.parent.handle,
            &self.temp.name,
            &self.parent.handle,
            &self.parent.leaf,
        )
        .map_err(|error| {
            match cap_fs::stat(
                &self.parent.handle,
                Path::new(&self.parent.leaf),
                cap_fs::FollowSymlinks::No,
            ) {
                Ok(_) => {
                    PersonaArtifactError::AlreadyExists(self.parent.public.join(&self.parent.leaf))
                }
                Err(check) if check.kind() == std::io::ErrorKind::NotFound => {
                    PersonaArtifactError::Indeterminate(error.to_string())
                }
                Err(_) => PersonaArtifactError::Indeterminate(error.to_string()),
            }
        })?;
        self.temp.published = true;

        let verification = (|| {
            sync_parent(&self.parent)?;
            let (_, readback, published) = observe_regular(
                &self.parent.handle,
                &self.parent.leaf,
                self.maximum,
                &self.path,
            )?;
            if !same_across_rename(&self.staged, &published) || readback != self.bytes {
                return unsafe_state("published artifact differs from staged artifact");
            }
            recheck_parent(&self.parent)?;
            let (_, again, repeated) = observe_regular(
                &self.parent.handle,
                &self.parent.leaf,
                self.maximum,
                &self.path,
            )?;
            if repeated != published || again != readback {
                return unsafe_state("published artifact changed after readback");
            }
            Ok(self.parent.public.join(&self.parent.leaf))
        })();
        verification.map_err(|error| PersonaArtifactError::Indeterminate(error.to_string()))
    }
}

#[derive(Debug)]
struct TempArtifact {
    parent: fs::File,
    name: String,
    identity: cap_fs::Metadata,
    file: fs::File,
    published: bool,
}

impl TempArtifact {
    fn create(parent: &BoundParent) -> Result<Self, PersonaArtifactError> {
        for _ in 0..32 {
            let name = temporary_name()?;
            let mut options = cap_fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use cap_fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = match cap_fs::open(&parent.handle, Path::new(&name), &options) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            };
            let identity = cap_fs::Metadata::from_file(&file)?;
            if !private_temporary(&identity) {
                return unsafe_state(
                    "temporary artifact is not a private single-link regular file",
                );
            }
            return Ok(Self {
                parent: parent.handle.try_clone()?,
                name,
                identity,
                file,
                published: false,
            });
        }
        unsafe_state("exhausted secure artifact staging candidates")
    }
}

impl Drop for TempArtifact {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        let Ok(current) = cap_fs::stat(
            &self.parent,
            Path::new(&self.name),
            cap_fs::FollowSymlinks::No,
        ) else {
            return;
        };
        if regular(&current, usize::MAX) && same(&current, &self.identity) {
            let _ = cap_fs::remove_file(&self.parent, Path::new(&self.name));
        }
    }
}

fn bind_parent(path: &Path) -> Result<BoundParent, PersonaArtifactError> {
    if !path.is_absolute()
        || path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return unsafe_state("path must be absolute and lexically normalized");
    }
    #[cfg(not(windows))]
    if path
        .components()
        .any(|component| matches!(component, Component::Prefix(_)))
    {
        return unsafe_state("path must be absolute and lexically normalized");
    }
    let components = path.components().count();
    if components > MAX_PATH_COMPONENTS
        || path.components().any(|component| match component {
            Component::Normal(part) => part.len() > MAX_PATH_COMPONENT_BYTES,
            _ => false,
        })
    {
        return unsafe_state("path exceeds component bounds");
    }
    let path = normalize_persona_path(path)?;
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| safe_leaf(name))
        .ok_or_else(|| PersonaArtifactError::Unsafe("path has an unsafe final component".into()))?
        .to_owned();
    let public = path
        .parent()
        .ok_or_else(|| PersonaArtifactError::Unsafe("path has no parent directory".into()))?
        .to_owned();
    let mut components = public.components().peekable();
    let mut root = PathBuf::new();
    #[cfg(windows)]
    if let Some(Component::Prefix(prefix)) = components.peek().copied() {
        if !matches!(prefix.kind(), std::path::Prefix::Disk(_)) {
            return unsafe_state("path must use an ordinary drive prefix");
        }
        root.push(prefix.as_os_str());
        components.next();
    }
    if !matches!(components.next(), Some(Component::RootDir)) {
        return unsafe_state("path must have an absolute root");
    }
    root.push(std::path::MAIN_SEPARATOR_STR);
    let mut handle = cap_fs::open_ambient_dir(&root, ambient_authority())?;
    for component in components {
        let Component::Normal(part) = component else {
            return unsafe_state("path must be lexically normalized");
        };
        handle = cap_fs::open_dir_nofollow(&handle, Path::new(part))?;
        let metadata = cap_fs::Metadata::from_file(&handle)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return unsafe_state("parent component is not a real directory");
        }
    }
    let identity = cap_fs::Metadata::from_file(&handle)?;
    if !identity.is_dir() || identity.file_type().is_symlink() {
        return unsafe_state("artifact parent is not a real directory");
    }
    Ok(BoundParent {
        handle,
        identity,
        public,
        leaf,
    })
}

fn recheck_parent(parent: &BoundParent) -> Result<(), PersonaArtifactError> {
    let retained = cap_fs::Metadata::from_file(&parent.handle)?;
    // Rewalk every public ancestor without following links.  A plain metadata
    // lookup would follow an ancestor symlink that points back to the retained
    // directory and incorrectly preserve the apparent identity.
    let rebound = bind_parent(&parent.public.join(&parent.leaf))?;
    if !retained.is_dir()
        || retained.file_type().is_symlink()
        || !same(&parent.identity, &retained)
        || rebound.public != parent.public
        || rebound.leaf != parent.leaf
        || !same(&parent.identity, &rebound.identity)
    {
        return unsafe_state("artifact parent identity changed");
    }
    Ok(())
}

fn absent(parent: &BoundParent) -> Result<(), PersonaArtifactError> {
    match cap_fs::stat(
        &parent.handle,
        Path::new(&parent.leaf),
        cap_fs::FollowSymlinks::No,
    ) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(PersonaArtifactError::AlreadyExists(
            parent.public.join(&parent.leaf),
        )),
        Err(error) => Err(error.into()),
    }
}

fn observe_regular(
    parent: &fs::File,
    leaf: &str,
    maximum: usize,
    label: &Path,
) -> Result<(fs::File, Vec<u8>, Observation), PersonaArtifactError> {
    let before = cap_fs::stat(parent, Path::new(leaf), cap_fs::FollowSymlinks::No)?;
    if !regular(&before, maximum) {
        return unsafe_state(format!(
            "{} must be a bounded single-link regular file",
            label.display()
        ));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(parent, Path::new(leaf), &options)?;
    let opened = cap_fs::Metadata::from_file(&file)?;
    let after = cap_fs::stat(parent, Path::new(leaf), cap_fs::FollowSymlinks::No)?;
    if !regular(&opened, maximum)
        || !regular(&after, maximum)
        || !same_metadata(&before, &opened)
        || !same_metadata(&opened, &after)
    {
        return unsafe_state("artifact changed while opening");
    }
    let (bytes, observation) = observe_open_file(&file, maximum, label)?;
    let named = cap_fs::stat(parent, Path::new(leaf), cap_fs::FollowSymlinks::No)?;
    if !regular(&named, maximum) || !same_metadata(&opened, &named) {
        return unsafe_state("artifact changed after reading");
    }
    Ok((file, bytes, observation))
}

fn observe_open_file(
    file: &fs::File,
    maximum: usize,
    label: &Path,
) -> Result<(Vec<u8>, Observation), PersonaArtifactError> {
    let opened = cap_fs::Metadata::from_file(file)?;
    if !regular(&opened, maximum) {
        return unsafe_state(format!(
            "{} must remain a bounded single-link regular file",
            label.display()
        ));
    }
    let mut reader = file.try_clone()?;
    reader.seek(std::io::SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    let read_limit = u64::try_from(maximum)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| PersonaArtifactError::Unsafe("artifact byte limit is unsupported".into()))?;
    (&mut reader).take(read_limit).read_to_end(&mut bytes)?;
    let final_metadata = cap_fs::Metadata::from_file(file)?;
    if bytes.len() as u64 != opened.len()
        || !regular(&final_metadata, maximum)
        || !same_metadata(&opened, &final_metadata)
    {
        return unsafe_state("retained artifact changed while reading");
    }
    std::str::from_utf8(&bytes)
        .map_err(|_| PersonaArtifactError::Unsafe("artifact is not UTF-8".into()))?;
    let observation = Observation {
        identity: identity(&opened),
        len: opened.len(),
        hash: hash_bytes(&bytes),
        #[cfg(unix)]
        mode: mode(&opened),
        #[cfg(unix)]
        nlink: links(&opened),
        #[cfg(unix)]
        mtime: mtime(&opened),
        #[cfg(unix)]
        mtime_nsec: mtime_nsec(&opened),
        #[cfg(unix)]
        ctime: ctime(&opened),
        #[cfg(unix)]
        ctime_nsec: ctime_nsec(&opened),
    };
    Ok((bytes, observation))
}

fn sync_parent(parent: &BoundParent) -> Result<(), PersonaArtifactError> {
    recheck_parent(parent)?;

    #[cfg(unix)]
    {
        let mut options = cap_fs::OpenOptions::new();
        options
            .read(true)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        let syncable = cap_fs::open(&parent.handle, Path::new("."), &options)?;
        let opened = cap_fs::Metadata::from_file(&syncable)?;
        if !opened.is_dir() || opened.file_type().is_symlink() || !same(&parent.identity, &opened) {
            return unsafe_state("artifact parent changed while reopening for fsync");
        }
        syncable.sync_all()?;
        recheck_parent(parent)
    }

    // Windows has no directory fsync: an ordinary directory reopen is denied
    // and FlushFileBuffers cannot flush the resulting directory handle.  Keep
    // the fail-closed half of the POSIX operation by proving that the retained
    // capability remains a real, non-reparse directory before and after the
    // publication.  File data was synced before the rename; NTFS metadata
    // journaling supplies the weaker directory-entry durability guarantee.
    #[cfg(windows)]
    {
        kio_core::platform::directory_handle_identity(&parent.handle).ok_or_else(|| {
            PersonaArtifactError::Unsafe(
                "artifact parent must be a real directory, not a reparse point".into(),
            )
        })?;
        recheck_parent(parent)
    }

    #[cfg(not(any(unix, windows)))]
    {
        Err(PersonaArtifactError::Unsafe(
            "artifact parent durability is unsupported on this platform".into(),
        ))
    }
}

fn regular(metadata: &cap_fs::Metadata, maximum: usize) -> bool {
    metadata.file_type().is_file()
        && !metadata.file_type().is_symlink()
        && metadata.len() <= maximum as u64
        && links(metadata) == 1
}

fn safe_leaf(name: &str) -> bool {
    !name.is_empty() && Path::new(name).components().count() == 1 && !name.contains('\0')
}

fn temporary_name() -> Result<String, PersonaArtifactError> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| {
        PersonaArtifactError::Unsafe(format!("secure staging randomness unavailable: {error}"))
    })?;
    let name = format!(
        "{TEMP_PREFIX}{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    if safe_leaf(&name) {
        Ok(name)
    } else {
        unsafe_state("unsafe temporary artifact name")
    }
}

fn validate_publication_parent(parent: &BoundParent) -> Result<(), PersonaArtifactError> {
    let metadata = cap_fs::Metadata::from_file(&parent.handle)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return unsafe_state("artifact publication parent is not a real directory");
    }
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        let mode = metadata.mode();
        let writable_by_others = mode & 0o022 != 0;
        let trusted_sticky_owner = mode & 0o1000 != 0
            && (metadata.uid() == 0 || metadata.uid() == unsafe { libc::geteuid() });
        if writable_by_others && !trusted_sticky_owner {
            return unsafe_state("artifact publication parent permissions are untrusted");
        }
    }
    Ok(())
}

fn private_temporary(metadata: &cap_fs::Metadata) -> bool {
    if !regular(metadata, usize::MAX) {
        return false;
    }
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        metadata.mode() & 0o777 == 0o600
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Canonicalize only the Darwin public aliases used by descriptor-bound persona
/// boundaries.  This is intentionally not filesystem canonicalization: every
/// caller must still walk components with no-follow descriptors.
#[cfg(target_os = "macos")]
pub(crate) fn normalize_persona_path(path: &Path) -> Result<PathBuf, PersonaArtifactError> {
    let text = path
        .to_str()
        .ok_or_else(|| PersonaArtifactError::Unsafe("path is not UTF-8".into()))?;
    if text == "/tmp" || text.starts_with("/tmp/") || text == "/var" || text.starts_with("/var/") {
        Ok(PathBuf::from(format!("/private{text}")))
    } else {
        Ok(path.to_owned())
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn normalize_persona_path(path: &Path) -> Result<PathBuf, PersonaArtifactError> {
    if path.to_str().is_none() {
        unsafe_state("path is not UTF-8")
    } else {
        Ok(path.to_owned())
    }
}

fn unsafe_state<T>(message: impl Into<String>) -> Result<T, PersonaArtifactError> {
    Err(PersonaArtifactError::Unsafe(message.into()))
}

fn links(metadata: &cap_fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        return metadata.nlink();
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        return u64::from(metadata.number_of_links().unwrap_or(0));
    }
    #[allow(unreachable_code)]
    0
}

fn identity(metadata: &cap_fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        FileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        FileIdentity {
            volume: metadata.volume_serial_number().unwrap_or(0),
            index: metadata.file_index().unwrap_or(0),
        }
    }
    #[cfg(not(any(unix, windows)))]
    unreachable!()
}

fn same(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    identity(left) == identity(right)
}

/// Identity alone is insufficient for a retained artifact: chmod/utime can
/// alter the object without replacement. Require every observable Unix
/// metadata field the persona boundaries bind, as well as the object identity.
fn same_metadata(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    if !same(left, right) || left.len() != right.len() || links(left) != links(right) {
        return false;
    }
    #[cfg(unix)]
    {
        mode(left) == mode(right)
            && mtime(left) == mtime(right)
            && mtime_nsec(left) == mtime_nsec(right)
            && ctime(left) == ctime(right)
            && ctime_nsec(left) == ctime_nsec(right)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// A successful rename preserves the object and its data but may update ctime.
/// Keep every stable binding (identity, size, hash, mode, links and mtime)
/// while treating that kernel-authored ctime transition as expected.
fn same_across_rename(left: &Observation, right: &Observation) -> bool {
    left.identity == right.identity && left.len == right.len && left.hash == right.hash && {
        #[cfg(unix)]
        {
            left.mode == right.mode
                && left.nlink == right.nlink
                && left.mtime == right.mtime
                && left.mtime_nsec == right.mtime_nsec
        }
        #[cfg(not(unix))]
        {
            true
        }
    }
}

#[cfg(unix)]
fn mode(metadata: &cap_fs::Metadata) -> u32 {
    use cap_fs::MetadataExt;
    metadata.mode()
}
#[cfg(unix)]
fn mtime(metadata: &cap_fs::Metadata) -> i64 {
    use cap_fs::MetadataExt;
    metadata.mtime()
}
#[cfg(unix)]
fn mtime_nsec(metadata: &cap_fs::Metadata) -> i64 {
    use cap_fs::MetadataExt;
    metadata.mtime_nsec()
}
#[cfg(unix)]
fn ctime(metadata: &cap_fs::Metadata) -> i64 {
    use cap_fs::MetadataExt;
    metadata.ctime()
}
#[cfg(unix)]
fn ctime_nsec(metadata: &cap_fs::Metadata) -> i64 {
    use cap_fs::MetadataExt;
    metadata.ctime_nsec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn absolute(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap()
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn creates_once_and_reads_exact_bytes() {
        let root = tempdir().unwrap();
        let output = absolute(root.path()).join("plan.json");
        let bytes = br#"{\"ok\":true}\n"#;
        assert_eq!(publish_create_only(&output, bytes, 64).unwrap(), output);
        assert_eq!(read_strict(&output, 64).unwrap(), bytes);
        assert!(publish_create_only(&output, bytes, 64).is_err());
    }

    #[test]
    fn rejects_relative_and_traversal_paths() {
        let root = tempdir().unwrap();
        assert!(read_strict(Path::new("plan.json"), 64).is_err());
        let unsafe_path = absolute(root.path())
            .join("nested")
            .join("..")
            .join("plan.json");
        assert!(publish_create_only(&unsafe_path, b"{}", 64).is_err());
        let long = absolute(root.path()).join("x".repeat(256));
        assert!(publish_create_only(&long, b"{}", 64).is_err());
    }

    #[test]
    fn rejects_an_unrepresentable_read_cap_without_overflow() {
        let root = tempdir().unwrap();
        let output = absolute(root.path()).join("plan.json");
        fs::write(&output, b"{}\n").unwrap();
        assert!(read_strict(&output, usize::MAX).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_cap_and_public_directory_identity_match() {
        let root = tempdir().unwrap();
        let target = root.path().join("artifact.json");
        assert!(matches!(
            target.components().next(),
            Some(Component::Prefix(prefix)) if matches!(prefix.kind(), std::path::Prefix::Disk(_))
        ));
        assert!(bind_parent(&target).is_ok());
        let directory = cap_fs::open_ambient_dir(root.path(), ambient_authority()).unwrap();
        let public = kio_core::platform::real_directory_identity(root.path()).unwrap();
        assert_eq!(
            kio_core::platform::directory_handle_identity(&directory),
            public
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_and_hardlink_artifacts() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let base = absolute(root.path());
        let source = base.join("source");
        fs::write(&source, b"{}\n").unwrap();
        let link = base.join("link");
        symlink(&source, &link).unwrap();
        assert!(read_strict(&link, 64).is_err());
        let hard = base.join("hard");
        fs::hard_link(&source, &hard).unwrap();
        assert!(read_strict(&hard, 64).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn retained_artifact_rejects_same_inode_metadata_mutation() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir().unwrap();
        let path = absolute(root.path()).join("artifact.json");
        fs::write(&path, b"{}\n").unwrap();
        let bound = bind_strict(&path, 64).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(bound.recheck().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_replaced_parent_before_publication() {
        let root = tempdir().unwrap();
        let base = absolute(root.path());
        let parent = base.join("out");
        fs::create_dir(&parent).unwrap();
        let target = parent.join("plan.json");
        // Binding is internal, but this exercises the retained-identity check
        // directly before any public leaf is created.
        let bound = bind_parent(&target).unwrap();
        fs::rename(&parent, base.join("old")).unwrap();
        fs::create_dir(&parent).unwrap();
        assert!(recheck_parent(&bound).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_ancestor_replaced_by_symlink_to_retained_parent() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let base = absolute(root.path());
        let ancestor = base.join("ancestor");
        let parent = ancestor.join("out");
        fs::create_dir(&ancestor).unwrap();
        fs::create_dir(&parent).unwrap();
        let bound = bind_parent(&parent.join("plan.json")).unwrap();
        let retained_ancestor = base.join("retained-ancestor");
        fs::rename(&ancestor, &retained_ancestor).unwrap();
        symlink(&retained_ancestor, &ancestor).unwrap();
        assert!(recheck_parent(&bound).is_err());
    }
}
