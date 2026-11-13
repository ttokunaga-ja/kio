//! Private streaming files. Persist `identity()` in the caller's durable journal
//! before writing payload. Dropping a writer closes it and preserves its name;
//! recovery must supply that saved identity, never infer ownership from bytes.

use super::{
    Publication, StoreDirectory,
    atomic::{FileSystemIdentity, failure, file_id, io_failure},
};
use crate::{
    Result,
    error::{Resource, ResourceLimit},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageIdentity {
    physical: FileSystemIdentity,
    nonce: [u8; 32],
}

/// Saved authority for a Kio-created directory stage, including after rename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryStageIdentity {
    physical: crate::management::DirectoryIdentity,
    nonce: [u8; 32],
}

impl DirectoryStageIdentity {
    pub fn read(directory: &StoreDirectory) -> Result<Self> {
        let handle = directory.root_handle();
        Ok(Self {
            physical: crate::management::directory_identity_from_handle(&handle)?,
            nonce: crate::platform::stage_nonce::read(&handle).map_err(|e| {
                nonce_error(
                    "cannot read directory stage nonce (xattr/EA required; no fallback)",
                    e,
                )
            })?,
        })
    }
    pub fn verify(&self, directory: &StoreDirectory) -> Result<()> {
        if &Self::read(directory)? != self {
            return Err(failure("directory stage identity does not match"));
        }
        Ok(())
    }
}

impl StageIdentity {
    pub(crate) fn initialize(file: &File) -> Result<Self> {
        let physical = file_id(file)?;
        let mut nonce = [0; 32];
        getrandom::fill(&mut nonce)
            .map_err(|e| failure(format!("cannot generate stage nonce: {e}")))?;
        crate::platform::stage_nonce::create(file, &nonce).map_err(|e| {
            nonce_error(
                "cannot persist stage nonce (filesystem must support xattr/EA; no fallback)",
                e,
            )
        })?;
        let identity = Self { physical, nonce };
        identity.verify(file)?;
        file.sync_all()
            .map_err(|e| stage_io("cannot sync stage identity", e))?;
        Ok(identity)
    }

    /// Compare both the retained physical identity and the persisted nonce.
    /// Missing, malformed or unreadable attributes never establish ownership.
    pub fn verify(&self, file: &File) -> Result<()> {
        if self.physical != file_id(file)? {
            return Err(failure("stage physical identity does not match"));
        }
        let nonce = crate::platform::stage_nonce::read(file).map_err(|e| {
            nonce_error(
                "cannot read stage nonce (xattr/EA required; no fallback)",
                e,
            )
        })?;
        if nonce != self.nonce {
            return Err(failure("stage nonce does not match"));
        }
        Ok(())
    }
}

fn nonce_error(message: &str, error: std::io::Error) -> crate::KioError {
    if crate::platform::stage_nonce::storage_exhausted(&error) {
        return stage_io(message, error);
    }
    crate::KioError::new(
        "KIO-E-STAGE-NONCE-001",
        format!("{message}: {error}"),
        serde_json::json!({"operation": "stage_identity", "os_error": error.raw_os_error(), "fallback": false}),
        crate::ExitCode::PermanentFailure,
    )
}

pub(crate) fn stage_io(message: &str, error: std::io::Error) -> crate::KioError {
    if crate::platform::stage_nonce::storage_exhausted(&error) {
        ResourceLimit {
            resource: Resource::Storage,
            stage: "private_stage".into(),
            limit: None,
            observed: None,
            resource_tier: None,
        }
        .into_error(format!("{message}: {error}"))
    } else {
        io_failure(message, error)
    }
}

/// One create-only private file, written with bounded caller-supplied chunks.
/// There is no implicit deletion on Drop and no payload-sized allocation.
pub struct StageWriter {
    directory: StoreDirectory,
    leaf: PathBuf,
    file: File,
    identity: StageIdentity,
}

impl StageWriter {
    pub fn create(directory: &StoreDirectory, leaf: &Path) -> Result<Self> {
        Self::create_with(directory, leaf, StageIdentity::initialize)
    }

