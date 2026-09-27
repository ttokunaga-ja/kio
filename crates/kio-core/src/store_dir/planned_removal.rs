//! One-shot maintenance deletion authority, captured before confirmation.
//! Quarantine names identify candidates for a *new* preview after interruption;
//! they never persist authority to delete without current caller proofs.
use super::{StoreDirectory, err, ioerr, platform, relative_components};
use crate::Result;
use cap_primitives::fs as cap_fs;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

pub const PRUNE_DIRECTORY_QUARANTINE_PREFIX: &str = ".kio-prune-directory-";
pub const CAS_REMOVAL_QUARANTINE_PREFIX: &str = ".kio-cas-remove-";
const MAX_PINS: usize = 1024;
const MAX_DEPTH: usize = 32;
const MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// Intentional whole-plan maintenance bounds; exceeding one aborts capture.
#[derive(Debug, Default)]
pub struct RemovalBudget {
    pins: usize,
    bytes: u64,
}
impl RemovalBudget {
    pub fn new() -> Self {
        Self::default()
    }
    /// Account for capabilities retained by the caller outside target capsules.
    pub fn reserve_pins(&mut self, count: usize) -> Result<()> {
        self.pins = self
            .pins
            .checked_add(count)
            .filter(|n| *n <= MAX_PINS)
            .ok_or_else(|| {
                err(
                    Path::new("prune plan"),
                    "planned removal exceeds 1024 retained handles",
                )
            })?;
        Ok(())
    }
    fn reserve_bytes(&mut self, bytes: u64) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|n| *n <= MAX_BYTES)
            .ok_or_else(|| {
                err(
                    Path::new("prune plan"),
                    "planned removal exceeds 1 GiB of file bytes",
                )
            })?;
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity(u64, u64);
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum Identity {
    File(crate::cas::WindowsRegularFileIdentity),
    Directory(crate::cas::WindowsDirectoryIdentity),
}

fn identity(file: &File, directory: bool, label: &Path) -> Result<Identity> {
    #[cfg(unix)]
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
    #[cfg(windows)]
    {
        if directory {
            crate::cas::windows_directory_handle_identity(file).map(Identity::Directory)
        } else {
            crate::cas::windows_regular_file_handle_identity(file).map(Identity::File)
        }
        .ok_or_else(|| {
            err(
                label,
                "planned removal target is reparse, hardlinked, or the wrong kind",
            )
        })
    }
}

