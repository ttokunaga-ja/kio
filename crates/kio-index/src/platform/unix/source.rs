//! Retained source index filesystem binding and observation.
use crate::fts::SourceFileState;
use crate::{IndexError, Result};
use cap_primitives::fs as cap_fs;
use std::path::{Path, PathBuf};

pub(crate) struct BoundSourceIndex {
    // Keep the repository-owned root (normally `.kio`) as well as `index`.
    // Opening `index` relative to this descriptor is what keeps a concurrent
    // replacement of `.kio` from changing the authority used for SQLite.
    pub(crate) _root: std::fs::File,
    pub(crate) _parent: std::fs::File,
    pub(crate) file: std::fs::File,
    pub(crate) public_path: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceFileIdentity {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

impl SourceFileIdentity {
    pub(crate) fn update_digest(self, digest: &mut sha2::Sha256) {
        use sha2::Digest;
        digest.update(self.dev.to_be_bytes());
        digest.update(self.ino.to_be_bytes());
    }
}

pub(crate) fn source_index_path_in_resolved_parent(
    path: &std::path::Path,
) -> Result<std::path::PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    // The immediate `.kio/index` parent is repository-controlled. Do not
    // canonicalize through a replacement symlink here: unlike an OS-owned
    // ancestor such as `/var`, that would redirect a fresh sqlite.db bootstrap
    // into an attacker-selected directory.
    let parent_metadata = std::fs::symlink_metadata(parent).map_err(|error| {
        IndexError::Schema(format!(
            "inspect source index parent {}: {error}",
            parent.display()
        ))
    })?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(IndexError::Schema(format!(
            "source index parent must be a real directory, not a symlink: {}",
            parent.display()
        )));
    }
    let file_name = path.file_name().ok_or_else(|| {
        IndexError::Schema(format!(
            "source index path has no file name: {}",
            path.display()
        ))
    })?;
    Ok(parent.join(file_name))
}