    fn create_with(
        directory: &StoreDirectory,
        leaf: &Path,
        initialize: impl FnOnce(&File) -> Result<StageIdentity>,
    ) -> Result<Self> {
        Self::create_with_sync(directory, leaf, initialize, StoreDirectory::sync)
    }

    fn create_with_sync(
        directory: &StoreDirectory,
        leaf: &Path,
        initialize: impl FnOnce(&File) -> Result<StageIdentity>,
        sync_parent: impl FnOnce(&StoreDirectory) -> Result<()>,
    ) -> Result<Self> {
        let file = crate::platform::stage_operations::create_file(directory, leaf)?;
        let identity = match initialize(&file) {
            Ok(identity) => identity,
            Err(error) => {
                return Err(initialization_failure(
                    error,
                    crate::platform::stage_operations::discard_file(directory, leaf, &file, None),
                ));
            }
        };
        if let Err(error) = sync_parent(directory) {
            identity.verify(&file)?;
            return Err(initialization_failure(
                error,
                crate::platform::stage_operations::discard_file(
                    directory,
                    leaf,
                    &file,
                    Some(&identity),
                ),
            ));
        }
        Ok(Self {
            directory: directory.clone(),
            leaf: leaf.to_owned(),
            file,
            identity,
        })
    }

    #[must_use]
    pub fn identity(&self) -> &StageIdentity {
        &self.identity
    }

    pub fn write_chunk(&mut self, bytes: &[u8]) -> Result<()> {
        self.identity.verify(&self.file)?;
        self.file
            .write_all(bytes)
            .map_err(|e| stage_io("cannot write private stage", e))
    }

    pub fn sync(&self) -> Result<()> {
        self.identity.verify(&self.file)?;
        self.file
            .sync_all()
            .map_err(|e| stage_io("cannot sync private stage", e))
    }

    // Temporarily allow dead code until S1–S3/P0/L2/J1 adopt this method.
    // Small atomic writes deliberately keep their physical-only implementation.
    #[allow(dead_code)]
    pub(crate) fn set_readonly(&self) -> Result<()> {
        let mut permissions = self
            .file
            .metadata()
            .map_err(|e| stage_io("cannot inspect stage", e))?
            .permissions();
        permissions.set_readonly(true);
        self.file
            .set_permissions(permissions)
            .map_err(|e| stage_io("cannot protect stage", e))
    }

    /// Sync and publish only this file. The caller retires its durable journal
    /// after publication; an absent stage on recovery never authorizes deletion
    /// of the public destination.
    pub fn publish(
        self,
        target: &StoreDirectory,
        leaf: &Path,
        publication: Publication,
    ) -> Result<()> {
        self.publish_with(target, leaf, publication, || Ok(()))
    }

    fn publish_with(
        self,
        target: &StoreDirectory,
        leaf: &Path,
        publication: Publication,
        after_verified: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        if super::relative_components(leaf, false, target.path())?.len() != 1 {
            return Err(failure(
                "stage publication destination must be a direct leaf",
            ));
        }
        self.sync()?;
        let Self {
            directory,
            leaf: source,
            file,
            identity,
        } = self;
        identity.verify(&file)?;
        after_verified()?;
        crate::platform::stage_operations::publish(
            &directory,
            &source,
            &file,
            target,
            leaf,
            publication,
            &identity,
        )?;
        directory.sync()?;
        target.sync()
    }
}

impl StoreDirectory {
    /// Create a private directory stage and tag only the newly created handle.
    /// Persist the returned identity before adding any payload or publishing it.
    pub fn create_stage_directory(
        &self,
        leaf: &Path,
    ) -> Result<(StoreDirectory, DirectoryStageIdentity)> {
        self.create_stage_directory_with(leaf, |handle| {
            let mut nonce = [0; 32];
            getrandom::fill(&mut nonce)
                .map_err(|e| failure(format!("cannot generate directory stage nonce: {e}")))?;
            crate::platform::stage_nonce::create(handle, &nonce).map_err(|e| {
                nonce_error(
                    "cannot persist directory stage nonce (xattr/EA required; no fallback)",
                    e,
                )
            })?;
            Ok(nonce)
        })
    }

