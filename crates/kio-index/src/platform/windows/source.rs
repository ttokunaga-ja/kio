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
    pub(crate) volume_serial_number: Option<u32>,
    pub(crate) file_index: Option<u64>,
}

impl SourceFileIdentity {
    pub(crate) fn update_digest(self, digest: &mut sha2::Sha256) {
        use sha2::Digest;
        digest.update(self.volume_serial_number.unwrap_or_default().to_be_bytes());
        digest.update(self.file_index.unwrap_or_default().to_be_bytes());
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
            let before_root_identity = kio_core::platform::real_directory_identity(root)
                .map_err(|e| {
                    IndexError::Schema(format!(
                        "inspect source index root reparse state {}: {e}",
                        root.display()
                    ))
                })?
                .ok_or_else(|| {
                    IndexError::Schema(format!(
                        "source index root must be a real directory, not a reparse point: {}",
                        root.display()
                    ))
                })?;
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
            let root_matches = kio_core::platform::directory_handle_identity(&root_handle)
                == Some(before_root_identity);
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
        let before_identity = kio_core::platform::real_directory_identity(parent)
            .map_err(|e| {
                IndexError::Schema(format!(
                    "inspect source index parent reparse state {}: {e}",
                    parent.display()
                ))
            })?
            .ok_or_else(|| {
                IndexError::Schema(format!(
                    "source index parent must be a real directory, not a reparse point: {}",
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
        let parent_matches =
            kio_core::platform::directory_handle_identity(&handle) == Some(before_identity);
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

    // SQLite's public-path open below cannot be passed this capability
    // handle. Keep this leaf handle open without FILE_SHARE_DELETE so a
    // concurrent rename/delete (and therefore path replacement) is denied
    // until SQLite has completed its own open and the connection closes.
    // cap-primitives gives directory handles this policy by default, but
    // ordinary file OpenOptions intentionally default to allowing delete.
    use cap_fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);

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
    use cap_fs::_WindowsByHandle;
    match cap_fs::stat(parent, leaf, cap_fs::FollowSymlinks::No) {
        Ok(metadata) if metadata.is_file() && metadata.number_of_links() == Some(1) => {
            Ok(Some(SourceFileIdentity {
                volume_serial_number: metadata.volume_serial_number(),
                file_index: metadata.file_index(),
            }))
        }
        Ok(metadata) if metadata.is_file() => Err(IndexError::Schema(format!(
            "source index target must have exactly one hard link (found {}): {}",
            metadata
                .number_of_links()
                .map_or_else(|| "unknown".to_owned(), |links| links.to_string()),
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
    use cap_fs::_WindowsByHandle;
    let metadata = cap_fs::Metadata::from_file(file)
        .map_err(|e| IndexError::Schema(format!("inspect opened source index: {e}")))?;
    Ok(SourceFileIdentity {
        volume_serial_number: metadata.volume_serial_number(),
        file_index: metadata.file_index(),
    })
}

pub(crate) fn source_file_state(file: &std::fs::File) -> Result<SourceFileState> {
    use cap_fs::_WindowsByHandle;
    use std::os::windows::fs::MetadataExt;
    let metadata = file
        .metadata()
        .map_err(|error| IndexError::Schema(format!("inspect GC source index state: {error}")))?;
    let by_handle = cap_fs::Metadata::from_file(file).map_err(|error| {
        IndexError::Schema(format!("inspect GC source index handle state: {error}"))
    })?;
    let changed = kio_core::platform::file_change_time(file).map_err(|error| {
        IndexError::Schema(format!(
            "inspect GC source index Windows change time: {error}"
        ))
    })?;
    if changed < 0 {
        return Err(IndexError::Schema(format!(
            "inspect GC source index Windows change time: {}",
            std::io::Error::last_os_error()
        )));
    }
    let modified = metadata.last_write_time();
    Ok(SourceFileState {
        identity: SourceFileIdentity {
            volume_serial_number: by_handle.volume_serial_number(),
            file_index: by_handle.file_index(),
        },
        len: metadata.file_size(),
        modified_seconds: i64::try_from(modified / 10_000_000).unwrap_or(i64::MAX),
        modified_nanos: i64::try_from((modified % 10_000_000) * 100).unwrap_or(i64::MAX),
        changed_seconds: changed / 10_000_000,
        changed_nanos: (changed % 10_000_000) * 100,
    })
}

pub(crate) fn canonical_gc_index_identity(file: &std::fs::File) -> Result<String> {
    let identity = source_file_identity(file)?;
    let volume = identity.volume_serial_number.ok_or_else(|| {
        IndexError::Schema("GC source index has no Windows volume identity".to_owned())
    })?;
    let index = identity.file_index.ok_or_else(|| {
        IndexError::Schema("GC source index has no Windows file identity".to_owned())
    })?;
    Ok(format!("windows:{volume:08x}:{index:016x}"))
}

pub(crate) fn sync_bound_gc_directory(directory: &std::fs::File, context: &str) -> Result<()> {
    if kio_core::platform::directory_handle_identity(directory).is_none() {
        return Err(IndexError::Schema(format!(
            "{context}: retained directory is not a real Windows directory"
        )));
    }
    Ok(())
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

    use cap_fs::_WindowsByHandle;
    let by_handle = cap_fs::Metadata::from_file(file).map_err(|error| {
        IndexError::Schema(format!(
            "inspect opened source index handle {}: {error}",
            path.display()
        ))
    })?;
    if by_handle.number_of_links() != Some(1) {
        return Err(IndexError::Schema(format!(
            "source index target must have exactly one hard link (found {}): {}",
            by_handle
                .number_of_links()
                .map_or_else(|| "unknown".to_owned(), |links| links.to_string()),
            path.display()
        )));
    }

    Ok(())
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

    use cap_fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    // Writable SQLite connections need write sharing. Read-only GC
    // attestation handles and their retained clones must exclude writers.
    options.share_mode(FILE_SHARE_READ | if writable { FILE_SHARE_WRITE } else { 0 });

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

    use cap_fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    // Keep the copied source immutable until the final state recheck.
    source_options.share_mode(FILE_SHARE_READ);

    let input = cap_fs::open(index, Path::new("sqlite.db"), &source_options)
        .map_err(|error| IndexError::Schema(format!("open GC source index copy: {error}")))?;
    Ok(input)
}

pub(crate) fn gc_cleanup_directory(kio_dir: &std::fs::File) -> Result<std::fs::File> {
    crate::platform::windows_source_gc::ensure_no_pending(kio_dir)?;
    crate::fts::open_gc_internal_index_dir(kio_dir)
}

pub(crate) fn remove_stale_gc_index_file(
    private: &std::fs::File,
    leaf: &str,
    identity: &str,
) -> Result<()> {
    crate::platform::windows_source_gc::remove_exact(private, leaf, identity).map(|_| ())
}
