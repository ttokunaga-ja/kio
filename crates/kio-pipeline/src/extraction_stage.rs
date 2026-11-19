//! Journal and payload operations for private extraction files.

use kio_core::scope::new_ulid;
use kio_core::store_dir::{Publication, StageIdentity, StageWriter, StoreDirectory};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};

use crate::{PipelineError, Result};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedStage {
    leaf: PathBuf,
    identity: StageIdentity,
}

impl SavedStage {
    /// The journal containing this identity must reach disk before writing bytes.
    pub(crate) fn create(directory: &StoreDirectory) -> Result<(Self, StageWriter)> {
        let leaf = PathBuf::from(format!("payload-{}", new_ulid(directory.path())));
        let writer =
            StageWriter::create(directory, &leaf).map_err(|error| stage_error(directory, error))?;
        let saved = Self {
            leaf,
            identity: writer.identity().clone(),
        };
        Ok((saved, writer))
    }

    fn validate_name(&self, directory: &StoreDirectory) -> Result<()> {
        if self.leaf.components().count() != 1
            || !self.leaf.to_str().is_some_and(|name| {
                name.strip_prefix("payload-").is_some_and(|suffix| {
                    suffix.len() == 26 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
                })
            })
        {
            return Err(PipelineError::corrupt(
                directory.path().display().to_string(),
                "invalid extraction stage name",
            ));
        }
        Ok(())
    }

    pub(crate) fn publish_content(
        &self,
        directory: &StoreDirectory,
        store: &kio_core::cas::ObjectStore,
        kind: kio_core::cas::ContentObjectKind,
        hash: &str,
    ) -> Result<()> {
        publish_content(directory, store, kind, hash, Some(self)).map(|_| ())
    }

    fn source(&self, directory: &StoreDirectory) -> Result<(&Path, &StageIdentity)> {
        self.validate_name(directory)?;
        Ok((&self.leaf, &self.identity))
    }

    #[cfg(test)]
    pub(crate) fn open(&self, directory: &StoreDirectory, limit: u64) -> Result<File> {
        self.validate_name(directory)?;
        directory
            .ensure_owner_private(&self.leaf)
            .map_err(|error| stage_error(directory, error))?;
        let file = directory
            .open_regular_read(&self.leaf, limit)
            .map_err(|error| stage_error(directory, error))?;
        self.identity
            .verify(&file)
            .map_err(|error| stage_error(directory, error))?;
        Ok(file)
    }

    pub(crate) fn discard(&self, directory: &StoreDirectory) -> Result<()> {
        self.validate_name(directory)?;
        // discard_stage revalidates both parts of the saved identity and
        // recovers a captured file if a previous discard was interrupted.
        directory
            .discard_stage(&self.leaf, &self.identity)
            .map(|_| ())
            .map_err(|error| stage_error(directory, error))
    }
}

pub(crate) fn publish_content(
    directory: &StoreDirectory,
    store: &kio_core::cas::ObjectStore,
    kind: kio_core::cas::ContentObjectKind,
    hash: &str,
    stage: Option<&SavedStage>,
) -> Result<bool> {
    store
        .publish_content_stage(
            kind,
            hash,
            directory,
            stage.map(|stage| stage.source(directory)).transpose()?,
        )
        .map_err(|error| stage_error(directory, error))
}