    fn create_stage_directory_with(
        &self,
        leaf: &Path,
        initialize: impl FnOnce(&File) -> Result<[u8; 32]>,
    ) -> Result<(StoreDirectory, DirectoryStageIdentity)> {
        if super::relative_components(leaf, false, self.path())?.len() != 1 {
            return Err(failure("stage directory must be a direct leaf"));
        }
        let handle = crate::platform::stage_nonce::create_directory(self, leaf)?;
        let result = (|| {
            let nonce = initialize(&handle)?;
            let stage = StoreDirectory::from_retained_with_control(
                handle
                    .try_clone()
                    .map_err(|e| stage_io("cannot retain new directory stage", e))?,
                self.path().join(leaf),
                self.control().clone(),
            )?;
            let identity = DirectoryStageIdentity::read(&stage)?;
            if identity.nonce != nonce {
                return Err(failure("directory stage nonce did not persist"));
            }
            stage.sync()?;
            self.sync()?;
            Ok((stage, identity))
        })();
        match result {
            Ok(stage) => Ok(stage),
            Err(error) => Err(initialization_failure(
                error,
                crate::platform::stage_operations::discard_empty_directory(self, leaf, &handle),
            )),
        }
    }

    /// Discard a crash residue only when both components of its saved identity
    /// still match. An absent name is idempotent; unknown files are preserved.
    pub fn discard_stage(&self, leaf: &Path, identity: &StageIdentity) -> Result<bool> {
        self.discard_stage_with(leaf, identity, || Ok(()))
    }

    fn discard_stage_with(
        &self,
        leaf: &Path,
        identity: &StageIdentity,
        after_verified: impl FnOnce() -> Result<()>,
    ) -> Result<bool> {
        if super::relative_components(leaf, false, self.path())?.len() != 1 {
            return Err(failure("stage must be a direct leaf"));
        }
        if crate::platform::stage_operations::recover_file(self, leaf, identity)? {
            return Ok(true);
        }
        if !self.contains_entry(leaf)? {
            // An earlier unlink may still be undurable; sync before reporting absence.
            self.sync()?;
            return Ok(false);
        }
        self.ensure_owner_private(leaf)?;
        let file = crate::platform::stage_operations::open_file(self, leaf)?;
        identity.verify(&file)?;
        after_verified()?;
        crate::platform::stage_operations::discard_file(self, leaf, &file, Some(identity))?;
        self.sync()?;
        Ok(true)
    }
}

fn initialization_failure(error: crate::KioError, cleanup: Result<()>) -> crate::KioError {
    let Err(cleanup) = cleanup else {
        return error;
    };
    let message = format!(
        "{}; initialization residue preserved: {}",
        error.message(),
        cleanup.message()
    );
    if let Some(limit) = error.resource_limit() {
        return limit.clone().into_error(message);
    }
    crate::KioError::new(
        error.error_code(),
        message,
        serde_json::json!({"initialization": error.context(), "cleanup": cleanup.context()}),
        error.exit_code(),
    )
}

