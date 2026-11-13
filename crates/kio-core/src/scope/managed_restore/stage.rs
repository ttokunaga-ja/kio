//! A restore stage is owned by the restore journal, including incomplete
//! payloads. Creation uses the shared nonce API; only verified complete bytes
//! can be published, and cleanup never derives ownership from their digest.

use super::*;
use crate::store_dir::{StageIdentity, StageWriter};
use std::io::{self, Write};

fn stage_leaf(index: usize) -> String {
    format!("managed-restore-stage-{index}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RestoreStage {
    path: String,
    raw_hash: String,
    length: u64,
    identity: StageIdentity,
}

impl RestoreStage {
    pub(super) fn validate(&self, changes: &[ManagedRestoreChange]) -> Result<()> {
        if !changes.iter().any(|change| {
            change.path == self.path
                && (change.old_raw_hash.as_ref() == Some(&self.raw_hash)
                    || change.new_raw_hash.as_ref() == Some(&self.raw_hash))
        }) {
            return Err(restore_incomplete(
                "restore stage does not match journal changes",
            ));
        }
        Ok(())
    }
}

// Preserve the original typed error (especially resource_limit(storage)) when
// adapting the streaming CAS writer interface to StageWriter.
struct StageSink<'a> {
    writer: &'a mut StageWriter,
    error: Option<KioError>,
}
impl Write for StageSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self.writer.write_chunk(bytes) {
            Ok(()) => Ok(bytes.len()),
            Err(error) => {
                self.error = Some(error);
                Err(io::Error::other("restore stage write failed"))
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Repository {
    pub(super) fn restore_stage_present(&self, journal: &ManagedRestoreJournal) -> Result<bool> {
        let Some(stage) = &journal.stages[journal.active] else {
            return Ok(false);
        };
        let owner = self.store_directory()?;
        let leaf = stage_leaf(journal.active);
        let capture = crate::platform::stage_operations::capture_name(&stage.identity)?;
        let entries =
            owner.inspect_stages(&[(leaf.clone().into(), stage.identity.clone())], &[])?;
        for entry in entries {
            use crate::store_dir::StageEntryState;
            match entry.state {
                StageEntryState::Owned => return Ok(true),
                StageEntryState::Captured {
                    payload_present: true,
                    ..
                } => return Ok(true),
                StageEntryState::Unidentified { reason }
                    if entry.name == leaf.as_str()
                        || capture
                            .as_ref()
                            .is_some_and(|name| name.as_os_str() == entry.name) =>
                {
                    return Err(restore_incomplete(format!(
                        "unverified restore stage: {reason}"
                    )));
                }
                _ => {}
            }
        }
        Ok(false)
    }

    pub(super) fn verify_restore_destination(
        &self,
        journal: &ManagedRestoreJournal,
        index: usize,
        path: &str,
    ) -> Result<()> {
        let stage = journal.stages[index].as_ref().ok_or_else(|| {
            restore_incomplete("published restore file has no saved stage identity")
        })?;
        let file = self
            .root_directory()?
            .open_regular_read(Path::new(path), u64::MAX)?;
        stage
            .identity
            .verify(&file)
            .map_err(|_| restore_incomplete("restored file was replaced by another writer"))
    }

    pub(super) fn remove_restore_destination(
        &self,
        journal: &ManagedRestoreJournal,
        change: &ManagedRestoreChange,
        raw: &str,
    ) -> Result<()> {
        let stage = journal.stages[journal.active]
            .as_ref()
            .ok_or_else(|| restore_incomplete("missing restore identity"))?;
        let length = self.store.inspect_object(ObjectKind::Raw, raw)?.size_bytes;
        self.root_directory()?.remove_hash_verified(
            &self.store_directory()?,
            Path::new(&change.path),
            raw,
            length,
            |file| stage.identity.verify(file),
        )
    }

    pub(super) fn discard_restore_stage(&self, journal: &ManagedRestoreJournal) -> Result<()> {
        for (index, stage) in journal.stages.iter().enumerate() {
            if let Some(stage) = stage {
                self.store_directory()?
                    .discard_stage(Path::new(&stage_leaf(index)), &stage.identity)?;
            }
        }
        Ok(())
    }

    pub(super) fn write_restore_stage<P: FnMut(&ManagedRestoreStatus) -> Result<()>>(
        &self,
        change: &ManagedRestoreChange,
        raw_hash: &str,
        journal: &mut ManagedRestoreJournal,
        bytes: &mut Vec<u8>,
        observe: &mut P,
    ) -> Result<()> {
        if let Some(stage) = &journal.stages[journal.active] {
            self.store_directory()?
                .discard_stage(Path::new(&stage_leaf(journal.active)), &stage.identity)?;
        }
        let length = ensure_raw_present(self, raw_hash)?;
        let owner = self.store_directory()?;
        let leaf = stage_leaf(journal.active);
        let mut writer = StageWriter::create(&owner, Path::new(&leaf))?;
        journal.stages[journal.active] = Some(RestoreStage {
            path: change.path.clone(),
            raw_hash: raw_hash.to_owned(),
            length,
            identity: writer.identity().clone(),
        });
        journal.progress[journal.active] = if change.new_raw_hash.as_deref() == Some(raw_hash) {
            RestoreProgress::PublishingNew
        } else {
            RestoreProgress::RestoringOld
        };
        // No payload may be written before the recovery authority is durable.
        self.persist_restore_progress(journal, bytes)?;
        observe(&journal.status())?;
        let mut sink = StageSink {
            writer: &mut writer,
            error: None,
        };
        let copied = self
            .store
            .copy_object_to(ObjectKind::Raw, raw_hash, &mut sink);
        if let Some(error) = sink.error {
            return Err(error);
        }
        if copied?.size_bytes != length {
            return Err(restore_incomplete("restore source changed while staging"));
        }
        writer.sync()?;
        self.control()
            .checkpoint(crate::durability::DurabilityPoint::RestoreStageReady)?;
        let stage = journal.stages[journal.active]
            .as_ref()
            .expect("journaled stage");
        let mut file = owner.open_regular_read(Path::new(&leaf), u64::MAX)?;
        stage.identity.verify(&file)?;
        let (digest, size) = hash_scope_file(
            &mut file,
            Path::new(&leaf),
            &leaf,
            u64::MAX,
            ArchiveLimits::default(),
        )?;
        if digest != stage.raw_hash || size != stage.length {
            return Err(restore_incomplete("restore stage digest or length changed"));
        }
        writer.publish(
            &self.root_directory()?,
            Path::new(&change.path),
            Publication::CreateOnly,
        )?;
        self.control()
            .checkpoint(crate::durability::DurabilityPoint::RestoreFilePublished)
    }
}
