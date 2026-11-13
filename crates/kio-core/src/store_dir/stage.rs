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
        let file = directory.create_private_file(leaf)?;
        let physical = file_id(&file)?;
        let identity = match initialize(&file) {
            Ok(identity) => identity,
            Err(error) => {
                // Retain the original identity while opening a read handle,
                // then close the writer before removal (Windows sharing).
                // Keeping the read handle prevents physical-ID reuse during
                // cleanup even when nonce initialization itself failed.
                let current = directory.open_regular_read(leaf, u64::MAX).ok();
                drop(file);
                if let Some(current) = current
                    && file_id(&current).ok().as_ref() == Some(&physical)
                {
                    let _ = crate::platform::atomic_semantics::remove_publication_residue(
                        directory, leaf,
                    );
                }
                return Err(error);
            }
        };
        directory.sync()?;
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
        self.sync()?;
        let Self {
            directory,
            leaf: source,
            file,
            identity,
        } = self;
        drop(file);
        let current = directory.open_regular_read(&source, u64::MAX)?;
        identity.verify(&current)?;
        drop(current);
        directory.rename_regular_between_raw(&source, target, leaf, publication)?;
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
        if super::relative_components(leaf, false, self.path())?.len() != 1 {
            return Err(failure("stage directory must be a direct leaf"));
        }
        let handle = crate::platform::stage_nonce::create_directory(self, leaf)?;
        let mut nonce = [0; 32];
        getrandom::fill(&mut nonce)
            .map_err(|e| failure(format!("cannot generate directory stage nonce: {e}")))?;
        crate::platform::stage_nonce::create(&handle, &nonce).map_err(|e| {
            nonce_error(
                "cannot persist directory stage nonce (xattr/EA required; no fallback)",
                e,
            )
        })?;
        let stage = StoreDirectory::from_retained_with_control(
            handle,
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
    }

    /// Discard a crash residue only when both components of its saved identity
    /// still match. An absent name is idempotent; unknown files are preserved.
    pub fn discard_stage(&self, leaf: &Path, identity: &StageIdentity) -> Result<bool> {
        if !self.contains_entry(leaf)? {
            return Ok(false);
        }
        self.ensure_owner_private(leaf)?;
        let file = self.open_regular_read(leaf, u64::MAX)?;
        identity.verify(&file)?;
        drop(file);
        crate::platform::atomic_semantics::remove_publication_residue(self, leaf)?;
        self.sync()?;
        Ok(true)
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
    fn unsupported_nonce_initialization_fails_without_fallback_and_closes_writer_before_cleanup() {
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
    fn atomic_stream_callback_failure_preserves_destination_and_recovers_partial_stage() {
        let (temp, directory) = directory();
        let error = directory
            .write_atomic_stream_with_owner(
                &directory,
                Path::new("public"),
                Publication::CreateOnly,
                |writer| {
                    writer.write_chunk(b"partial")?;
                    Err(crate::KioError::invalid_usage("interrupted producer"))
                },
            )
            .unwrap_err();
        assert_eq!(error.message(), "interrupted producer");
        assert!(!temp.path().join("public").exists());
        directory.recover_atomic(&[&directory]).unwrap();
        directory
            .write_atomic_stream_with_owner(
                &directory,
                Path::new("public"),
                Publication::CreateOnly,
                |writer| {
                    for _ in 0..3 {
                        writer.write_chunk(b"chunk")?;
                    }
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(
            fs::read(temp.path().join("public")).unwrap(),
            b"chunkchunkchunk"
        );
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