/// Inspection is read-only. A tagged file without a saved identity remains
/// unidentified: nonce presence alone never grants authority to reclaim it.
#[derive(Debug, PartialEq, Eq)]
pub enum StageEntryState {
    Owned,
    /// The saved stage is in a durable Unix capture, rather than at its source.
    Captured {
        source: PathBuf,
        payload_present: bool,
    },
    Unidentified {
        reason: String,
    },
    Missing,
}
#[derive(Debug)]
pub struct StageInspectionEntry {
    pub name: std::ffi::OsString,
    pub state: StageEntryState,
}
impl StoreDirectory {
    /// Inspect a dedicated large-stage directory against the caller's saved
    /// journal identities. Unknown, unsafe and mismatching entries are reported
    /// with a reason; missing expected entries are included as well. This API
    /// never adopts, locks, tags, publishes or removes anything.
    pub fn inspect_stages(
        &self,
        files: &[(PathBuf, StageIdentity)],
        directories: &[(PathBuf, DirectoryStageIdentity)],
    ) -> Result<Vec<StageInspectionEntry>> {
        enum Expected<'a> {
            File(&'a StageIdentity),
            Directory(&'a DirectoryStageIdentity),
        }
        let mut expected = std::collections::BTreeMap::new();
        for (name, identity) in files
            .iter()
            .map(|(n, i)| (n, Expected::File(i)))
            .chain(directories.iter().map(|(n, i)| (n, Expected::Directory(i))))
        {
            if super::relative_components(name, false, self.path())?.len() != 1
                || expected
                    .insert(name.as_os_str().to_owned(), identity)
                    .is_some()
            {
                return Err(failure(
                    "stage inspection requires unique direct leaf names",
                ));
            }
        }
        let mut captures = std::collections::BTreeMap::new();
        for (source, identity) in files {
            if let Some(name) = crate::platform::stage_operations::capture_name(identity)? {
                captures.insert(name.into_os_string(), (source, identity));
            }
        }
        let mut result = Vec::new();
        // Inventory names without adopting entries or following links. The
        // general store listing rejects unsafe leaves before naming them;
        // inspection must instead report each such residue to its caller.
        let entries = cap_primitives::fs::read_base_dir(&self.root_handle())
            .map_err(|e| io_failure("cannot enumerate stage residues", e))?;
        for entry in entries {
            let name = entry
                .map_err(|e| io_failure("cannot inspect stage residue name", e))?
                .file_name();
            if let Some((source, identity)) = captures.get(&name) {
                let state = match crate::platform::stage_operations::inspect_capture(
                    self,
                    Path::new(&name),
                    identity,
                ) {
                    Ok(payload_present) => {
                        if payload_present {
                            expected.remove(source.as_os_str());
                        }
                        StageEntryState::Captured {
                            source: (*source).clone(),
                            payload_present,
                        }
                    }
                    Err(error) => StageEntryState::Unidentified {
                        reason: error.message().to_owned(),
                    },
                };
                result.push(StageInspectionEntry { name, state });
                continue;
            }
            let verification = match expected.remove(&name) {
                None => Err(failure("no saved stage identity; residue preserved")),
                Some(Expected::File(identity)) => (|| {
                    self.ensure_owner_private(Path::new(&name))?;
                    identity.verify(&self.open_regular_read(Path::new(&name), u64::MAX)?)
                })(),
                Some(Expected::Directory(identity)) => (|| {
                    let directory = StoreDirectory::from_retained_with_control(
                        self.open_directory(Path::new(&name))?,
                        self.path().join(&name),
                        self.control().clone(),
                    )?;
                    crate::private_fs::verify_private_directory_handle(&directory)?;
                    identity.verify(&directory)
                })(),
            };
            let state = match verification {
                Ok(()) => StageEntryState::Owned,
                Err(error) => StageEntryState::Unidentified {
                    reason: error.message().to_owned(),
                },
            };
            result.push(StageInspectionEntry { name, state });
        }
        result.extend(expected.into_keys().map(|name| StageInspectionEntry {
            name,
            state: StageEntryState::Missing,
        }));
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Read};

    fn directory() -> (tempfile::TempDir, StoreDirectory) {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let directory = StoreDirectory::open(temp.path()).unwrap();
        (temp, directory)
    }

    #[test]
    fn streams_and_publishes_without_tagging_existing_user_file() {
        let (temp, directory) = directory();
        fs::write(temp.path().join("user"), b"previous").unwrap();
        let user = directory.open_regular_read(Path::new("user"), 64).unwrap();
        assert!(crate::platform::stage_nonce::read(&user).is_err());
        let mut writer = StageWriter::create(&directory, Path::new("stage")).unwrap();
        let identity = writer.identity().clone();
        let encoded = serde_jcs::to_vec(&identity).unwrap();
        assert_eq!(
            serde_json::from_slice::<StageIdentity>(&encoded).unwrap(),
            identity
        );
        for part in [b"one".as_slice(), b"", b"two", b"three"] {
            writer.write_chunk(part).unwrap();
        }
        writer
            .publish(&directory, Path::new("published"), Publication::CreateOnly)
            .unwrap();
        let mut published = directory
            .open_regular_read(Path::new("published"), 64)
            .unwrap();
        identity.verify(&published).unwrap();
        let mut bytes = Vec::new();
        published.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"onetwothree");
        assert!(
            !directory
                .discard_stage(Path::new("stage"), &identity)
                .unwrap()
        );
        assert_eq!(fs::read(temp.path().join("user")).unwrap(), b"previous");
        assert!(crate::platform::stage_nonce::read(&user).is_err());
    }

