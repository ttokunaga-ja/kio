use super::gc_observation::{file_state, same_file_state};
use crate::cas::hash_bytes;
use crate::error::Result;
#[cfg(test)]
use crate::gc::read_regular_observed;
use crate::gc::{
    FileObservation, GcInProgressMarker, MAX_MARKER_BYTES, MAX_METADATA, corrupt, id_meta, ioerr,
    limit, unique_internal_name, valid_file, validate_directory_handle,
};
use crate::platform::gc_io as windows;
use crate::store_dir::StoreDirectory;
#[cfg(test)]
use cap_primitives::ambient_authority;
use cap_primitives::fs as cap_fs;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub(crate) fn open_verified_receipt_durable_handle(
    directory: &std::fs::File,
    leaf: &str,
    expected: &FileObservation,
) -> Result<std::fs::File> {
    use cap_fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    // FlushFileBuffers requires write access even when the receipt bytes stay
    // unchanged. Bind only this receipt through its retained parent, excluding
    // concurrent writes and namespace mutations while it authorizes retirement.
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file =
        cap_fs::open(directory, Path::new(leaf), &options).map_err(|error| ioerr(error, leaf))?;
    verify_retained_receipt_observation(&file, leaf, expected)?;
    Ok(file)
}

/// Windows cannot flush a directory handle (`FlushFileBuffers` rejects it),
/// so retain the fail-closed capability validation and rely on the preceding
/// file-level sync and NTFS metadata journaling. This deliberately does not
/// claim POSIX directory durability on Windows.
pub(crate) fn sync_bound_directory(
    directory: &std::fs::File,
    label: impl AsRef<Path>,
) -> Result<()> {
    validate_directory_handle(directory, label.as_ref())
}

pub(crate) fn verify_retained_receipt_observation(
    file: &std::fs::File,
    leaf: &str,
    expected: &FileObservation,
) -> Result<()> {
    use std::io::{Seek, SeekFrom};

    let before = cap_fs::Metadata::from_file(file).map_err(|error| ioerr(error, leaf))?;
    valid_file(&before, MAX_METADATA)?;
    let mut reader = file;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| ioerr(error, leaf))?;
    let mut bytes = Vec::new();
    reader
        .take(MAX_METADATA.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| ioerr(error, leaf))?;
    let after = cap_fs::Metadata::from_file(file).map_err(|error| ioerr(error, leaf))?;
    valid_file(&after, MAX_METADATA)?;
    if !same_file_state(&before, &after)?
        || bytes.len() as u64 != after.len()
        || id_meta(&after)? != expected.identity
        || file_state(&after) != expected.state
        || hash_bytes(&bytes) != expected.digest
    {
        return Err(corrupt(
            "existing GC shallow receipt changed on retained handle",
        ));
    }
    Ok(())
}

// Move the verified source HANDLE relative to retained parents, refusing an
// occupied destination without reopening the source during the move.
pub(crate) fn rename_snapshot_state_noreplace(
    dir: &std::fs::File,
    from: &str,
    to: &str,
    control: &crate::control::ControlHandle,
) -> Result<()> {
    rename_noreplace_between(dir, from, dir, to, control)
}

pub(crate) fn retire_snapshot_state_temporary(
    dir: &std::fs::File,
    temporary: &str,
    expected_bytes: &[u8],
    expected: &FileObservation,
    control: &crate::control::ControlHandle,
) -> Result<()> {
    windows::retire_expected(
        dir,
        temporary,
        expected_bytes,
        expected,
        MAX_METADATA,
        control,
    )
}

pub(crate) fn rename_noreplace_between(
    from_dir: &std::fs::File,
    from: &str,
    to_dir: &std::fs::File,
    to: &str,
    control: &crate::control::ControlHandle,
) -> Result<()> {
    windows::move_noreplace(from_dir, from, to_dir, to, control)
}

pub(crate) fn directory(
    file: &File,
    control: &crate::control::ControlHandle,
) -> Result<StoreDirectory> {
    StoreDirectory::from_retained_with_control(
        file.try_clone().map_err(|e| ioerr(e, "GC directory"))?,
        PathBuf::from("<retained GC directory>"),
        control.clone(),
    )
}

pub(crate) fn observe(file: &mut File, max: u64) -> Result<(Vec<u8>, FileObservation)> {
    let before = cap_fs::Metadata::from_file(file).map_err(|e| ioerr(e, "GC victim"))?;
    valid_file(&before, max)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| ioerr(e, "GC victim"))?;
    let mut bytes = Vec::new();
    (&mut *file)
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| ioerr(e, "GC victim"))?;
    let after = cap_fs::Metadata::from_file(file).map_err(|e| ioerr(e, "GC victim"))?;
    valid_file(&after, max)?;
    if !same_file_state(&before, &after)? || bytes.len() as u64 != before.len() {
        return Err(corrupt("GC retained victim changed while reading"));
    }
    Ok((
        bytes.clone(),
        FileObservation {
            identity: id_meta(&after)?,
            state: file_state(&after),
            digest: hash_bytes(&bytes),
        },
    ))
}

pub(crate) fn matches_stable(actual: &FileObservation, expected: &FileObservation) -> bool {
    actual.identity == expected.identity
        && actual.state.len == expected.state.len
        && actual.digest == expected.digest
}