// Compare retained handles before reading descendants, including the target root.
// Linux mount IDs additionally distinguish same-device bind mounts.
pub(crate) fn require_same_filesystem(parent: &File, child: &File, label: &Path) -> Result<()> {
    #[cfg(unix)]
    let same = {
        use std::os::unix::fs::MetadataExt;
        parent.metadata().map_err(|e| ioerr(label, e))?.dev()
            == child.metadata().map_err(|e| ioerr(label, e))?.dev()
    };
    #[cfg(windows)]
    let same = platform::volume_serial(parent, label)? == platform::volume_serial(child, label)?;
    if !same {
        return Err(err(label, "planned removal crosses a filesystem boundary"));
    }
    #[cfg(target_os = "linux")]
    if mount_id(parent, label)? != mount_id(child, label)? {
        return Err(err(label, "planned removal crosses a mount boundary"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount_id(file: &File, label: &Path) -> Result<u64> {
    use std::os::fd::AsRawFd;
    let mut info: libc::statx = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_MNT_ID,
            &mut info,
        )
    } != 0
    {
        return Err(ioerr(label, std::io::Error::last_os_error()));
    }
    if info.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(err(
            label,
            "planned removal requires retained-handle mount IDs",
        ));
    }
    Ok(info.stx_mnt_id)
}

#[derive(Debug)]
struct Node {
    held: File,
    identity: Identity,
    kind: NodeKind,
}
#[derive(Debug)]
enum NodeKind {
    File { length: u64, digest: String },
    Directory(BTreeMap<OsString, Node>),
}
impl Node {
    fn is_dir(&self) -> bool {
        matches!(self.kind, NodeKind::Directory(_))
    }
}

fn direct_leaf(path: &Path) -> Result<&OsStr> {
    let components = relative_components(path, false, path)?;
    if components.len() != 1 {
        return Err(err(path, "planned removal requires one direct leaf"));
    }
    Ok(components[0])
}
fn quarantine_leaf(prefix: &str, original: &OsStr) -> Result<OsString> {
    let original = original
        .to_str()
        .ok_or_else(|| err(Path::new(original), "planned removal leaf must be UTF-8"))?;
    let value = format!("{prefix}{original}");
    // No truncation or lossy encoding: fit conservative single-component limits
    // on both UTF-8 Unix filesystems and Windows UTF-16 filesystems.
    if value.len() > 255 || value.encode_utf16().count() > 255 {
        return Err(err(
            Path::new(original),
            "planned quarantine filename exceeds the component limit",
        ));
    }
    Ok(value.into())
}
fn exists(parent: &File, leaf: &OsStr, label: &Path) -> Result<bool> {
    match cap_fs::stat(parent, Path::new(leaf), cap_fs::FollowSymlinks::No) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(ioerr(label, e)),
    }
}
fn open(
    parent: &File,
    leaf: &OsStr,
    directory: bool,
    mutation: bool,
    label: &Path,
) -> Result<File> {
    #[cfg(unix)]
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
    #[cfg(unix)]
    {
        use cap_fs::OpenOptionsExt;
        let mut flags = libc::O_NONBLOCK;
        if directory {
            flags |= libc::O_DIRECTORY;
        }
        options.custom_flags(flags);
        let _ = mutation;
    }
    #[cfg(windows)]
    {
        use cap_fs::OpenOptionsExt;
        use windows_sys::Win32::{
            Foundation::{GENERIC_READ, GENERIC_WRITE},
            Storage::FileSystem::{
                DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            },
        };
        // Capture pins permit the later exact DELETE mutator; that mutator
        // denies further writers/deleters and is compared to the pinned identity.
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
        if mutation {
            options
                .access_mode(GENERIC_READ | DELETE | if directory { 0 } else { GENERIC_WRITE })
                .share_mode(FILE_SHARE_READ | if directory { FILE_SHARE_WRITE } else { 0 });
        }
        options.custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT
                | if directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
        );
    }
    let file = cap_fs::open(parent, Path::new(leaf), &options).map_err(|e| ioerr(label, e))?;
    identity(&file, directory, label)?;
    require_same_filesystem(parent, &file, label)?;
    Ok(file)
}
fn digest(file: &File, max: u64, label: &Path) -> Result<(u64, String)> {
    let before = identity(file, false, label)?;
    let metadata = file.metadata().map_err(|e| ioerr(label, e))?;
    if metadata.len() > max {
        return Err(err(label, "planned removal file exceeds its byte bound"));
    }
    let mut reader = file;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|e| ioerr(label, e))?;
    let mut hasher = Sha256::new();
    let mut count = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let cap = (max.saturating_sub(count).saturating_add(1)).min(buffer.len() as u64) as usize;
        let size = reader
            .read(&mut buffer[..cap])
            .map_err(|e| ioerr(label, e))?;
        if size == 0 {
            break;
        }
        count = count
            .checked_add(size as u64)
            .filter(|n| *n <= max)
            .ok_or_else(|| err(label, "planned removal file grew past its bound"))?;
        hasher.update(&buffer[..size]);
    }
    let after = file.metadata().map_err(|e| ioerr(label, e))?;
    if identity(file, false, label)? != before
        || count != metadata.len()
        || after.len() != metadata.len()
        || after.modified().ok() != metadata.modified().ok()
    {
        return Err(err(label, "planned removal file changed while hashing"));
    }
    Ok((
        count,
        format!("sha256:{}", crate::cas::lower_hex(&hasher.finalize())),
    ))
}
fn capture_node(
    parent: &File,
    leaf: &OsStr,
    directory: bool,
    budget: &mut RemovalBudget,
    depth: usize,
    max_file: u64,
    label: &Path,
) -> Result<Node> {
    if depth > MAX_DEPTH {
        return Err(err(label, "planned removal exceeds depth 32"));
    }
    budget.reserve_pins(1)?;
    let held = open(parent, leaf, directory, false, label)?;
    let id = identity(&held, directory, label)?;
    let kind = if directory {
        let mut children = BTreeMap::new();
        for entry in cap_fs::read_base_dir(&held).map_err(|e| ioerr(label, e))? {
            let entry = entry.map_err(|e| ioerr(label, e))?;
            let name = entry.file_name();
            let metadata = cap_fs::stat(&held, Path::new(&name), cap_fs::FollowSymlinks::No)
                .map_err(|e| ioerr(label, e))?;
            let child = capture_node(
                &held,
                &name,
                metadata.is_dir(),
                budget,
                depth + 1,
                MAX_BYTES,
                &label.join(&name),
            )?;
            if children.insert(name, child).is_some() {
                return Err(err(label, "duplicate planned directory entry"));
            }
        }
        NodeKind::Directory(children)
    } else {
        let length = held.metadata().map_err(|e| ioerr(label, e))?.len();
        if length > max_file.min(MAX_BYTES) {
            return Err(err(label, "planned removal file exceeds its byte bound"));
        }
        budget.reserve_bytes(length)?;
        let (length, digest) = digest(&held, length, label)?;
        NodeKind::File { length, digest }
    };
    let node = Node {
        held,
        identity: id,
        kind,
    };
    validate_named_identity(parent, leaf, &node, label)?;
    Ok(node)
}
fn validate_named_identity(parent: &File, leaf: &OsStr, node: &Node, label: &Path) -> Result<()> {
    let named = open(parent, leaf, node.is_dir(), false, label)?;
    if identity(&named, node.is_dir(), label)? != node.identity
        || identity(&node.held, node.is_dir(), label)? != node.identity
    {
        return Err(err(label, "planned removal target identity changed"));
    }
    Ok(())
}
fn validate_named(parent: &File, leaf: &OsStr, node: &Node, label: &Path) -> Result<()> {
    validate_named_identity(parent, leaf, node, label)?;
    validate_node(node, label)
}
fn validate_node(node: &Node, label: &Path) -> Result<()> {
    if identity(&node.held, node.is_dir(), label)? != node.identity {
        return Err(err(label, "planned handle identity changed"));
    }
    match &node.kind {
        NodeKind::File {
            length,
            digest: expected,
        } => {
            if digest(&node.held, *length, label)? != (*length, expected.clone()) {
                return Err(err(label, "planned removal file content changed"));
            }
        }
        NodeKind::Directory(children) => {
            let mut count = 0_usize;
            for entry in cap_fs::read_base_dir(&node.held).map_err(|e| ioerr(label, e))? {
                let entry = entry.map_err(|e| ioerr(label, e))?;
                let name = entry.file_name();
                let child = children
                    .get(&name)
                    .ok_or_else(|| err(label, "unplanned directory entry appeared"))?;
                count = count
                    .checked_add(1)
                    .filter(|n| *n <= children.len())
                    .ok_or_else(|| err(label, "directory inventory changed"))?;
                validate_named(&node.held, &name, child, &label.join(&name))?;
            }
            if count != children.len() {
                return Err(err(label, "planned directory entries disappeared"));
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct Target {
    parent: StoreDirectory,
    canonical: OsString,
    quarantine: OsString,
    quarantined: bool,
    node: Node,
}
impl Target {
    fn capture(
        parent: &StoreDirectory,
        leaf: &Path,
        prefix: &str,
        directory: bool,
        max_file: u64,
        budget: &mut RemovalBudget,
    ) -> Result<Option<Self>> {
        let canonical = direct_leaf(leaf)?.to_os_string();
        let quarantine = quarantine_leaf(prefix, &canonical)?;
        let root = parent.root_handle();
        let original = exists(&root, &canonical, leaf)?;
        let retired = exists(&root, &quarantine, leaf)?;
        if original && retired {
            return Err(err(leaf, "canonical and planned quarantine both exist"));
        }
        if !original && !retired {
            return Ok(None);
        }
        budget.reserve_pins(1)?; // retained parent capability, conservatively per target
        let node = capture_node(
            &root,
            if retired { &quarantine } else { &canonical },
            directory,
            budget,
            0,
            max_file,
            leaf,
        )?;
        Ok(Some(Self {
            parent: parent.clone(),
            canonical,
            quarantine,
            quarantined: retired,
            node,
        }))
    }
    fn revalidate(&self) -> Result<bool> {
        let root = self.parent.root_handle();
        let source = if self.quarantined {
            &self.quarantine
        } else {
            &self.canonical
        };
        let other = if self.quarantined {
            &self.canonical
        } else {
            &self.quarantine
        };
        if exists(&root, other, self.parent.path())? {
            return Err(err(self.parent.path(), "planned target namespace changed"));
        }
        if !exists(&root, source, self.parent.path())? {
            return Ok(false);
        }
        validate_named(&root, source, &self.node, self.parent.path())?;
        Ok(true)
    }
    fn remove(self) -> Result<bool> {
        if !self.revalidate()? {
            return Ok(false);
        }
        let parent = self.parent.root_handle();
        let source = if self.quarantined {
            &self.quarantine
        } else {
            &self.canonical
        };
        let mutator = open(
            &parent,
            source,
            self.node.is_dir(),
            true,
            self.parent.path(),
        )?;
        if identity(&mutator, self.node.is_dir(), self.parent.path())? != self.node.identity {
            return Err(err(
                self.parent.path(),
                "planned mutation handle was substituted",
            ));
        }
        if !self.quarantined {
            rename(
                &parent,
                &self.canonical,
                &self.quarantine,
                &mutator,
                self.parent.path(),
            )?;
            if let Err(error) =
                validate_named(&parent, &self.quarantine, &self.node, self.parent.path())
            {
                // Only a no-replace restoration is permitted. If another leaf
                // occupies the original name, preserve the captured quarantine.
                let restored = rename(
                    &parent,
                    &self.quarantine,
                    &self.canonical,
                    &mutator,
                    self.parent.path(),
                )
                .is_ok();
                let disposition = if restored {
                    "restored to original name".to_owned()
                } else {
                    format!(
                        "quarantine retained at {}",
                        self.parent.path().join(&self.quarantine).display()
                    )
                };
                return Err(err(
                    self.parent.path(),
                    format!("capture validation failed ({error}); {disposition}"),
                ));
            }
            platform::sync_directory(&parent, self.parent.path())?;
        }
        // Revalidate the whole frozen subtree before the first byte/name deletion.
        validate_node(&self.node, self.parent.path())?;
        remove_node(
            &parent,
            &self.quarantine,
            &self.node,
            Some(mutator),
            self.parent.path(),
        )?;
        platform::sync_directory(&parent, self.parent.path())?;
        Ok(true)
    }
}

#[cfg(unix)]
fn rename(
    parent: &File,
    source: &OsStr,
    destination: &OsStr,
    _held: &File,
    label: &Path,
) -> Result<()> {
    platform::rename_regular_noreplace(parent, source, parent, destination, label)
}
#[cfg(windows)]
fn rename(
    parent: &File,
    _source: &OsStr,
    destination: &OsStr,
    held: &File,
    label: &Path,
) -> Result<()> {
    platform::rename_retained_handle(held, parent, destination, false, label)
}
fn remove_node(
    parent: &File,
    leaf: &OsStr,
    node: &Node,
    mutator: Option<File>,
    label: &Path,
) -> Result<()> {
    validate_named_identity(parent, leaf, node, label)?;
    if !node.is_dir() {
        validate_node(node, label)?;
    }
    let mutator = match mutator {
        Some(file) => file,
        None => open(parent, leaf, node.is_dir(), true, label)?,
    };
    if identity(&mutator, node.is_dir(), label)? != node.identity {
        return Err(err(label, "planned child mutation handle changed"));
    }
    if let NodeKind::Directory(children) = &node.kind {
        for (name, child) in children {
            remove_node(&node.held, name, child, None, &label.join(name))?;
        }
        if cap_fs::read_base_dir(&node.held)
            .map_err(|e| ioerr(label, e))?
            .next()
            .is_some()
        {
            return Err(err(
                label,
                "new directory entries prevent planned retirement",
            ));
        }
    }
    // A retained root quarantine is the Unix reserved namespace boundary; no
    // final unlink claims a portable atomic compare-and-delete syscall.
    #[cfg(unix)]
    {
        let named = open(parent, leaf, node.is_dir(), false, label)?;
        if identity(&named, node.is_dir(), label)? != node.identity
            || identity(&node.held, node.is_dir(), label)? != node.identity
        {
            return Err(err(label, "planned child changed before unlink"));
        }
        if node.is_dir() {
            cap_fs::remove_dir(parent, Path::new(leaf))
        } else {
            cap_fs::remove_file(parent, Path::new(leaf))
        }
        .map_err(|e| ioerr(label, e))?;
    }
    #[cfg(windows)]
    {
        if node.is_dir() {
            retire_directory(&mutator, label)?;
        } else {
            platform::retire_gc_handle(&mutator, label)?;
        }
    }
    drop(mutator);
    platform::sync_directory(parent, label)
}
#[cfg(windows)]
fn retire_directory(file: &File, label: &Path) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX, FileDispositionInfoEx, SetFileInformationByHandle,
    };
    let expected = identity(file, true, label)?;
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            &info as *const _ as _,
            std::mem::size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(ioerr(label, std::io::Error::last_os_error()));
    }
    if identity(file, true, label)? != expected {
        return Err(err(label, "retired directory handle identity changed"));
    }
    Ok(())
}

#[derive(Debug)]
pub struct PlannedDirectoryRemoval(Target);
impl PlannedDirectoryRemoval {
    pub fn capture(
        parent: &StoreDirectory,
        leaf: &Path,
        budget: &mut RemovalBudget,
    ) -> Result<Option<Self>> {
        Target::capture(
            parent,
            leaf,
            PRUNE_DIRECTORY_QUARANTINE_PREFIX,
            true,
            MAX_BYTES,
            budget,
        )
        .map(|target| target.map(Self))
    }
    pub fn revalidate(&self) -> Result<bool> {
        self.0.revalidate()
    }
    pub fn remove(self) -> Result<bool> {
        self.0.remove()
    }
}

#[derive(Debug)]
pub struct PlannedFileRemoval(Target);
impl PlannedFileRemoval {
    pub(crate) fn capture_content(
        parent: &StoreDirectory,
        leaf: &Path,
        max: u64,
        budget: &mut RemovalBudget,
    ) -> Result<Option<Self>> {
        Target::capture(
            parent,
            leaf,
            CAS_REMOVAL_QUARANTINE_PREFIX,
            false,
            max,
            budget,
        )
        .map(|target| target.map(Self))
    }
    pub(crate) fn content_hash(&self) -> &str {
        match &self.0.node.kind {
            NodeKind::File { digest, .. } => digest,
            _ => unreachable!("file capsule"),
        }
    }
    pub fn revalidate(&self) -> Result<bool> {
        self.0.revalidate()
    }
    pub fn remove(self) -> Result<bool> {
        self.0.remove()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, StoreDirectory) {
        let temp = tempfile::tempdir().unwrap();
        let parent = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
        (temp, parent)
    }
    fn file_plan(parent: &StoreDirectory, name: &str) -> PlannedFileRemoval {
        PlannedFileRemoval::capture_content(
            parent,
            Path::new(name),
            1024,
            &mut RemovalBudget::new(),
        )
        .unwrap()
        .unwrap()
    }
    fn directory_plan(parent: &StoreDirectory, name: &str) -> PlannedDirectoryRemoval {
        PlannedDirectoryRemoval::capture(parent, Path::new(name), &mut RemovalBudget::new())
            .unwrap()
            .unwrap()
    }
    #[test]
    fn file_replacement_with_identical_bytes_is_never_adopted() {
        let (temp, parent) = fixture();
        std::fs::write(temp.path().join("victim"), b"same").unwrap();
        let plan = file_plan(&parent, "victim");
        std::fs::rename(temp.path().join("victim"), temp.path().join("saved")).unwrap();
        std::fs::write(temp.path().join("victim"), b"same").unwrap();
        assert!(plan.revalidate().is_err());
        assert!(plan.remove().is_err());
        assert_eq!(std::fs::read(temp.path().join("victim")).unwrap(), b"same");
        assert_eq!(std::fs::read(temp.path().join("saved")).unwrap(), b"same");
    }
    #[test]
    fn file_content_change_is_not_hidden_by_same_identity() {
        let (temp, parent) = fixture();
        std::fs::write(temp.path().join("victim"), b"before").unwrap();
        let plan = file_plan(&parent, "victim");
        std::fs::write(temp.path().join("victim"), b"after!").unwrap();
        assert!(plan.remove().is_err());
        assert_eq!(
            std::fs::read(temp.path().join("victim")).unwrap(),
            b"after!"
        );
    }
    #[test]
    fn absent_file_is_false_and_existing_quarantine_is_freshly_captured() {
        let (temp, parent) = fixture();
        std::fs::write(temp.path().join("victim"), b"bytes").unwrap();
        let plan = file_plan(&parent, "victim");
        std::fs::remove_file(temp.path().join("victim")).unwrap();
        assert!(!plan.revalidate().unwrap());
        assert!(!plan.remove().unwrap());
        std::fs::write(temp.path().join(".kio-cas-remove-victim"), b"remaining").unwrap();
        let recovered = file_plan(&parent, "victim");
        assert!(recovered.remove().unwrap());
        assert!(!temp.path().join(".kio-cas-remove-victim").exists());
    }
    #[test]
    fn new_or_replaced_directory_children_abort_before_any_deletion() {
        for replacement in [false, true] {
            let (temp, parent) = fixture();
            std::fs::create_dir(temp.path().join("tree")).unwrap();
            std::fs::write(temp.path().join("tree/first"), b"first").unwrap();
            let plan = directory_plan(&parent, "tree");
            if replacement {
                std::fs::rename(temp.path().join("tree/first"), temp.path().join("saved")).unwrap();
                std::fs::write(temp.path().join("tree/first"), b"first").unwrap();
            } else {
                std::fs::write(temp.path().join("tree/new"), b"new").unwrap();
            }
            assert!(plan.remove().is_err());
            assert_eq!(
                std::fs::read(temp.path().join("tree/first")).unwrap(),
                b"first"
            );
            assert!(!temp.path().join(".kio-prune-directory-tree").exists());
        }
    }
    #[test]
    fn directory_root_replacement_preserves_both_roots() {
        let (temp, parent) = fixture();
        std::fs::create_dir(temp.path().join("tree")).unwrap();
        std::fs::write(temp.path().join("tree/file"), b"same").unwrap();
        let plan = directory_plan(&parent, "tree");
        std::fs::rename(temp.path().join("tree"), temp.path().join("saved")).unwrap();
        std::fs::create_dir(temp.path().join("tree")).unwrap();
        std::fs::write(temp.path().join("tree/file"), b"same").unwrap();
        assert!(plan.remove().is_err());
        assert!(temp.path().join("tree/file").exists());
        assert!(temp.path().join("saved/file").exists());
    }
    #[test]
    fn fresh_directory_preview_can_finish_partial_quarantine() {
        let (temp, parent) = fixture();
        let quarantine = temp.path().join(".kio-prune-directory-tree");
        std::fs::create_dir_all(quarantine.join("nested")).unwrap();
        std::fs::write(quarantine.join("nested/remaining"), b"remaining").unwrap();
        let plan = directory_plan(&parent, "tree");
        assert!(plan.revalidate().unwrap());
        assert!(plan.remove().unwrap());
        assert!(!quarantine.exists());
    }
    #[test]
    fn canonical_and_quarantine_collision_preserves_both() {
        let (temp, parent) = fixture();
        std::fs::write(temp.path().join("victim"), b"canonical").unwrap();
        std::fs::write(temp.path().join(".kio-cas-remove-victim"), b"quarantine").unwrap();
        assert!(
            PlannedFileRemoval::capture_content(
                &parent,
                Path::new("victim"),
                1024,
                &mut RemovalBudget::new()
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(temp.path().join("victim")).unwrap(),
            b"canonical"
        );
        assert_eq!(
            std::fs::read(temp.path().join(".kio-cas-remove-victim")).unwrap(),
            b"quarantine"
        );
    }
    #[test]
    fn hardlinks_and_budget_overflow_do_not_mutate_targets() {
        let (temp, parent) = fixture();
        std::fs::write(temp.path().join("victim"), b"bytes").unwrap();
        std::fs::hard_link(temp.path().join("victim"), temp.path().join("other")).unwrap();
        assert!(
            PlannedFileRemoval::capture_content(
                &parent,
                Path::new("victim"),
                1024,
                &mut RemovalBudget::new()
            )
            .is_err()
        );
        std::fs::remove_file(temp.path().join("other")).unwrap();
        let mut budget = RemovalBudget::new();
        budget.reserve_pins(MAX_PINS).unwrap();
        assert!(
            PlannedFileRemoval::capture_content(&parent, Path::new("victim"), 1024, &mut budget)
                .is_err()
        );
        let mut budget = RemovalBudget {
            pins: 0,
            bytes: MAX_BYTES,
        };
        assert!(
            PlannedFileRemoval::capture_content(&parent, Path::new("victim"), 1024, &mut budget)
                .is_err()
        );
        assert_eq!(std::fs::read(temp.path().join("victim")).unwrap(), b"bytes");
    }
    #[test]
    fn depth_and_quarantine_name_bounds_are_checked_without_truncation() {
        assert!(
            quarantine_leaf(
                PRUNE_DIRECTORY_QUARANTINE_PREFIX,
                OsStr::new(&"x".repeat(255))
            )
            .is_err()
        );
        assert_eq!(
            quarantine_leaf(PRUNE_DIRECTORY_QUARANTINE_PREFIX, OsStr::new("leaf")).unwrap(),
            OsStr::new(".kio-prune-directory-leaf")
        );
        let (temp, parent) = fixture();
        let mut path = temp.path().join("tree");
        std::fs::create_dir(&path).unwrap();
        for _ in 0..=MAX_DEPTH {
            path.push("d");
            std::fs::create_dir(&path).unwrap();
        }
        assert!(
            PlannedDirectoryRemoval::capture(&parent, Path::new("tree"), &mut RemovalBudget::new())
                .is_err()
        );
        assert!(path.is_dir());
        assert!(!temp.path().join(".kio-prune-directory-tree").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_descendant_is_never_followed() {
        let (temp, parent) = fixture();
        std::fs::create_dir(temp.path().join("tree")).unwrap();
        std::fs::write(temp.path().join("external"), b"external").unwrap();
        std::os::unix::fs::symlink(temp.path().join("external"), temp.path().join("tree/link"))
            .unwrap();
        assert!(
            PlannedDirectoryRemoval::capture(&parent, Path::new("tree"), &mut RemovalBudget::new())
                .is_err()
        );
        assert_eq!(
            std::fs::read(temp.path().join("external")).unwrap(),
            b"external"
        );
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn mounted_root_is_rejected_before_descendant_inventory() {
        let root =
            cap_fs::open_ambient_dir(Path::new("/"), cap_primitives::ambient_authority()).unwrap();
        #[cfg(target_os = "macos")]
        let leaf = OsStr::new("dev");
        #[cfg(target_os = "linux")]
        let leaf = OsStr::new("proc");
        let mut budget = RemovalBudget::new();
        let error = capture_node(
            &root,
            leaf,
            true,
            &mut budget,
            0,
            MAX_BYTES,
            Path::new(leaf),
        )
        .unwrap_err();
        assert!(error.to_string().contains("boundary"), "{error}");
        assert_eq!(budget.pins, 1);
        assert_eq!(budget.bytes, 0);
    }

    #[test]
    fn retained_parent_and_child_share_filesystem() {
        let (temp, parent) = fixture();
        std::fs::write(temp.path().join("child"), b"child").unwrap();
        let child = open(
            &parent.handle,
            OsStr::new("child"),
            false,
            false,
            temp.path(),
        )
        .unwrap();
        require_same_filesystem(&parent.handle, &child, temp.path()).unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(
            mount_id(&parent.handle, temp.path()).unwrap(),
            mount_id(&child, temp.path()).unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn captured_read_pin_permits_exact_delete_mutator() {
        let (temp, parent) = fixture();
        std::fs::write(temp.path().join("child"), b"child").unwrap();
        let read = open(
            &parent.handle,
            OsStr::new("child"),
            false,
            false,
            temp.path(),
        )
        .unwrap();
        let mutation = open(
            &parent.handle,
            OsStr::new("child"),
            false,
            true,
            temp.path(),
        )
        .unwrap();
        assert_eq!(
            identity(&read, false, temp.path()).unwrap(),
            identity(&mutation, false, temp.path()).unwrap()
        );
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn maintenance_ancestry_rejects_mount_before_opening_nested_child() {
        let root = StoreDirectory::open(Path::new("/")).unwrap();
        #[cfg(target_os = "macos")]
        let mount = Path::new("dev");
        #[cfg(target_os = "linux")]
        let mount = Path::new("proc");
        // Ordinary opens deliberately retain their existing mount semantics.
        let mounted =
            StoreDirectory::from_retained(root.open_directory(mount).unwrap(), mount.to_path_buf())
                .unwrap();
        let error = root.require_same_filesystem(&mounted).unwrap_err();
        assert!(error.to_string().contains("boundary"), "{error}");
        // The nonexistent grandchild must never be attempted: rejection comes
        // from the first mount hop, before a target capsule can be captured.
        let error = root
            .open_maintenance_directory(&mount.join("kio-nonexistent-maintenance-child"))
            .unwrap_err();
        assert!(error.to_string().contains("boundary"), "{error}");
        let (temp, parent) = fixture();
        std::fs::create_dir_all(temp.path().join("a/b")).unwrap();
        let nested = parent.open_maintenance_directory(Path::new("a/b")).unwrap();
        require_same_filesystem(&parent.root_handle(), &nested, temp.path()).unwrap();
    }
}