    #[test]
    fn same_bytes_and_simulated_physical_id_reuse_do_not_establish_ownership() {
        let (temp, directory) = directory();
        let mut first = StageWriter::create(&directory, Path::new("first")).unwrap();
        first.write_chunk(b"same").unwrap();
        let mut second = StageWriter::create(&directory, Path::new("second")).unwrap();
        second.write_chunk(b"same").unwrap();
        let mut expected = first.identity().clone();
        assert!(expected.verify(&second.file).is_err());
        // Simulate inode/file-index reuse by making the saved physical ID match.
        expected.physical = second.identity.physical.clone();
        assert!(
            expected
                .verify(&second.file)
                .unwrap_err()
                .message()
                .contains("nonce")
        );
        assert!(
            directory
                .discard_stage(Path::new("second"), &expected)
                .is_err()
        );
        assert_eq!(fs::read(temp.path().join("second")).unwrap(), b"same");
        drop(second);
        directory
            .discard_stage(Path::new("first"), first.identity())
            .unwrap();
    }

    #[test]
    fn untagged_replacement_with_simulated_reused_id_is_preserved() {
        let (temp, directory) = directory();
        let writer = StageWriter::create(&directory, Path::new("stage")).unwrap();
        let mut expected = writer.identity().clone();
        drop(writer);
        fs::rename(temp.path().join("stage"), temp.path().join("original")).unwrap();
        let file = directory.create_private_file(Path::new("stage")).unwrap();
        expected.physical = file_id(&file).unwrap();
        drop(file);
        assert!(
            directory
                .discard_stage(Path::new("stage"), &expected)
                .is_err()
        );
        assert!(temp.path().join("stage").exists());
    }

    #[test]
    fn unsupported_nonce_initialization_fails_without_fallback_and_cleans_created_handle() {
        let (temp, directory) = directory();
        let result = StageWriter::create_with(&directory, Path::new("stage"), |_| {
            Err(nonce_error(
                "xattr/EA unsupported; no fallback",
                std::io::Error::from(std::io::ErrorKind::Unsupported),
            ))
        });
        let error = result.err().unwrap();
        assert_eq!(error.error_code(), "KIO-E-STAGE-NONCE-001");
        assert!(error.message().contains("unsupported; no fallback"));
        assert!(!temp.path().join("stage").exists());
        assert!(directory.entries(Path::new("")).unwrap().is_empty());
    }

    #[test]
    fn interrupted_creation_and_partial_stream_can_be_reclaimed_from_saved_identity() {
        for chunks in [0, 1, 3] {
            let (_temp, directory) = directory();
            let mut writer = StageWriter::create(&directory, Path::new("stage")).unwrap();
            let expected = writer.identity().clone();
            for _ in 0..chunks {
                writer.write_chunk(b"partial").unwrap();
            }
            drop(writer); // no destructors that remove or publish a stage
            assert!(
                directory
                    .discard_stage(Path::new("stage"), &expected)
                    .unwrap()
            );
            assert!(
                !directory
                    .discard_stage(Path::new("stage"), &expected)
                    .unwrap()
            );
        }
    }

