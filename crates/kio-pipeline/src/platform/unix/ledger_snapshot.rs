use crate::ledger::snapshot::{AttemptError, LedgerSnapshotError};
use crate::ledger::snapshot::{
    Leaf, MAX_LEAF_BYTES, Manifest, Observed, SNAPSHOT_MAIN, validate_file,
};
use cap_primitives::fs as cap_fs;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::{
    fs,
    path::{Path, PathBuf},
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    dev: u64,
    ino: u64,
}
pub(crate) struct PrivateStorage(tempfile::TempDir);
impl PrivateStorage {
    pub(crate) fn create(
        #[cfg(test)] hooks: Option<&crate::ledger::snapshot::SnapshotTestHooks>,
    ) -> Result<Self, AttemptError> {
        #[cfg(test)]
        crate::ledger::snapshot::run_private_storage_create_hook(hooks)?;
        use std::os::unix::fs::PermissionsExt;
        let mut builder = tempfile::Builder::new();
        builder.prefix("kio-ledger-snapshot-");
        builder.permissions(fs::Permissions::from_mode(0o700));
        builder
            .tempdir()
            .map(Self)
            .map_err(|e| AttemptError::Unstable(format!("create private ledger snapshot dir: {e}")))
    }
    pub(crate) fn path(&self) -> &Path {
        self.0.path()
    }
    pub(crate) fn create_file(&self, name: &str) -> Result<fs::File, AttemptError> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        options.open(self.0.path().join(name)).map_err(|e| {
            AttemptError::Unstable(format!("create private ledger snapshot {name}: {e}"))
        })
    }
}
pub(crate) fn identity(m: &cap_fs::Metadata) -> Identity {
    use cap_fs::MetadataExt;
    Identity {
        dev: m.dev(),
        ino: m.ino(),
    }
}
pub(crate) fn same_identity(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    identity(left) == identity(right)
}
pub(crate) fn filesystem_root(_path: &Path) -> Result<PathBuf, LedgerSnapshotError> {
    Ok(PathBuf::from("/"))
}
pub(crate) fn verify_directory_binding(
    _path: &Path,
    _file: &fs::File,
    _label: &str,
) -> Result<(), LedgerSnapshotError> {
    Ok(())
}
pub(crate) fn verify_regular_binding(
    _path: &Path,
    _file: &fs::File,
    _label: &str,
) -> Result<(), AttemptError> {
    Ok(())
}
pub(crate) fn validate_file_links(m: &cap_fs::Metadata, label: &str) -> Result<(), AttemptError> {
    use cap_fs::MetadataExt;
    if m.nlink() != 1 {
        return Err(AttemptError::Unsafe(format!(
            "ledger {label} has hard links"
        )));
    }
    Ok(())
}
pub(crate) fn validate_test_barrier_parent(parent: &fs::File) -> Result<(), AttemptError> {
    let metadata = cap_fs::Metadata::from_file(parent)
        .map_err(|e| AttemptError::Unstable(format!("inspect test ledger barrier parent: {e}")))?;
    if !metadata.is_dir() {
        return Err(AttemptError::Unsafe(
            "test ledger barrier parent is not a directory".into(),
        ));
    }

    use cap_fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and reads only process metadata.
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid || metadata.mode() & 0o077 != 0 {
        return Err(AttemptError::Unsafe(
            "test ledger barrier parent is not owner-private".into(),
        ));
    }
    Ok(())
}
pub(crate) fn verify_private_leaf(
    dir: &fs::File,
    name: &str,
    expected: &Observed,
) -> Result<(), AttemptError> {
    let before = cap_fs::stat(dir, Path::new(name), cap_fs::FollowSymlinks::No).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AttemptError::Unsafe(format!("private ledger snapshot {name} is missing"))
        } else {
            AttemptError::Unstable(format!("stat private ledger snapshot {name}: {e}"))
        }
    })?;
    validate_file(&before, name)?;
    if before.len() != expected.bytes {
        return Err(AttemptError::Unsafe(format!(
            "private ledger snapshot {name} size differs"
        )));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut input = cap_fs::open(dir, Path::new(name), &options).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AttemptError::Unsafe(format!("private ledger snapshot {name} disappeared"))
        } else {
            AttemptError::Unstable(format!("open private ledger snapshot {name}: {e}"))
        }
    })?;
    let opened = cap_fs::Metadata::from_file(&input).map_err(|e| {
        AttemptError::Unstable(format!("inspect private ledger snapshot {name}: {e}"))
    })?;
    validate_file(&opened, name)?;
    if !same_identity(&before, &opened) || before.len() != opened.len() {
        return Err(AttemptError::Unsafe(format!(
            "private ledger snapshot {name} changed while opening"
        )));
    }
    let mut bytes = 0u64;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = input.read(&mut buffer).map_err(|e| {
            AttemptError::Unstable(format!("read private ledger snapshot {name}: {e}"))
        })?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        if bytes > MAX_LEAF_BYTES {
            return Err(AttemptError::Unsafe(format!(
                "private ledger snapshot {name} exceeds bound"
            )));
        }
        hash.update(&buffer[..count]);
    }
    let after = cap_fs::stat(dir, Path::new(name), cap_fs::FollowSymlinks::No).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AttemptError::Unsafe(format!("private ledger snapshot {name} disappeared"))
        } else {
            AttemptError::Unstable(format!("re-stat private ledger snapshot {name}: {e}"))
        }
    })?;
    validate_file(&after, name)?;
    if !same_identity(&opened, &after)
        || bytes != expected.bytes
        || hash.finalize().as_slice() != expected.sha256
    {
        return Err(AttemptError::Unsafe(format!(
            "private ledger snapshot {name} differs from source copy"
        )));
    }
    Ok(())
}
pub(crate) fn verify_private_snapshot(
    path: &Path,
    manifest: &Manifest,
) -> Result<(), AttemptError> {
    let dir = cap_fs::open_ambient_dir(path, cap_primitives::ambient_authority()).map_err(|e| {
        AttemptError::Unstable(format!("open private ledger snapshot capability: {e}"))
    })?;
    let Leaf::Present(main) = &manifest.main else {
        return Err(AttemptError::Unsafe(
            "private ledger snapshot without source main".into(),
        ));
    };
    verify_private_leaf(&dir, SNAPSHOT_MAIN, main)?;
    let wal_name = format!("{SNAPSHOT_MAIN}-wal");
    let mut allowed = std::collections::BTreeSet::from([SNAPSHOT_MAIN.to_owned()]);
    if let Leaf::Present(wal) = &manifest.wal {
        allowed.insert(wal_name.clone());
        verify_private_leaf(&dir, &wal_name, wal)?;
    }
    let shm = format!("{SNAPSHOT_MAIN}-shm");
    match cap_fs::stat(&dir, Path::new(&shm), cap_fs::FollowSymlinks::No) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(AttemptError::Unsafe(
                "private ledger snapshot has pre-open SHM".into(),
            ));
        }
        Err(error) => {
            return Err(AttemptError::Unstable(format!(
                "stat private ledger SHM: {error}"
            )));
        }
    }
    for entry in cap_fs::read_base_dir(&dir)
        .map_err(|e| AttemptError::Unstable(format!("enumerate private ledger snapshot: {e}")))?
    {
        let entry = entry.map_err(|e| {
            AttemptError::Unstable(format!("enumerate private ledger snapshot leaf: {e}"))
        })?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            return Err(AttemptError::Unsafe(
                "private ledger snapshot non-utf8 leaf".into(),
            ));
        };
        if !allowed.contains(name) {
            return Err(AttemptError::Unsafe(format!(
                "private ledger snapshot unexpected leaf {name}"
            )));
        }
    }
    Ok(())
}
pub(crate) fn verify_private_storage(
    storage: &PrivateStorage,
    manifest: &Manifest,
) -> Result<(), AttemptError> {
    verify_private_snapshot(storage.path(), manifest)
}
