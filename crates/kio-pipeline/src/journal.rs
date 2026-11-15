//! Atomic replacement of rebuildable projections. Readers only use the canonical
//! file; recovery discards an interrupted candidate and never replays stale state.
use std::path::Path;

use kio_core::store_dir::{Publication, StageIdentity, StageWriter, StoreDirectory};
use serde::{Deserialize, Serialize};

use crate::{PipelineError, Result};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    stage: String,
    identity: StageIdentity,
}

pub(crate) fn error(path: &Path, error: kio_core::KioError) -> PipelineError {
    match error.resource_limit() {
        Some(limit) => PipelineError::ResourceLimit(limit.clone()),
        None => PipelineError::corrupt(path.display().to_string(), error.to_string()),
    }
}

pub(crate) fn verify_target(directory: &StoreDirectory, leaf: &Path) -> kio_core::Result<()> {
    kio_core::private_fs::verify_private_directory_handle(directory)?;
    if directory.contains_entry(leaf)? {
        let file = directory.open_regular_read(leaf, u64::MAX)?;
        kio_core::private_fs::verify_private_regular_file_handle(&file)?;
    }
    Ok(())
}

/// Only called under the surrounding repository writer lock. An absent stage
/// means publication may already have happened: never remove the destination.
pub(crate) fn recover(directory: &StoreDirectory, leaf: &Path) -> Result<()> {
    let result = (|| {
        let prefix = format!(".{}.compaction-", leaf.display());
        for entry in directory.entries(Path::new(""))? {
            let Some(name) = entry.name.to_str() else {
                continue;
            };
            let Some(suffix) = name.strip_prefix(&prefix) else {
                continue;
            };
            if suffix.len() != 64 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(kio_core::KioError::schema("invalid compaction intent name"));
            }
            let marker = Path::new(name);
            verify_target(directory, marker)?;
            let Some(bytes) = directory.read_optional(marker, 4096)? else {
                continue;
            };
            let intent: Intent = serde_json::from_slice(&bytes).map_err(|e| {
                kio_core::KioError::schema(format!("invalid compaction intent: {e}"))
            })?;
            if intent.stage != format!(".{}.stage-{suffix}", leaf.display()) {
                return Err(kio_core::KioError::schema("invalid compaction stage name"));
            }
            // A copy has different physical identities. Check before calling
            // reclamation (which may restore private permissions on owned files).
            // Preserve unidentified residues and their saved authority; they
            // cannot be replayed and do not prevent a fresh replacement.
            let stage = Path::new(&intent.stage);
            let cleanup = (|| {
                if directory.contains_entry(stage)? {
                    let file = directory.open_regular_read(stage, u64::MAX)?;
                    intent.identity.verify(&file)?;
                }
                directory.discard_stage(stage, &intent.identity)?;
                directory.remove_file(marker)
            })();
            match cleanup {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.error_code(),
                        "KIO-E-ATOMIC-WORKSPACE-UNSAFE-001"
                            | "KIO-E-STAGE-NONCE-001"
                            | "KIO-E-STORE-UNSAFE-001"
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    })();
    result.map_err(|e| error(&directory.path().join(leaf), e))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Created,
    Recorded,
    ChunkWritten,
    Synced,
    Published,
}

pub(crate) fn replace(
    directory: &StoreDirectory,
    leaf: &Path,
    records: impl IntoIterator<Item = Result<Vec<u8>>>,
) -> Result<()> {
    // Keep small writes on the existing atomic path; only large replacements
    // require the nonce-tagged, recoverable stage API.
    const SMALL_WRITE_BYTES: usize = 1024 * 1024;
    let mut records = records.into_iter();
    let mut bytes = Vec::new();
    while let Some(record) = records.next() {
        let record = record?;
        if bytes.len().saturating_add(record.len()) > SMALL_WRITE_BYTES {
            return replace_with(
                directory,
                leaf,
                [Ok(bytes), Ok(record)].into_iter().chain(records),
                |_| Ok(()),
            );
        }
        bytes.extend_from_slice(&record);
    }
    recover(directory, leaf)?;
    directory
        .write_atomic_checked(leaf, &bytes, Publication::Upsert, verify_target)
        .map_err(|e| error(&directory.path().join(leaf), e))
}

fn replace_with(
    directory: &StoreDirectory,
    leaf: &Path,
    records: impl IntoIterator<Item = Result<Vec<u8>>>,
    mut checkpoint: impl FnMut(Phase) -> Result<()>,
) -> Result<()> {
    let path = directory.path().join(leaf);
    recover(directory, leaf)?;
    verify_target(directory, leaf).map_err(|e| error(&path, e))?;
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).map_err(|e| PipelineError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    let nonce: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let stage_name = format!(".{}.stage-{nonce}", leaf.display());
    let mut writer =
        StageWriter::create(directory, Path::new(&stage_name)).map_err(|e| error(&path, e))?;
    checkpoint(Phase::Created)?;
    let marker_name = format!(".{}.compaction-{nonce}", leaf.display());
    let marker = Path::new(&marker_name);
    let intent = serde_json::to_vec(&Intent {
        stage: stage_name,
        identity: writer.identity().clone(),
    })
    .map_err(|e| PipelineError::Schema(e.to_string()))?;
    directory
        .write_atomic(marker, &intent, Publication::CreateOnly)
        .map_err(|e| error(&path, e))?;
    checkpoint(Phase::Recorded)?;
    for record in records {
        writer.write_chunk(&record?).map_err(|e| error(&path, e))?;
        checkpoint(Phase::ChunkWritten)?;
    }
    writer.sync().map_err(|e| error(&path, e))?;
    checkpoint(Phase::Synced)?;
    verify_target(directory, leaf).map_err(|e| error(&path, e))?;
    writer
        .publish(directory, leaf, Publication::Upsert)
        .map_err(|e| error(&path, e))?;
    checkpoint(Phase::Published)?;
    recover(directory, leaf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn j1_copied_interrupted_compaction_never_adopts_or_deletes_unowned_stage() {
        let source = crate::test_support::PrivateTempDir::new();
        let directory = StoreDirectory::open(source.path()).unwrap();
        let leaf = Path::new("tasks.jsonl");
        directory
            .write_atomic(leaf, b"current\n", Publication::CreateOnly)
            .unwrap();
        assert!(
            replace_with(
                &directory,
                leaf,
                [Ok(b"stale candidate\n".to_vec())],
                |phase| {
                    if phase == Phase::Synced {
                        Err(PipelineError::Schema("crash".into()))
                    } else {
                        Ok(())
                    }
                }
            )
            .is_err()
        );
        let copy = crate::test_support::PrivateTempDir::new();
        let copied = StoreDirectory::open(copy.path()).unwrap();
        let mut residues = Vec::new();
        for entry in directory.entries(Path::new("")).unwrap() {
            let name = Path::new(&entry.name);
            if entry.name.to_string_lossy().contains(".stage-")
                || entry.name.to_string_lossy().contains(".compaction-")
                || name == leaf
            {
                let bytes = directory.read_optional(name, 4096).unwrap().unwrap();
                copied
                    .write_atomic(name, &bytes, Publication::CreateOnly)
                    .unwrap();
                if name != leaf {
                    residues.push((entry.name, bytes));
                }
            }
        }
        recover(&copied, leaf).unwrap();
        assert_eq!(
            copied.read_optional(leaf, 100).unwrap().unwrap(),
            b"current\n"
        );
        replace(&copied, leaf, [Ok(b"resumed\n".to_vec())]).unwrap();
        for (name, bytes) in residues {
            assert_eq!(
                copied
                    .read_optional(Path::new(&name), 4096)
                    .unwrap()
                    .unwrap(),
                bytes
            );
        }
        assert_eq!(
            copied.read_optional(leaf, 100).unwrap().unwrap(),
            b"resumed\n"
        );
    }

    #[test]
    fn j1_compaction_crash_at_every_phase_keeps_one_current_projection() {
        for phase in [
            Phase::Created,
            Phase::Recorded,
            Phase::ChunkWritten,
            Phase::Synced,
            Phase::Published,
        ] {
            let temp = crate::test_support::PrivateTempDir::new();
            let directory = StoreDirectory::open(temp.path()).unwrap();
            let leaf = Path::new("tasks.jsonl");
            directory
                .write_atomic(leaf, b"old\n", Publication::CreateOnly)
                .unwrap();
            let result = replace_with(
                &directory,
                leaf,
                [Ok(b"new\n".to_vec()), Ok(b"second\n".to_vec())],
                |at| {
                    if at == phase {
                        Err(PipelineError::Schema("simulated crash".into()))
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(result.is_err());
            // Reopen without using the candidate as authority, including after
            // publication but before retiring the saved stage identity.
            let reopened = StoreDirectory::open(temp.path()).unwrap();
            recover(&reopened, leaf).unwrap();
            let expected: &[u8] = if phase == Phase::Published {
                b"new\nsecond\n"
            } else {
                b"old\n"
            };
            assert_eq!(
                reopened.read_optional(leaf, 100).unwrap().unwrap(),
                expected
            );
            recover(&reopened, leaf).unwrap();
            replace(&reopened, leaf, [Ok(b"next\n".to_vec())]).unwrap();
            assert_eq!(
                reopened.read_optional(leaf, 100).unwrap().unwrap(),
                b"next\n"
            );
        }
    }
}