    #[test]
    fn replaced_stage_is_never_published_by_retained_writer() {
        let (temp, directory) = directory();
        let mut writer = StageWriter::create(&directory, Path::new("stage")).unwrap();
        writer.write_chunk(b"expected").unwrap();
        fs::rename(temp.path().join("stage"), temp.path().join("retained")).unwrap();
        let mut replacement = StageWriter::create(&directory, Path::new("stage")).unwrap();
        replacement.write_chunk(b"expected").unwrap();
        assert!(
            writer
                .publish(&directory, Path::new("public"), Publication::CreateOnly)
                .is_err()
        );
        assert!(!temp.path().join("public").exists());
        assert!(temp.path().join("stage").exists());
    }

    #[test]
    fn directory_stage_rejects_physical_id_reuse_and_untagged_replacement() {
        let (temp, parent) = directory();
        let (first, mut expected) = parent.create_stage_directory(Path::new("first")).unwrap();
        let (second, actual) = parent.create_stage_directory(Path::new("second")).unwrap();
        expected.verify(&first).unwrap();
        assert!(expected.verify(&second).is_err());
        expected.physical = actual.physical;
        assert!(expected.verify(&second).is_err());
        assert!(temp.path().join("second").is_dir());
        let untagged = StoreDirectory::from_retained(
            parent.create_directory(Path::new("untagged")).unwrap(),
            temp.path().join("untagged"),
        )
        .unwrap();
        expected.physical =
            crate::management::directory_identity_from_handle(&untagged.root_handle()).unwrap();
        assert!(expected.verify(&untagged).is_err());
    }

    #[test]
    fn replacement_after_verification_is_never_published_or_deleted() {
        for publication in [
            Publication::CreateOnly,
            Publication::Replace,
            Publication::Upsert,
        ] {
            let (temp, directory) = directory();
            let mut writer = StageWriter::create(&directory, Path::new("stage")).unwrap();
            writer.write_chunk(b"ours").unwrap();
            if publication != Publication::CreateOnly {
                fs::write(temp.path().join("public"), b"previous").unwrap();
            }
            let result = writer.publish_with(&directory, Path::new("public"), publication, || {
                fs::rename(temp.path().join("stage"), temp.path().join("displaced")).unwrap();
                let mut foreign = directory.create_private_file(Path::new("stage"))?;
                foreign.write_all(b"foreign").unwrap();
                Ok(())
            });
            if let Ok(bytes) = fs::read(temp.path().join("public")) {
                assert_ne!(bytes, b"foreign");
            }
            assert_eq!(fs::read(temp.path().join("stage")).unwrap(), b"foreign");
            if result.is_err() && publication != Publication::CreateOnly {
                assert_eq!(fs::read(temp.path().join("public")).unwrap(), b"previous");
            }
        }
    }

    #[test]
    fn replacement_after_verification_is_never_deleted() {
        let (temp, directory) = directory();
        let writer = StageWriter::create(&directory, Path::new("stage")).unwrap();
        let identity = writer.identity().clone();
        drop(writer);
        let _ = directory.discard_stage_with(Path::new("stage"), &identity, || {
            fs::rename(temp.path().join("stage"), temp.path().join("displaced")).unwrap();
            let mut foreign = directory.create_private_file(Path::new("stage"))?;
            foreign.write_all(b"foreign").unwrap();
            Ok(())
        });
        assert_eq!(fs::read(temp.path().join("stage")).unwrap(), b"foreign");
    }

    #[test]
    fn directory_initialization_error_cleans_only_its_new_empty_directory() {
        for reason in [
            "random generation failed",
            "EA unsupported",
            "nonce verification failed",
        ] {
            let (temp, directory) = directory();
            let error = directory
                .create_stage_directory_with(Path::new("stage"), |_| Err(failure(reason)))
                .unwrap_err();
            assert_eq!(error.message(), reason);
            assert!(!temp.path().join("stage").exists());
            assert!(directory.entries(Path::new("")).unwrap().is_empty());
        }
    }