pub(crate) fn retire_expected(
    parent: &File,
    leaf: &str,
    bytes: &[u8],
    expected: &FileObservation,
    max: u64,
    control: &crate::control::ControlHandle,
) -> Result<()> {
    let parent = directory(parent, control)?;
    let mut file = crate::platform::open_gc_mutation(&parent, Path::new(leaf), max)?;
    let (actual, observed) = observe(&mut file, max)?;
    if actual != bytes || !matches_stable(&observed, expected) {
        return Err(corrupt(
            "GC temporary changed before exact-handle retirement",
        ));
    }
    crate::platform::retire_gc_handle(&parent, &file)
}

pub(crate) fn remove_completed_marker(
    gc: &File,
    markers: &File,
    expected: &GcInProgressMarker,
    observation: &FileObservation,
    control: &crate::control::ControlHandle,
) -> Result<()> {
    let source = directory(gc, control)?;
    let target = directory(markers, control)?;
    let mut file =
        crate::platform::open_gc_mutation(&source, Path::new("in_progress"), MAX_MARKER_BYTES)?;
    let (bytes, actual) = observe(&mut file, MAX_MARKER_BYTES)?;
    if actual != *observation || GcInProgressMarker::parse_canonical(&bytes)? != *expected {
        return Err(corrupt("GC marker changed before completion archive"));
    }
    let archive = unique_internal_name("completed");
    crate::platform::move_verified_regular_handle_to_raw(
        &source,
        &file,
        &target,
        Path::new(&archive),
    )?;
    let (moved, actual) = observe(&mut file, MAX_MARKER_BYTES)?;
    if moved != bytes || !matches_stable(&actual, observation) {
        return Err(corrupt("GC marker changed during completion archive"));
    }
    crate::platform::retire_gc_handle(&target, &file)
}

pub(crate) fn move_noreplace(
    from_dir: &File,
    from: &str,
    to_dir: &File,
    to: &str,
    control: &crate::control::ControlHandle,
) -> Result<()> {
    let source = directory(from_dir, control)?;
    let target = directory(to_dir, control)?;
    let mut file = crate::platform::open_gc_mutation(&source, Path::new(from), MAX_MARKER_BYTES)?;
    let (bytes, before) = observe(&mut file, MAX_MARKER_BYTES)?;
    crate::platform::move_verified_regular_handle_to_raw(&source, &file, &target, Path::new(to))?;
    let (after_bytes, after) = observe(&mut file, MAX_MARKER_BYTES)?;
    if bytes != after_bytes || !matches_stable(&after, &before) {
        return Err(corrupt("GC publication changed during exact-handle move"));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn publish_staged(
    from_dir: &File,
    staged: &str,
    to_dir: &File,
    leaf: &str,
    bytes: &[u8],
    max: u64,
    staged_checkpoint: impl FnOnce() -> Result<()>,
    control: &crate::control::ControlHandle,
) -> Result<()> {
    use cap_fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, SYNCHRONIZE,
    };
    if bytes.len() as u64 > max {
        return Err(limit("GC publication bytes"));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ)
        .create_new(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file =
        cap_fs::open(from_dir, Path::new(staged), &options).map_err(|e| ioerr(e, staged))?;
    file.write_all(bytes).map_err(|e| ioerr(e, staged))?;
    file.sync_all().map_err(|e| ioerr(e, staged))?;
    let (written, before) = observe(&mut file, max)?;
    if written != bytes {
        return Err(corrupt("GC staged publication changed"));
    }
    staged_checkpoint()?;
    let source = directory(from_dir, control)?;
    let target = directory(to_dir, control)?;
    crate::platform::move_verified_regular_handle_to_raw(&source, &file, &target, Path::new(leaf))?;
    let (published, after) = observe(&mut file, max)?;
    if published != bytes || !matches_stable(&after, &before) {
        return Err(corrupt("GC publication changed during exact-handle move"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent(path: &Path) -> File {
        cap_fs::open_ambient_dir(path, ambient_authority()).unwrap()
    }

    #[test]
    fn staged_publication_collision_preserves_both_files() {
        let temp = tempfile::tempdir().unwrap();
        let directory = parent(temp.path());
        std::fs::write(temp.path().join("public"), b"existing").unwrap();
        assert!(
            publish_staged(
                &directory,
                "stage",
                &directory,
                "public",
                b"replacement",
                64,
                || Ok(()),
                &crate::control::ControlHandle::default()
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(temp.path().join("public")).unwrap(),
            b"existing"
        );
        assert_eq!(
            std::fs::read(temp.path().join("stage")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn staged_publication_interruption_leaves_no_public_name() {
        let temp = tempfile::tempdir().unwrap();
        let directory = parent(temp.path());
        assert!(
            publish_staged(
                &directory,
                "stage",
                &directory,
                "public",
                b"record",
                64,
                || Err(corrupt("test interruption")),
                &crate::control::ControlHandle::default()
            )
            .is_err()
        );
        assert!(!temp.path().join("public").exists());
        assert_eq!(std::fs::read(temp.path().join("stage")).unwrap(), b"record");
    }

    #[test]
    fn exact_retirement_rejects_substituted_identity() {
        let temp = tempfile::tempdir().unwrap();
        let directory = parent(temp.path());
        std::fs::write(temp.path().join("victim"), b"record").unwrap();
        let (bytes, expected) = read_regular_observed(&directory, "victim", 64).unwrap();
        std::fs::rename(temp.path().join("victim"), temp.path().join("original")).unwrap();
        std::fs::write(temp.path().join("victim"), &bytes).unwrap();
        assert!(
            retire_expected(
                &directory,
                "victim",
                &bytes,
                &expected,
                64,
                &crate::control::ControlHandle::default()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(temp.path().join("victim")).unwrap(), bytes);
        assert_eq!(std::fs::read(temp.path().join("original")).unwrap(), bytes);
    }
}
