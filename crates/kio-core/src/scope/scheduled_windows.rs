//! Retained-handle observations for Windows scheduled snapshot publication.

use super::*;
use cap_primitives::fs as cap_fs;

pub(super) fn source_identity(file: &File) -> Result<crate::cas::WindowsRegularFileIdentity> {
    crate::cas::windows_regular_file_handle_identity(file)
        .ok_or_else(|| KioError::schema("scheduled file is nonregular, reparse, or hardlinked"))
}

pub(super) fn read_bound_regular_text_observed_at(
    kio: &File,
    relative: &str,
    max_bytes: u64,
) -> Result<(String, BoundMetadataObservation)> {
    let path = Path::new(relative);
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(KioError::invalid_usage(
            "bound metadata path must contain only normal components",
        ));
    }
    let leaf = path
        .file_name()
        .ok_or_else(|| KioError::invalid_usage("bound metadata path must name a regular file"))?;
    let mut directory = kio.try_clone().kio_io(path)?;
    validate_directory(&directory)?;
    for part in path.parent().unwrap_or(Path::new("")).components() {
        let Component::Normal(part) = part else {
            continue;
        };
        directory = cap_fs::open_dir_nofollow(&directory, Path::new(part)).kio_io(path)?;
        validate_directory(&directory)?;
    }
    let open = || {
        let mut options = cap_fs::OpenOptions::new();
        options
            .read(true)
            ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
        cap_fs::open(&directory, Path::new(leaf), &options).kio_io(path)
    };
    let mut file = open()?;
    let identity = source_identity(&file)?;
    let len = file.metadata().kio_io(path)?.len();
    if len > max_bytes {
        return Err(KioError::schema("bound metadata exceeds byte limit"));
    }
    let bytes = read_bytes(&mut file, max_bytes, path)?;
    // Re-open through the retained parent, binding the public leaf as well as
    // the original handle. A same-byte replacement must fail this comparison.
    let mut current = open()?;
    if source_identity(&file)? != identity
        || source_identity(&current)? != identity
        || file.metadata().kio_io(path)?.len() != len
        || current.metadata().kio_io(path)?.len() != len
        || bytes.len() as u64 != len
        || read_bytes(&mut current, max_bytes, path)? != bytes
        || source_identity(&current)? != identity
        || current.metadata().kio_io(path)?.len() != len
    {
        return Err(KioError::schema("bound metadata changed while reading"));
    }
    let digest = crate::cas::lower_hex(&Sha256::digest(&bytes));
    let text = String::from_utf8(bytes).map_err(|error| KioError::schema(error.to_string()))?;
    Ok((
        text,
        BoundMetadataObservation {
            identity,
            len,
            nlink: 1,
            digest,
        },
    ))
}

fn validate_directory(file: &File) -> Result<()> {
    if crate::cas::windows_directory_handle_identity(file).is_none() {
        return Err(KioError::schema(
            "bound metadata parent is not a real directory",
        ));
    }
    Ok(())
}

fn read_bytes(file: &mut File, max_bytes: u64, path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .kio_io(path)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_observation_rejects_same_byte_replacement_and_hardlinks() {
        let temp = tempfile::tempdir().unwrap();
        let leaf = temp.path().join("HEAD");
        fs::write(&leaf, "same bytes").unwrap();
        let directory = StoreDirectory::open(temp.path()).unwrap();
        let root = directory.root_handle();
        let (_, first) = read_bound_regular_text_observed_at(&root, "HEAD", 100).unwrap();
        // Keep the old identity allocated so NTFS cannot reuse it.
        let retained = directory.open_regular_read(Path::new("HEAD"), 100).unwrap();
        fs::rename(&leaf, temp.path().join("old")).unwrap();
        fs::write(&leaf, "same bytes").unwrap();
        let (_, second) = read_bound_regular_text_observed_at(&root, "HEAD", 100).unwrap();
        assert_ne!(first, second);
        assert!(source_identity(&retained).is_ok());
        fs::hard_link(&leaf, temp.path().join("alias")).unwrap();
        assert!(read_bound_regular_text_observed_at(&root, "HEAD", 100).is_err());
    }

    #[test]
    fn metadata_observation_rejects_reparse_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::write(&target, "outside authority").unwrap();
        let link = temp.path().join("HEAD");
        std::os::windows::fs::symlink_file(&target, &link).unwrap();
        let directory = StoreDirectory::open(temp.path()).unwrap();
        assert!(
            read_bound_regular_text_observed_at(&directory.root_handle(), "HEAD", 100).is_err()
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "outside authority");
    }

    #[test]
    fn metadata_observation_rejects_parent_escape_and_oversize() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("HEAD"), "oversize").unwrap();
        let directory = StoreDirectory::open(temp.path()).unwrap();
        let root = directory.root_handle();
        assert!(read_bound_regular_text_observed_at(&root, "../HEAD", 100).is_err());
        assert!(read_bound_regular_text_observed_at(&root, "HEAD", 1).is_err());
    }
}