    #[test]
    fn inspection_reports_untagged_unknown_mismatching_and_missing_residue_without_mutation() {
        let (temp, directory) = directory();
        let writer = StageWriter::create(&directory, Path::new("owned")).unwrap();
        let expected = writer.identity().clone();
        drop(writer);
        directory
            .create_private_file(Path::new("unidentified"))
            .unwrap();
        for name in ["unknown", "mismatch"] {
            let mut other = StageWriter::create(&directory, Path::new(name)).unwrap();
            other.write_chunk(b"preserve").unwrap();
        }
        let (_child, child_id) = directory
            .create_stage_directory(Path::new("directory"))
            .unwrap();
        let result = directory
            .inspect_stages(
                &[
                    (PathBuf::from("owned"), expected.clone()),
                    (PathBuf::from("mismatch"), expected.clone()),
                    (PathBuf::from("missing"), expected),
                ],
                &[(PathBuf::from("directory"), child_id)],
            )
            .unwrap();
        assert_eq!(result.len(), 6);
        for name in ["owned", "directory"] {
            assert!(
                result
                    .iter()
                    .any(|e| e.name == name && e.state == StageEntryState::Owned)
            );
        }
        for name in ["unidentified", "unknown", "mismatch"] {
            assert!(result.iter().any(|e| e.name == name && matches!(e.state, StageEntryState::Unidentified { ref reason } if !reason.is_empty())));
            assert!(temp.path().join(name).exists());
        }
        assert!(
            result
                .iter()
                .any(|e| e.name == "missing" && e.state == StageEntryState::Missing)
        );
        assert_eq!(fs::read(temp.path().join("mismatch")).unwrap(), b"preserve");
    }

    #[test]
    fn directory_verification_failure_also_cleans_the_new_empty_directory() {
        let (temp, directory) = directory();
        assert!(
            directory
                .create_stage_directory_with(Path::new("stage"), |_| Ok([0; 32]))
                .is_err()
        );
        assert!(!temp.path().join("stage").exists());
    }

