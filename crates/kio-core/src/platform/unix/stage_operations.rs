//! Capture shared names in an exclusively owned private directory before
//! publication. A saved stage identity determines its capture name and durable
//! record. Deletion and initialization cleanup do not allocate a capture: they
//! compare the retained object with its name and unlink it directly. Owner-private
//! namespaces are trusted; hostile same-UID namespace/record edits are out of scope.
use crate::{
    Result,
    management::{DirectoryIdentity, directory_identity_from_handle},
    store_dir::{
        Publication, StageIdentity, StoreDirectory,
        atomic::{failure, file_id, io_failure},
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const ITEM: &str = "item";
const RECORD: &str = "capture.json";
const RECORD_LIMIT: u64 = 4096;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureRecord {
    directory: DirectoryIdentity,
    stage: StageIdentity,
}

pub(crate) fn capture_name(identity: &StageIdentity) -> Result<Option<PathBuf>> {
    let bytes =
        serde_jcs::to_vec(identity).map_err(|_| failure("cannot encode stage capture identity"))?;
    Ok(Some(PathBuf::from(format!(
        ".kio-stage-operation-{}",
        crate::cas::lower_hex(&Sha256::digest(bytes))
    ))))
}

pub(crate) fn create_file(directory: &StoreDirectory, leaf: &Path) -> Result<File> {
    directory.create_private_file(leaf)
}
pub(crate) fn open_file(directory: &StoreDirectory, leaf: &Path) -> Result<File> {
    directory.open_regular_read(leaf, u64::MAX)
}

fn read_capture(
    parent: &StoreDirectory,
    name: &Path,
    identity: &StageIdentity,
) -> Result<(StoreDirectory, File, DirectoryIdentity)> {
    let directory = StoreDirectory::from_retained_with_control(
        parent.open_directory(name)?,
        parent.path().join(name),
        parent.control().clone(),
    )?;
    crate::private_fs::verify_private_directory_handle(&directory)?;
    directory.ensure_owner_private(Path::new(RECORD))?;
    let mut file = directory.open_regular_read(Path::new(RECORD), RECORD_LIMIT)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(RECORD_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_failure("cannot read capture record", e))?;
    if bytes.len() as u64 > RECORD_LIMIT {
        return Err(failure("capture record exceeds its bound"));
    }
    let record: CaptureRecord = serde_json::from_slice(&bytes)
        .map_err(|_| failure("capture record is malformed; preserved"))?;
    if record.stage != *identity
        || record.directory != directory_identity_from_handle(&directory.root_handle())?
        || serde_jcs::to_vec(&record).map_err(|_| failure("cannot canonicalize capture record"))?
            != bytes
    {
        return Err(failure(
            "capture record does not match saved identity and directory; preserved",
        ));
    }
    for entry in directory.entries(Path::new(""))? {
        if !entry.is_regular_file || (entry.name != RECORD && entry.name != ITEM) {
            return Err(failure("capture has unknown contents; preserved"));
        }
    }
    Ok((directory, file, record.directory))
}

pub(crate) fn inspect_capture(
    parent: &StoreDirectory,
    name: &Path,
    identity: &StageIdentity,
) -> Result<bool> {
    let (directory, _record, _) = read_capture(parent, name, identity)?;
    if !directory.contains_entry(Path::new(ITEM))? {
        return Ok(false);
    }
    directory.ensure_owner_private(Path::new(ITEM))?;
    identity.verify(&open_file(&directory, Path::new(ITEM))?)?;
    Ok(true)
}

struct Capture {
    parent: StoreDirectory,
    source: PathBuf,
    name: PathBuf,
    directory: StoreDirectory,
    created_identity: DirectoryIdentity,
    record_file: Option<File>,
}
impl Capture {
    fn new(
        parent: &StoreDirectory,
        source: &Path,
        held: &File,
        identity: &StageIdentity,
    ) -> Result<Self> {
        let name = capture_name(identity)?.expect("Unix capture name");
        let handle = parent.create_directory(&name)?;
        let directory = StoreDirectory::from_retained_with_control(
            handle,
            parent.path().join(&name),
            parent.control().clone(),
        )?;
        let created_identity = directory_identity_from_handle(&directory.root_handle())?;
        let mut capture = Self {
            parent: parent.clone(),
            source: source.to_owned(),
            name,
            directory,
            created_identity,
            record_file: None,
        };
        {
            let bytes = serde_jcs::to_vec(&CaptureRecord {
                directory: capture.created_identity.clone(),
                stage: identity.clone(),
            })
            .map_err(|_| failure("cannot encode capture record"))?;
            capture.record_file = Some(capture.directory.create_private_file(Path::new(RECORD))?);
            let file = capture.record_file.as_mut().expect("new capture record");
            file.write_all(&bytes)
                .map_err(|e| io_failure("cannot write capture record", e))?;
            file.sync_all()
                .map_err(|e| io_failure("cannot sync capture record", e))?;
            capture.directory.sync()?;
            parent.sync()?;
        }
        parent.rename_regular_between_raw(
            source,
            &capture.directory,
            Path::new(ITEM),
            Publication::CreateOnly,
        )?;
        let current = open_file(&capture.directory, Path::new(ITEM))?;
        if file_id(held)? != file_id(&current)? {
            return Err(failure("file stage changed before capture; preserved"));
        }
        identity.verify(&current)?;
        // Both the ownership record and the capture rename are durable before
        // the public destination can be touched.
        parent
            .control()
            .checkpoint(crate::durability::DurabilityPoint::StageCaptured)?;
        Ok(capture)
    }

    fn named_directory_matches(&self) -> Result<bool> {
        let current = self.parent.open_directory(&self.name)?;
        Ok(directory_identity_from_handle(&current)? == self.created_identity)
    }

    fn cleanup(&self) -> Result<()> {
        if !self.named_directory_matches()? {
            return Err(failure("capture directory was replaced; preserved"));
        }
        let entries = self.directory.entries(Path::new(""))?;
        if let Some(held_record) = self.record_file.as_ref() {
            if entries.len() != 1 || entries[0].name != RECORD {
                return Err(failure(
                    "capture is not empty except for its record; preserved",
                ));
            }
            let current = self
                .directory
                .open_regular_read(Path::new(RECORD), RECORD_LIMIT)?;
            if file_id(&current)? != file_id(held_record)? {
                return Err(failure("capture record was replaced; preserved"));
            }
            self.directory.remove_file(Path::new(RECORD))?;
        } else if !entries.is_empty() {
            return Err(failure("capture is not empty; preserved"));
        }
        // Keep the created directory handle alive and compare the parent entry
        // again after metadata cleanup. A renamed/replaced root is never adopted.
        if !self.named_directory_matches()? {
            return Err(failure("capture directory was replaced; preserved"));
        }
        cap_primitives::fs::remove_dir(&self.parent.root_handle(), &self.name)
            .map_err(|e| io_failure("cannot remove empty owned capture directory", e))?;
        self.parent.sync()
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        // Restore only into an absent source; collisions leave the durable
        // capture intact for inspection/recovery. Never recursively remove it.
        if self
            .directory
            .contains_entry(Path::new(ITEM))
            .unwrap_or(true)
        {
            let _ = self.directory.rename_regular_between_raw(
                Path::new(ITEM),
                &self.parent,
                &self.source,
                Publication::CreateOnly,
            );
        }
        let _ = self.cleanup();
    }
}

pub(crate) fn recover_file(
    parent: &StoreDirectory,
    source: &Path,
    identity: &StageIdentity,
) -> Result<bool> {
    let name = capture_name(identity)?.expect("Unix capture name");
    if !parent.contains_entry(&name)? {
        return Ok(false);
    }
    let (directory, record, created_identity) = match read_capture(parent, &name, identity) {
        Ok(capture) => capture,
        Err(error) => {
            // A crash before the record was completed, or after it was retired,
            // can leave an unclaimed directory. Keep reporting it as unidentified
            // but do not let it block a verified stage at its original name.
            let directory = StoreDirectory::from_retained_with_control(
                parent.open_directory(&name)?,
                parent.path().join(&name),
                parent.control().clone(),
            )?;
            let original_matches =
                open_file(parent, source).is_ok_and(|file| identity.verify(&file).is_ok());
            if !directory.contains_entry(Path::new(ITEM))? || original_matches {
                return Ok(false);
            }
            // An unverified capture with a payload and no matching original
            // remains an error, rather than being mistaken for an absent stage.
            return Err(error);
        }
    };
    let held = if directory.contains_entry(Path::new(ITEM))? {
        directory.ensure_owner_private(Path::new(ITEM))?;
        let held = open_file(&directory, Path::new(ITEM))?;
        identity.verify(&held)?;
        Some(held)
    } else {
        None
    };
    let capture = Capture {
        parent: parent.clone(),
        source: source.to_owned(),
        name,
        directory,
        created_identity,
        record_file: Some(record),
    };
    if held.is_some() {
        cap_primitives::fs::remove_file(&capture.directory.root_handle(), Path::new(ITEM))
            .map_err(|e| io_failure("cannot reclaim captured stage", e))?;
        capture.directory.sync()?;
    }
    capture.cleanup()?;
    // Drop is idempotent after cleanup. An empty pre-move capture must not
    // prevent the caller from discarding a stage still at its original name.
    Ok(held.is_some())
}

pub(crate) fn publish(
    directory: &StoreDirectory,
    source: &Path,
    held: &File,
    target: &StoreDirectory,
    leaf: &Path,
    publication: Publication,
    identity: &StageIdentity,
) -> Result<()> {
    let capture = Capture::new(directory, source, held, identity)?;
    capture
        .directory
        .rename_regular_between_raw(Path::new(ITEM), target, leaf, publication)?;
    capture.directory.sync()?;
    target.sync()
}
pub(crate) fn discard_file(
    directory: &StoreDirectory,
    source: &Path,
    held: &File,
    identity: Option<&StageIdentity>,
) -> Result<()> {
    // No mkdir, rename or fsync is required before unlink. This also works
    // when parent fsync keeps failing during initialization error cleanup.
    let current = open_file(directory, source)?;
    if file_id(held)? != file_id(&current)? {
        return Err(failure(
            "stage name no longer matches the retained file; preserved",
        ));
    }
    if let Some(identity) = identity {
        identity.verify(&current)?;
    }
    cap_primitives::fs::remove_file(&directory.root_handle(), source)
        .map_err(|e| io_failure("cannot unlink verified stage", e))?;
    directory.sync()
}
pub(crate) fn discard_empty_directory(
    parent: &StoreDirectory,
    leaf: &Path,
    held: &File,
) -> Result<()> {
    let current = parent.open_directory(leaf)?;
    if directory_identity_from_handle(held)? != directory_identity_from_handle(&current)? {
        return Err(failure(
            "stage directory no longer matches the retained handle; preserved",
        ));
    }
    // rmdir can remove only an empty directory; unexpected contents survive.
    cap_primitives::fs::remove_dir(&parent.root_handle(), leaf)
        .map_err(|e| io_failure("cannot remove empty verified stage directory", e))?;
    parent.sync()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    #[test]
    fn continuing_parent_fsync_failure_does_not_allocate_cleanup_directories() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let parent = StoreDirectory::open(temp.path()).unwrap();
        super::super::store::with_failed_directory_sync(&parent.root_handle(), || {
            assert!(parent.sync().is_err());
            let error = crate::store_dir::StageWriter::create(&parent, Path::new("source"))
                .err()
                .unwrap();
            assert_eq!(error.error_code(), "KIO-E-STORE-IO-001");
            assert!(!temp.path().join("source").exists());
            assert!(parent.entries(Path::new("")).unwrap().is_empty());
            assert!(
                parent.sync().is_err(),
                "fault must remain active during cleanup"
            );
        });
        assert!(crate::store_dir::StageWriter::create(&parent, Path::new("source")).is_ok());
    }

    #[test]
    fn missing_and_partial_capture_records_do_not_block_original_stage_cleanup() {
        for record in [None, Some(b"{".as_slice()), Some(b"".as_slice())] {
            let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
            let parent = StoreDirectory::open(temp.path()).unwrap();
            let writer =
                crate::store_dir::StageWriter::create(&parent, Path::new("source")).unwrap();
            let identity = writer.identity().clone();
            drop(writer);
            let name = capture_name(&identity).unwrap().unwrap();
            let directory = StoreDirectory::from_retained(
                parent.create_directory(&name).unwrap(),
                temp.path().join(&name),
            )
            .unwrap();
            if let Some(bytes) = record {
                let mut file = directory.create_private_file(Path::new(RECORD)).unwrap();
                file.write_all(bytes).unwrap();
                file.sync_all().unwrap();
            }
            let expected = [(PathBuf::from("source"), identity.clone())];
            let inventory = parent.inspect_stages(&expected, &[]).unwrap();
            assert!(inventory.iter().any(|e| e.name == name
                && matches!(
                    e.state,
                    crate::store_dir::StageEntryState::Unidentified { .. }
                )));
            assert!(
                parent
                    .discard_stage(Path::new("source"), &identity)
                    .unwrap()
            );
            assert!(!temp.path().join("source").exists());
            assert!(
                !parent
                    .discard_stage(Path::new("source"), &identity)
                    .unwrap()
            );
            assert!(temp.path().join(&name).is_dir());
            if let Some(bytes) = record {
                assert_eq!(
                    fs::read(temp.path().join(&name).join(RECORD)).unwrap(),
                    bytes
                );
            }
            let inventory = parent.inspect_stages(&expected, &[]).unwrap();
            assert!(inventory.iter().any(|e| e.name == name
                && matches!(
                    e.state,
                    crate::store_dir::StageEntryState::Unidentified { .. }
                )));
        }
    }

    #[test]
    fn unknown_capture_payload_does_not_block_a_verified_original() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let parent = StoreDirectory::open(temp.path()).unwrap();
        let writer = crate::store_dir::StageWriter::create(&parent, Path::new("source")).unwrap();
        let identity = writer.identity().clone();
        drop(writer);
        let name = capture_name(&identity).unwrap().unwrap();
        let directory = StoreDirectory::from_retained(
            parent.create_directory(&name).unwrap(),
            temp.path().join(&name),
        )
        .unwrap();
        let mut unknown = directory.create_private_file(Path::new(ITEM)).unwrap();
        unknown.write_all(b"preserve unknown payload").unwrap();
        assert!(
            parent
                .discard_stage(Path::new("source"), &identity)
                .unwrap()
        );
        assert!(!temp.path().join("source").exists());
        assert_eq!(
            fs::read(temp.path().join(&name).join(ITEM)).unwrap(),
            b"preserve unknown payload"
        );
        assert!(
            parent
                .discard_stage(Path::new("source"), &identity)
                .is_err()
        );
        assert!(
            parent
                .inspect_stages(&[(PathBuf::from("source"), identity)], &[])
                .unwrap()
                .iter()
                .any(|e| e.name == name
                    && matches!(
                        e.state,
                        crate::store_dir::StageEntryState::Unidentified { .. }
                    ))
        );
    }

    #[test]
    fn interrupted_capture_is_inspected_and_reclaimed_from_saved_stage_identity() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let directory = StoreDirectory::open(temp.path()).unwrap();
        let mut writer =
            crate::store_dir::StageWriter::create(&directory, Path::new("source")).unwrap();
        writer.write_chunk(b"owned").unwrap();
        writer.sync().unwrap();
        let identity = writer.identity().clone();
        drop(writer);
        let held = open_file(&directory, Path::new("source")).unwrap();
        let capture = Capture::new(&directory, Path::new("source"), &held, &identity).unwrap();
        std::mem::forget(capture); // abrupt exit: no restoring Drop
        let inventory = directory
            .inspect_stages(&[(PathBuf::from("source"), identity.clone())], &[])
            .unwrap();
        assert!(!inventory.iter().any(|e| matches!(
            e.state,
            crate::store_dir::StageEntryState::Unidentified { .. }
                | crate::store_dir::StageEntryState::Missing
        )));
        assert!(
            directory
                .discard_stage(Path::new("source"), &identity)
                .unwrap()
        );
        assert!(directory.entries(Path::new("")).unwrap().is_empty());
        assert!(
            !directory
                .discard_stage(Path::new("source"), &identity)
                .unwrap()
        );
    }

    #[test]
    fn capture_cleanup_preserves_a_replacement_directory_and_reports_residue() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let directory = StoreDirectory::open(temp.path()).unwrap();
        let held = create_file(&directory, Path::new("source")).unwrap();
        let identity = StageIdentity::initialize(&held).unwrap();
        let capture = Capture::new(&directory, Path::new("source"), &held, &identity).unwrap();
        let original_name = capture.name.clone();
        fs::rename(temp.path().join(&original_name), temp.path().join("moved")).unwrap();
        directory.create_directory(&original_name).unwrap();
        drop(capture);
        assert!(temp.path().join(&original_name).is_dir());
        assert!(temp.path().join("moved").is_dir());
        let inventory = directory.inspect_stages(&[], &[]).unwrap();
        for name in [original_name.as_os_str(), std::ffi::OsStr::new("moved")] {
            assert!(inventory.iter().any(|e| e.name == name
                && matches!(
                    e.state,
                    crate::store_dir::StageEntryState::Unidentified { .. }
                )));
        }
    }

    #[test]
    fn recovery_preserves_a_new_source_and_refuses_a_copied_capture_record() {
        for copied_record in [false, true] {
            let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
            let directory = StoreDirectory::open(temp.path()).unwrap();
            let mut held = create_file(&directory, Path::new("source")).unwrap();
            held.write_all(b"owned").unwrap();
            let identity = StageIdentity::initialize(&held).unwrap();
            let capture = Capture::new(&directory, Path::new("source"), &held, &identity).unwrap();
            let name = capture.name.clone();
            std::mem::forget(capture);
            let mut foreign = create_file(&directory, Path::new("source")).unwrap();
            foreign.write_all(b"foreign").unwrap();
            if copied_record {
                fs::rename(temp.path().join(&name), temp.path().join("moved")).unwrap();
                directory.create_directory(&name).unwrap();
                let replacement = StoreDirectory::from_retained(
                    directory.open_directory(&name).unwrap(),
                    temp.path().join(&name),
                )
                .unwrap();
                let bytes = fs::read(temp.path().join("moved").join(RECORD)).unwrap();
                let mut record = replacement.create_private_file(Path::new(RECORD)).unwrap();
                record.write_all(&bytes).unwrap();
                assert!(
                    directory
                        .discard_stage(Path::new("source"), &identity)
                        .is_err()
                );
                assert_eq!(fs::read(temp.path().join("moved/item")).unwrap(), b"owned");
                assert!(temp.path().join(&name).is_dir());
            } else {
                assert!(
                    directory
                        .discard_stage(Path::new("source"), &identity)
                        .unwrap()
                );
                assert!(!temp.path().join(&name).exists());
            }
            assert_eq!(fs::read(temp.path().join("source")).unwrap(), b"foreign");
        }
    }

    #[test]
    fn completed_capture_metadata_is_reclaimed_without_touching_publication() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let directory = StoreDirectory::open(temp.path()).unwrap();
        let mut held = create_file(&directory, Path::new("source")).unwrap();
        held.write_all(b"published").unwrap();
        let identity = StageIdentity::initialize(&held).unwrap();
        let capture = Capture::new(&directory, Path::new("source"), &held, &identity).unwrap();
        capture
            .directory
            .rename_regular_between_raw(
                Path::new(ITEM),
                &directory,
                Path::new("public"),
                Publication::CreateOnly,
            )
            .unwrap();
        let name = capture.name.clone();
        std::mem::forget(capture);
        assert!(
            !directory
                .discard_stage(Path::new("source"), &identity)
                .unwrap()
        );
        assert!(!temp.path().join(name).exists());
        assert_eq!(fs::read(temp.path().join("public")).unwrap(), b"published");
    }

    #[test]
    fn stage_capture_mutations_do_not_reuse_the_shared_source_name() {
        for publish in [false, true] {
            let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
            let directory = StoreDirectory::open(temp.path()).unwrap();
            let mut held = directory.create_private_file(Path::new("source")).unwrap();
            held.write_all(b"owned").unwrap();
            let identity = StageIdentity::initialize(&held).unwrap();
            let capture = Capture::new(&directory, Path::new("source"), &held, &identity).unwrap();
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
