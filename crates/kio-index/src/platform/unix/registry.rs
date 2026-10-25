use crate::registry::{
    FileObservation, LeafObservation, MAX_REGISTRY_LEAF_BYTES, RegistrySnapshotError,
    SnapshotAttemptError,
};
#[cfg(test)]
use crate::registry::{SnapshotTestHook, SnapshotTestPhase};
use cap_primitives::fs as cap_fs;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    dev: u64,
    ino: u64,
}

pub(crate) struct PrivateSnapshotStorage(tempfile::TempDir);
impl PrivateSnapshotStorage {
    pub(crate) fn create() -> std::result::Result<Self, SnapshotAttemptError> {
        use std::os::unix::fs::PermissionsExt;
        let mut builder = tempfile::Builder::new();
        builder.prefix("kio-registry-snapshot-");
        builder.permissions(fs::Permissions::from_mode(0o700));
        builder.tempdir().map(Self).map_err(|error| {
            SnapshotAttemptError::Unstable(format!("create registry snapshot directory: {error}"))
        })
    }
    pub(crate) fn path(&self) -> &Path {
        self.0.path()
    }
    pub(crate) fn create_file(
        &self,
        basename: &str,
    ) -> std::result::Result<fs::File, SnapshotAttemptError> {
        let temp = &self.0;
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        options.open(temp.path().join(basename)).map_err(|error| {
            SnapshotAttemptError::Unstable(format!(
                "create registry snapshot {}: {error}",
                temp.path().join(basename).display()
            ))
        })
    }
    pub(super) fn native_binding(
        &self,
        path: &Path,
        directory: &kio_core::store_dir::StoreDirectory,
    ) -> crate::Result<super::registry_native::Binding> {
        if path.parent() != Some(self.path()) {
            return Err(crate::IndexError::Schema(
                "native snapshot path is outside its owned storage".into(),
            ));
        }
        super::registry_native::bind(path, directory)
    }
    pub(crate) fn capability(&self) -> std::result::Result<fs::File, SnapshotAttemptError> {
        cap_fs::open_ambient_dir(self.path(), cap_primitives::ambient_authority()).map_err(
            |error| {
                SnapshotAttemptError::Unstable(format!(
                    "open private registry snapshot directory capability: {error}"
                ))
            },
        )
    }
}
pub(crate) fn registry_filesystem_root(
    path: &Path,
) -> std::result::Result<PathBuf, RegistrySnapshotError> {
    {
        let _ = path;
        Ok(PathBuf::from("/"))
    }
}

pub(crate) fn leaf_identity(metadata: &cap_fs::Metadata) -> Option<FileIdentity> {
    {
        use cap_fs::MetadataExt;
        Some(FileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
}

pub(crate) fn validate_regular(
    metadata: &cap_fs::Metadata,
    label: &str,
) -> std::result::Result<(), SnapshotAttemptError> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "scope registry {label} must be a regular non-symlink file"
        )));
    }
    if metadata.len() > MAX_REGISTRY_LEAF_BYTES {
        return Err(SnapshotAttemptError::Unsafe(format!(
            "scope registry {label} exceeds {} byte limit",
            MAX_REGISTRY_LEAF_BYTES
        )));
    }

    {
        use cap_fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(SnapshotAttemptError::Unsafe(format!(
                "scope registry {label} must have exactly one hard link"
            )));
        }
    }

    Ok(())
}

pub(crate) fn observe_leaf(
    parent: &fs::File,
    parent_path: &Path,
    name: &str,
    copy_to: Option<(&PrivateSnapshotStorage, &str)>,
    #[cfg(test)] hook: Option<&SnapshotTestHook>,
    #[cfg(test)] attempt: usize,
) -> std::result::Result<LeafObservation, SnapshotAttemptError> {
    let _ = parent_path;
    let before = match cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LeafObservation::Absent);
        }
        Err(error) => {
            return Err(SnapshotAttemptError::Unstable(format!(
                "stat scope registry {name}: {error}"
            )));
        }
    };
    validate_regular(&before, name)?;
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(
            SnapshotTestPhase::BeforeSourceLeafOpen,
            attempt,
            Some(&parent_path.join(name)),
        );
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut input = cap_fs::open(parent, Path::new(name), &options).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("open scope registry {name}: {error}"))
    })?;
    let opened = cap_fs::Metadata::from_file(&input).map_err(|error| {
        SnapshotAttemptError::Unstable(format!("inspect opened registry {name}: {error}"))
    })?;
    validate_regular(&opened, name)?;
    kio_core::private_fs::verify_private_regular_file_handle(&input)
        .map_err(|error| SnapshotAttemptError::Unsafe(error.to_string()))?;

    let before_identity = leaf_identity(&before).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!("scope registry {name} has no usable identity"))
    })?;
    let opened_identity = leaf_identity(&opened).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!(
            "opened scope registry {name} has no usable identity"
        ))
    })?;
    if before_identity != opened_identity || before.len() != opened.len() {
        return Err(SnapshotAttemptError::Unstable(format!(
            "scope registry {name} changed while opening"
        )));
    }

    let mut output = match copy_to {
        Some((storage, basename)) => Some(storage.create_file(basename)?),
        None => None,
    };
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buf).map_err(|error| {
            SnapshotAttemptError::Unstable(format!("read scope registry {name}: {error}"))
        })?;
        if read == 0 {
            break;
        }
        total = total.checked_add(read as u64).ok_or_else(|| {
            SnapshotAttemptError::Unsafe(format!("scope registry {name} size overflow"))
        })?;
        if total > MAX_REGISTRY_LEAF_BYTES {
            return Err(SnapshotAttemptError::Unsafe(format!(
                "scope registry {name} exceeded byte limit while reading"
            )));
        }
        hash.update(&buf[..read]);
        if let Some(output) = output.as_mut() {
            output.write_all(&buf[..read]).map_err(|error| {
                SnapshotAttemptError::Unstable(format!("write registry snapshot {name}: {error}"))
            })?;
        }
    }
    if let Some(output) = output.as_mut() {
        output.flush().map_err(|error| {
            SnapshotAttemptError::Unstable(format!("flush registry snapshot {name}: {error}"))
        })?;
    }
    #[cfg(test)]
    if let Some(hook) = hook {
        hook(
            SnapshotTestPhase::BeforeSourceLeafRestat,
            attempt,
            Some(&parent_path.join(name)),
        );
    }
    let after =
        cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No).map_err(|error| {
            SnapshotAttemptError::Unstable(format!("restat scope registry {name}: {error}"))
        })?;
    validate_regular(&after, name)?;

    let after_identity = leaf_identity(&after).ok_or_else(|| {
        SnapshotAttemptError::Unsafe(format!(
            "reinspected scope registry {name} has no usable identity"
        ))
    })?;
    if opened_identity != after_identity || opened.len() != after.len() || total != opened.len() {
        return Err(SnapshotAttemptError::Unstable(format!(
            "scope registry {name} changed while reading"
        )));
    }
    Ok(LeafObservation::Present(FileObservation {
        identity: opened_identity,
        bytes: total,
        sha256: hash.finalize().into(),
    }))
}