    #[test]
    fn initialization_cleanup_preserves_nonempty_and_replaced_directories() {
        let (temp, directory) = directory();
        let error = directory
            .create_stage_directory_with(Path::new("stage"), |_| {
                fs::write(temp.path().join("stage/sentinel"), b"keep").unwrap();
                Err(failure("initialization failed"))
            })
            .unwrap_err();
        assert!(error.message().contains("residue preserved"));
        assert_eq!(
            fs::read(temp.path().join("stage/sentinel")).unwrap(),
            b"keep"
        );

        let error = directory
            .create_stage_directory_with(Path::new("other"), |_| {
                fs::rename(temp.path().join("other"), temp.path().join("displaced")).unwrap();
                directory.create_directory(Path::new("other"))?;
                fs::write(temp.path().join("other/foreign"), b"keep").unwrap();
                Err(failure("initialization failed"))
            })
            .unwrap_err();
        assert!(error.message().contains("initialization failed"));
        assert_eq!(
            fs::read(temp.path().join("other/foreign")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn inspection_rejects_conflicting_or_nonlocal_expected_names() {
        let (_temp, directory) = directory();
        let writer = StageWriter::create(&directory, Path::new("stage")).unwrap();
        let id = writer.identity().clone();
        assert!(
            directory
                .inspect_stages(
                    &[
                        (PathBuf::from("stage"), id.clone()),
                        (PathBuf::from("stage"), id.clone())
                    ],
                    &[]
                )
                .is_err()
        );
        assert!(
            directory
                .inspect_stages(&[(PathBuf::from("../stage"), id)], &[])
                .is_err()
        );
    }

    #[test]
    fn private_readonly_stage_can_be_reclaimed_using_its_verified_handle() {
        let (temp, directory) = directory();
        let mut writer = StageWriter::create(&directory, Path::new("stage")).unwrap();
        writer.write_chunk(b"owned").unwrap();
        writer.set_readonly().unwrap();
        writer.sync().unwrap();
        let expected = writer.identity().clone();
        drop(writer);
        assert!(
            directory
                .discard_stage(Path::new("stage"), &expected)
                .unwrap()
        );
        assert!(!temp.path().join("stage").exists());
        assert!(directory.entries(Path::new("")).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn inspection_reports_symlinks_without_following_them() {
        let (temp, directory) = directory();
        let writer = StageWriter::create(&directory, Path::new("owned")).unwrap();
        let expected = writer.identity().clone();
        std::os::unix::fs::symlink(temp.path().join("owned"), temp.path().join("link")).unwrap();
        let result = directory
            .inspect_stages(&[(PathBuf::from("link"), expected)], &[])
            .unwrap();
        assert!(
            result.iter().any(
                |e| e.name == "link" && matches!(e.state, StageEntryState::Unidentified { .. })
            )
        );
        assert!(
            temp.path()
                .join("link")
                .symlink_metadata()
                .unwrap()
                .is_symlink()
        );
        assert!(temp.path().join("owned").is_file());
    }

    #[test]
    fn small_atomic_writes_do_not_require_nonce_metadata() {
        let (_temp, directory) = directory();
        directory
            .write_atomic(Path::new("public"), b"small", Publication::CreateOnly)
            .unwrap();
        let file = directory
            .open_regular_read(Path::new("public"), 64)
            .unwrap();
        assert!(crate::platform::stage_nonce::read(&file).is_err());
    }

    #[test]
    fn parent_sync_failure_cleans_the_initialized_file_before_returning_error() {
        let (temp, directory) = directory();
        let result = StageWriter::create_with_sync(
            &directory,
            Path::new("stage"),
            StageIdentity::initialize,
            |_| Err(failure("injected parent fsync failure")),
        );
        assert_eq!(
            result.err().unwrap().message(),
            "injected parent fsync failure"
        );
        assert!(!temp.path().join("stage").exists());
        assert!(directory.entries(Path::new("")).unwrap().is_empty());
        assert!(StageWriter::create(&directory, Path::new("stage")).is_ok());
    }

    #[test]
    fn parent_sync_failure_never_cleans_a_replacement_file() {
        let (temp, directory) = directory();
        let result = StageWriter::create_with_sync(
            &directory,
            Path::new("stage"),
            StageIdentity::initialize,
            |_| {
                fs::rename(temp.path().join("stage"), temp.path().join("displaced")).unwrap();
                let mut foreign = directory.create_private_file(Path::new("stage"))?;
                foreign.write_all(b"foreign").unwrap();
                Err(failure("injected parent fsync failure"))
            },
        );
        assert!(
            result
                .err()
                .unwrap()
                .message()
                .contains("injected parent fsync failure")
        );
        assert_eq!(fs::read(temp.path().join("stage")).unwrap(), b"foreign");
    }

    #[cfg(unix)]
    #[test]
    fn unremovable_initialization_residue_is_reported_by_inspection() {
        use std::os::unix::fs::PermissionsExt;
        let (temp, directory) = directory();
        let result = StageWriter::create_with_sync(
            &directory,
            Path::new("stage"),
            StageIdentity::initialize,
            |_| {
                fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o500)).unwrap();
                Err(failure("parent sync failed"))
            },
        );
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert!(temp.path().join("stage").exists());
        let inventory = directory.inspect_stages(&[], &[]).unwrap();
        assert!(
            inventory
                .iter()
                .any(|e| e.name == "stage"
                    && matches!(e.state, StageEntryState::Unidentified { .. }))
        );
    }

    #[test]
    fn stage_creation_storage_failure_preserves_resource_type() {
        let error = crate::store_dir::ioerr(
            Path::new("stage"),
            std::io::Error::from(std::io::ErrorKind::StorageFull),
        );
        assert_eq!(error.resource_limit().unwrap().resource, Resource::Storage);
    }

    #[test]
    fn storage_full_is_typed_resource_exhaustion() {
        let error = stage_io(
            "write",
            std::io::Error::from(std::io::ErrorKind::StorageFull),
        );
        assert_eq!(error.resource_limit().unwrap().resource, Resource::Storage);
        assert!(
            stage_io(
                "write",
                std::io::Error::from(std::io::ErrorKind::InvalidData)
            )
            .resource_limit()
            .is_none()
        );
    }
}