pub(crate) fn publish_file(
    directory: &StoreDirectory,
    target: &StoreDirectory,
    leaf: &Path,
    stage: Option<&SavedStage>,
    verify: impl FnOnce(&mut File) -> Result<()>,
) -> Result<bool> {
    directory
        .publish_or_adopt_stage(
            target,
            leaf,
            stage.map(|stage| stage.source(directory)).transpose()?,
            |file| {
                verify(file).map_err(|error| match error {
                    PipelineError::ResourceLimit(limit) => limit.into_error("stage verification"),
                    error => kio_core::KioError::schema(error.to_string()),
                })
            },
        )
        .map_err(|error| stage_error(directory, error))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PayloadRecord {
    stage: SavedStage,
    ready: bool,
}

pub(crate) fn publish_payload(
    workspace: &StoreDirectory,
    journal: &Path,
    write: impl FnOnce(&mut StageWriter) -> Result<()>,
    before_publish: impl FnOnce() -> Result<()>,
    mut publish: impl FnMut(Option<&SavedStage>) -> Result<bool>,
) -> Result<()> {
    let adopted = if let Some(saved) = read_record::<PayloadRecord>(workspace, journal, 4096)? {
        if saved.ready {
            publish(Some(&saved.stage))?;
            return remove_record(workspace, journal);
        }
        let adopted = publish(None)?;
        saved.stage.discard(workspace)?;
        remove_record(workspace, journal)?;
        adopted
    } else {
        publish(None)?
    };
    if adopted {
        return Ok(());
    }
    let (stage, mut writer) = SavedStage::create(workspace)?;
    let mut saved = PayloadRecord {
        stage,
        ready: false,
    };
    write_record(workspace, journal, &saved, Publication::CreateOnly)?;
    write(&mut writer)?;
    writer
        .sync()
        .map_err(|error| stage_error(workspace, error))?;
    drop(writer);
    saved.ready = true;
    write_record(workspace, journal, &saved, Publication::Upsert)?;
    before_publish()?;
    publish(Some(&saved.stage))?;
    remove_record(workspace, journal)
}

pub(crate) fn discard_payload(workspace: &StoreDirectory, journal: &Path) -> Result<()> {
    if let Some(saved) = read_record::<PayloadRecord>(workspace, journal, 4096)? {
        saved.stage.discard(workspace)?;
        remove_record(workspace, journal)?;
    }
    Ok(())
}

fn remove_record(workspace: &StoreDirectory, journal: &Path) -> Result<()> {
    workspace
        .remove_file(journal)
        .map_err(|error| stage_error(workspace, error))
}

pub(crate) fn child_directory(directory: &StoreDirectory, path: &Path) -> Result<StoreDirectory> {
    let handle = directory
        .create_directory_all(path)
        .map_err(|error| stage_error(directory, error))?;
    let child = StoreDirectory::from_retained_with_control(
        handle,
        directory.path().join(path),
        directory.control().clone(),
    )
    .map_err(|error| stage_error(directory, error))?;
    kio_core::private_fs::verify_private_directory_handle(&child)
        .map_err(|error| stage_error(&child, error))?;
    Ok(child)
}

pub(crate) fn read_record<T: for<'de> Deserialize<'de>>(
    directory: &StoreDirectory,
    leaf: &Path,
    limit: u64,
) -> Result<Option<T>> {
    directory
        .read_optional(leaf, limit)
        .map_err(|error| stage_error(directory, error))?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|error| {
                PipelineError::corrupt(
                    directory.path().join(leaf).display().to_string(),
                    error.to_string(),
                )
            })
        })
        .transpose()
}

pub(crate) fn write_record(
    directory: &StoreDirectory,
    leaf: &Path,
    record: &impl Serialize,
    publication: Publication,
) -> Result<()> {
    let bytes =
        serde_json::to_vec(record).map_err(|error| PipelineError::Schema(error.to_string()))?;
    directory
        .write_atomic(leaf, &bytes, publication)
        .map_err(|error| stage_error(directory, error))
}

pub(crate) fn stage_error(directory: &StoreDirectory, error: kio_core::KioError) -> PipelineError {
    if let Some(limit) = error.resource_limit() {
        return PipelineError::ResourceLimit(limit.clone());
    }
    if error.error_code() == "KIO-E-STAGE-NONCE-001" {
        return PipelineError::contract("KIO-E-STAGE-NONCE-001", error.to_string());
    }
    PipelineError::corrupt(directory.path().display().to_string(), error.to_string())
}

/// Visit only a raw object's extraction workspaces through the retained store.
pub(crate) fn visit_raw_workspaces(
    root: &StoreDirectory,
    stage_kind: &str,
    raw_hash: &str,
    mut visit: impl FnMut(&StoreDirectory) -> Result<()>,
) -> Result<()> {
    if !kio_core::cas::is_hash(raw_hash) {
        return Err(PipelineError::Schema(
            "invalid extraction workspace raw hash".to_owned(),
        ));
    }
    let relative = Path::new("extraction-stages")
        .join(stage_kind)
        .join(raw_hash.trim_start_matches("sha256:"));
    let Some(entries) = root
        .entries_optional(&relative)
        .map_err(|error| stage_error(root, error))?
    else {
        return Ok(());
    };
    for entry in entries {
        if !entry.is_directory
            || !entry.name.to_str().is_some_and(|name| {
                name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            continue;
        }
        let path = relative.join(entry.name);
        let handle = root
            .open_directory(&path)
            .map_err(|error| stage_error(root, error))?;
        let workspace = StoreDirectory::from_retained_with_control(
            handle,
            root.path().join(path),
            root.control().clone(),
        )
        .map_err(|error| stage_error(root, error))?;
        kio_core::private_fs::verify_private_directory_handle(&workspace)
            .map_err(|error| stage_error(&workspace, error))?;
        visit(&workspace)?;
    }
    Ok(())
}