pub(crate) fn bind_source_index(
    path: &Path,
    writable: bool,
    create_missing: bool,
) -> Result<BoundSourceIndex> {
    let path = source_index_path_in_resolved_parent(path)?;
    let parent = path.parent().expect("source index has a parent");
    let root = parent.parent().filter(|root| !root.as_os_str().is_empty());
    let (root_handle, parent_handle) = if let Some(root) = root {
        // A descriptor-bound child index has already changed cwd to the
        // retained `.kio` directory, which is represented as `.`. Treat that
        // current directory as the root capability directly instead of trying
        // to split `.` into a public parent/leaf path.
        if root == Path::new(".") {
            let root_handle =
                cap_fs::open_ambient_dir(Path::new("."), cap_primitives::ambient_authority())
                    .map_err(|e| {
                        IndexError::Schema(format!("open descriptor-bound source index root: {e}"))
                    })?;
            let parent_leaf = parent.file_name().expect("source index parent has a leaf");
            let parent_handle = cap_fs::open_dir_nofollow(&root_handle, Path::new(parent_leaf))
                .map_err(|e| {
                    IndexError::Schema(format!(
                        "open source index parent {}: {e}",
                        parent.display()
                    ))
                })?;
            (root_handle, parent_handle)
        } else {
            let root_leaf = root.file_name().ok_or_else(|| {
                IndexError::Schema(format!(
                    "source index parent has no repository root component: {}",
                    parent.display()
                ))
            })?;
            let outer = root
                .parent()
                .filter(|outer| !outer.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let before_root = std::fs::symlink_metadata(root).map_err(|e| {
                IndexError::Schema(format!("inspect source index root {}: {e}", root.display()))
            })?;
            if before_root.file_type().is_symlink() || !before_root.is_dir() {
                return Err(IndexError::Schema(format!(
                    "source index root must be a real directory, not a symlink: {}",
                    root.display()
                )));
            }

            let outer_handle = cap_fs::open_ambient_dir(outer, cap_primitives::ambient_authority())
                .map_err(|e| {
                    IndexError::Schema(format!(
                        "open source index root ancestor {}: {e}",
                        outer.display()
                    ))
                })?;
            let root_handle = cap_fs::open_dir_nofollow(&outer_handle, Path::new(root_leaf))
                .map_err(|e| {
                    IndexError::Schema(format!("open source index root {}: {e}", root.display()))
                })?;
            let opened_root = root_handle.metadata().map_err(|e| {
                IndexError::Schema(format!(
                    "inspect opened source index root {}: {e}",
                    root.display()
                ))
            })?;
            let root_matches = same_std_and_cap_directory(&before_root, &opened_root);

            if !root_matches {
                return Err(IndexError::Schema(format!(
                    "source index root changed while opening: {}",
                    root.display()
                )));
            }
            let parent_leaf = parent.file_name().expect("source index parent has a leaf");
            let parent_handle = cap_fs::open_dir_nofollow(&root_handle, Path::new(parent_leaf))
                .map_err(|e| {
                    IndexError::Schema(format!(
                        "open source index parent {}: {e}",
                        parent.display()
                    ))
                })?;
            (root_handle, parent_handle)
        }
    } else {
        // A bare relative path has no repository-root component to bind. Keep
        // the old lstat/open/identity check for that API convenience case.
        let before = std::fs::symlink_metadata(parent).map_err(|e| {
            IndexError::Schema(format!(
                "inspect source index parent {}: {e}",
                parent.display()
            ))
        })?;

        let handle = cap_fs::open_ambient_dir(parent, cap_primitives::ambient_authority())
            .map_err(|e| {
                IndexError::Schema(format!(
                    "open source index parent {}: {e}",
                    parent.display()
                ))
            })?;
        let after = handle.metadata().map_err(|e| {
            IndexError::Schema(format!(
                "inspect opened source index parent {}: {e}",
                parent.display()
            ))
        })?;
        let parent_matches = same_std_and_cap_directory(&before, &after);

        if !parent_matches {
            return Err(IndexError::Schema(format!(
                "source index parent changed while opening: {}",
                parent.display()
            )));
        }
        (
            handle.try_clone().map_err(|e| {
                IndexError::Schema(format!(
                    "retain source index parent {}: {e}",
                    parent.display()
                ))
            })?,
            handle,
        )
    };
    let before = parent_handle.metadata().map_err(|e| {
        IndexError::Schema(format!(
            "inspect opened source index parent {}: {e}",
            parent.display()
        ))
    })?;
    if !before.is_dir() {
        return Err(IndexError::Schema(format!(
            "source index parent is not a directory: {}",
            parent.display()
        )));
    }
    let leaf = path.file_name().expect("source index has a leaf");
    let before_leaf = cap_source_leaf_identity(&parent_handle, Path::new(leaf))?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .write(writable)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);

    if create_missing && before_leaf.is_none() {
        options.create_new(true);
    }
    let file = cap_fs::open(&parent_handle, Path::new(leaf), &options)
        .map_err(|e| IndexError::Schema(format!("inspect source index {}: {e}", path.display())))?;
    validate_bound_source_file(&file, &path)?;
    if let Some(before_leaf) = before_leaf
        && source_file_identity(&file)? != before_leaf
    {
        return Err(IndexError::Schema(format!(
            "source index leaf changed while opening: {}",
            path.display()
        )));
    }
    Ok(BoundSourceIndex {
        _root: root_handle,
        _parent: parent_handle,
        file,
        public_path: path,
    })
}

pub(crate) fn cap_source_leaf_identity(
    parent: &std::fs::File,
    leaf: &Path,
) -> Result<Option<SourceFileIdentity>> {
    use cap_fs::MetadataExt;
    match cap_fs::stat(parent, leaf, cap_fs::FollowSymlinks::No) {
        Ok(metadata) if metadata.is_file() && metadata.nlink() == 1 => {
            Ok(Some(SourceFileIdentity {
                dev: metadata.dev(),
                ino: metadata.ino(),
            }))
        }
        Ok(metadata) if metadata.is_file() => Err(IndexError::Schema(format!(
            "source index target must have exactly one hard link (found {}): {}",
            metadata.nlink(),
            leaf.display()
        ))),
        Ok(_) => Err(IndexError::Schema(format!(
            "source index target is not a regular file: {}",
            leaf.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(IndexError::Schema(format!(
            "inspect source index {}: {error}",
            leaf.display()
        ))),
    }
}

pub(crate) fn source_file_identity(file: &std::fs::File) -> Result<SourceFileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let m = file
        .metadata()
        .map_err(|e| IndexError::Schema(format!("inspect opened source index: {e}")))?;
    Ok(SourceFileIdentity {
        dev: m.dev(),
        ino: m.ino(),
    })
}

pub(crate) fn source_file_state(file: &std::fs::File) -> Result<SourceFileState> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file
        .metadata()
        .map_err(|error| IndexError::Schema(format!("inspect GC source index state: {error}")))?;
    Ok(SourceFileState {
        identity: SourceFileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        },
        len: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanos: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    })
}

