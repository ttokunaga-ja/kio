//! POSIX has no unlink/rename-by-file-descriptor operation. Capture a name in
//! a fresh, exclusively owned private directory, then compare it with the live
//! handle before publication or unlink. The shared source name is never used
//! again for a destructive operation. A mismatching capture is restored without
//! replacement, or preserved for inspection if restoration is no longer safe.
use crate::{
    Result,
    store_dir::{
        Publication, StoreDirectory,
        atomic::{failure, file_id, io_failure},
    },
};
use std::{
    fs::File,
    path::{Path, PathBuf},
};

const ITEM: &str = "item";

pub(crate) fn create_file(directory: &StoreDirectory, leaf: &Path) -> Result<File> {
    directory.create_private_file(leaf)
}
pub(crate) fn open_file(directory: &StoreDirectory, leaf: &Path) -> Result<File> {
    directory.open_regular_read(leaf, u64::MAX)
}

struct Capture {
    parent: StoreDirectory,
    source: PathBuf,
    name: PathBuf,
    directory: StoreDirectory,
    is_directory: bool,
}
impl Capture {
    fn new(
        parent: &StoreDirectory,
        source: &Path,
        held: &File,
        is_directory: bool,
    ) -> Result<Self> {
        // Cleanup must still work when the entropy source itself failed.
        // Create-only allocation makes these private operation directories
        // exclusive; the name is never used as ownership evidence.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut reserved = None;
        for _ in 0..64 {
            let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let name = PathBuf::from(format!(
                ".kio-stage-operation-{}-{serial}",
                std::process::id()
            ));
            match parent.create_directory(&name) {
                Ok(handle) => {
                    reserved = Some((name, handle));
                    break;
                }
                Err(error) if error.context()["io_error_kind"] == "already_exists" => continue,
                Err(error) => return Err(error),
            }
        }
        let (name, handle) =
            reserved.ok_or_else(|| failure("cannot reserve private stage operation directory"))?;
        let directory = StoreDirectory::from_retained_with_control(
            handle,
            parent.path().join(&name),
            parent.control().clone(),
        )?;
        let capture = Self {
            parent: parent.clone(),
            source: source.to_owned(),
            name,
            directory,
            is_directory,
        };
        if is_directory {
            parent.rename_directory_between_create_only(
                source,
                &capture.directory,
                Path::new(ITEM),
            )?;
            let current = capture.directory.open_directory(Path::new(ITEM))?;
            if crate::management::directory_identity_from_handle(held)?
                != crate::management::directory_identity_from_handle(&current)?
            {
                return Err(failure("directory stage changed before capture; preserved"));
            }
        } else {
            parent.rename_regular_between_raw(
                source,
                &capture.directory,
                Path::new(ITEM),
                Publication::CreateOnly,
            )?;
            let current = capture
                .directory
                .open_regular_read(Path::new(ITEM), u64::MAX)?;
            if file_id(held)? != file_id(&current)? {
                return Err(failure("file stage changed before capture; preserved"));
            }
        }
        Ok(capture)
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        // On failure restore only into an absent source. Never delete unknown
        // contents, recursively clean a capture, or overwrite a concurrent file.
        if self
            .directory
            .contains_entry(Path::new(ITEM))
            .unwrap_or(true)
        {
            if self.is_directory {
                let _ = self.directory.rename_directory_between_create_only(
                    Path::new(ITEM),
                    &self.parent,
                    &self.source,
                );
            } else {
                let _ = self.directory.rename_regular_between_raw(
                    Path::new(ITEM),
                    &self.parent,
                    &self.source,
                    Publication::CreateOnly,
                );
            }
        }
        let _ = cap_primitives::fs::remove_dir(&self.parent.root_handle(), &self.name);
        let _ = self.parent.sync();
    }
}

pub(crate) fn publish(
    directory: &StoreDirectory,
    source: &Path,
    held: &File,
    target: &StoreDirectory,
    leaf: &Path,
    publication: Publication,
) -> Result<()> {
    let capture = Capture::new(directory, source, held, false)?;
    capture
        .directory
        .rename_regular_between_raw(Path::new(ITEM), target, leaf, publication)?;
    capture.directory.sync()?;
    target.sync()
}

pub(crate) fn discard_file(directory: &StoreDirectory, source: &Path, held: &File) -> Result<()> {
    let capture = Capture::new(directory, source, held, false)?;
    cap_primitives::fs::remove_file(&capture.directory.root_handle(), Path::new(ITEM))
        .map_err(|e| io_failure("cannot unlink captured stage", e))?;
    capture.directory.sync()
}

pub(crate) fn discard_empty_directory(
    parent: &StoreDirectory,
    leaf: &Path,
    held: &File,
) -> Result<()> {
    let capture = Capture::new(parent, leaf, held, true)?;
    cap_primitives::fs::remove_dir(&capture.directory.root_handle(), Path::new(ITEM))
        .map_err(|e| io_failure("cannot remove empty captured stage directory", e))?;
    capture.directory.sync()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    #[test]
    fn stage_capture_mutations_do_not_reuse_the_shared_source_name() {
        for publish in [false, true] {
            let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
            let directory = StoreDirectory::open(temp.path()).unwrap();
            let mut held = directory.create_private_file(Path::new("source")).unwrap();
            held.write_all(b"owned").unwrap();
            let capture = Capture::new(&directory, Path::new("source"), &held, false).unwrap();
            let mut foreign = directory.create_private_file(Path::new("source")).unwrap();
            foreign.write_all(b"foreign").unwrap();
            fs::write(temp.path().join("public"), b"previous").unwrap();
            if publish {
                capture
                    .directory
                    .rename_regular_between_raw(
                        Path::new(ITEM),
                        &directory,
                        Path::new("public"),
                        Publication::Replace,
                    )
                    .unwrap();
                assert_eq!(fs::read(temp.path().join("public")).unwrap(), b"owned");
            } else {
                cap_primitives::fs::remove_file(&capture.directory.root_handle(), Path::new(ITEM))
                    .unwrap();
                assert_eq!(fs::read(temp.path().join("public")).unwrap(), b"previous");
            }
            drop(capture);
            assert_eq!(fs::read(temp.path().join("source")).unwrap(), b"foreign");
            assert_eq!(directory.entries(Path::new("")).unwrap().len(), 2);
        }
    }
}