pub(crate) fn canonical_gc_index_identity(file: &std::fs::File) -> Result<String> {
    let identity = source_file_identity(file)?;
    Ok(format!("unix:{:016x}:{:016x}", identity.dev, identity.ino))
}

pub(crate) fn sync_bound_gc_directory(directory: &std::fs::File, context: &str) -> Result<()> {
    let expected = source_file_identity(directory)?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let syncable = cap_fs::open(directory, Path::new("."), &options)
        .map_err(|error| IndexError::Schema(format!("{context}: {error}")))?;
    let metadata = syncable
        .metadata()
        .map_err(|error| IndexError::Schema(format!("{context}: {error}")))?;
    if !metadata.is_dir() || source_file_identity(&syncable)? != expected {
        return Err(IndexError::Schema(format!(
            "{context}: retained directory changed while reopening for fsync"
        )));
    }
    syncable
        .sync_all()
        .map_err(|error| IndexError::Schema(format!("{context}: {error}")))
}

pub(crate) fn validate_bound_source_file(file: &std::fs::File, path: &Path) -> Result<()> {
    let metadata = file.metadata().map_err(|e| {
        IndexError::Schema(format!(
            "inspect opened source index {}: {e}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(IndexError::Schema(format!(
            "source index target is not a regular file: {}",
            path.display()
        )));
    }

    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() != 1 {
        return Err(IndexError::Schema(format!(
            "source index target must have exactly one hard link (found {}): {}",
            metadata.nlink(),
            path.display()
        )));
    }

    Ok(())
}

pub(crate) fn same_std_and_cap_directory(
    before: &std::fs::Metadata,
    after: &std::fs::Metadata,
) -> bool {
    use std::os::unix::fs::MetadataExt;
    before.dev() == after.dev() && before.ino() == after.ino()
}

pub(crate) fn bind_gc_source_leaf(
    parent: &std::fs::File,
    leaf: &str,
    writable: bool,
) -> Result<Option<BoundSourceIndex>> {
    let root = parent
        .try_clone()
        .map_err(|e| IndexError::Schema(format!("retain GC index capability: {e}")))?;
    let parent = parent
        .try_clone()
        .map_err(|e| IndexError::Schema(format!("retain GC index parent: {e}")))?;
    let public_path = PathBuf::from(leaf);
    let before = cap_source_leaf_identity(&parent, Path::new(leaf))?;
    let Some(before) = before else {
        return Ok(None);
    };
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .write(writable)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);

    let file = cap_fs::open(&parent, Path::new(leaf), &options)
        .map_err(|error| IndexError::Schema(format!("open bound GC source index: {error}")))?;
    validate_bound_source_file(&file, &public_path)?;
    if source_file_identity(&file)? != before {
        return Err(IndexError::Schema(
            "GC source index changed while opening retained capability".to_owned(),
        ));
    }
    let source = BoundSourceIndex {
        _root: root,
        _parent: parent,
        file,
        public_path,
    };
    Ok(Some(source))
}

pub(crate) fn open_gc_source_copy(index: &std::fs::File) -> Result<std::fs::File> {
    let mut source_options = cap_fs::OpenOptions::new();
    source_options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);

    let input = cap_fs::open(index, Path::new("sqlite.db"), &source_options)
        .map_err(|error| IndexError::Schema(format!("open GC source index copy: {error}")))?;
    Ok(input)
}

pub(crate) fn gc_cleanup_directory(
    kio_dir: &std::fs::File,
    _control: &kio_core::control::ControlHandle,
) -> Result<std::fs::File> {
    crate::fts::open_gc_internal_index_dir(kio_dir)
}

pub(crate) fn remove_stale_gc_index_file(
    private: &std::fs::File,
    leaf: &str,
    _identity: &str,
    _control: &kio_core::control::ControlHandle,
) -> Result<()> {
    cap_fs::remove_file(private, Path::new(leaf))
        .map_err(|error| IndexError::Schema(format!("remove stale GC index copy: {error}")))
}
