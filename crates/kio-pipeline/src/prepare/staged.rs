use super::{
    ExtractionBudget, PrepareStageBytesRequest, PrepareStageOutput, PreparedUnit, hash_bytes,
    office_preparation_profile_hash, prepare_units_emitting, resolve_prepare_converter,
};
#[cfg(test)]
use crate::IoResultExt;
use crate::extraction_stage::{
    SMALL_EXTRACTION_BYTES, SavedStage, child_directory, existing_workspace, read_record,
    stage_error, write_record,
};
use crate::{PipelineError, Result};
#[cfg(test)]
use kio_core::cas::MAX_CONTENT_OBJECT_BYTES;
use kio_core::cas::{ContentObjectKind, ObjectStore, is_hash};
use kio_core::store_dir::{Publication, StoreDirectory};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::BTreeSet;
#[cfg(test)]
use std::io::Read;
use std::path::{Path, PathBuf};

const COMPLETE: &str = "complete.json";
const LAYOUT: &str = "layout.json";
const RECORD_BYTES: u64 = 128 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitRecord {
    unit: PreparedUnit,
    stage: SavedStage,
    complete: bool,
}

/// A completed extraction whose payloads remain private until persisted.
#[derive(Debug)]
pub struct StagedPreparation {
    payloads: PreparedPayloads,
    raw_hash: String,
    pub output: PrepareStageOutput,
}

#[derive(Debug)]
enum PreparedPayloads {
    Small(Vec<(PreparedUnit, Vec<u8>)>),
    Staged(StoreDirectory),
}

pub fn prepare_staged(
    root: &StoreDirectory,
    request: PrepareStageBytesRequest<'_>,
    budget: ExtractionBudget,
) -> Result<StagedPreparation> {
    let converter = resolve_prepare_converter(request.media_type, root.control());
    let profile = converter.as_ref().map_or_else(
        || request.tool_profile_hash.to_owned(),
        |converter| {
            office_preparation_profile_hash(
                request.tool_profile_hash,
                &converter.profile_identity(),
            )
        },
    );
    prepare_staged_with(root, request, &profile, |completed, emit| {
        prepare_units_emitting(request, converter.as_ref(), budget, completed, emit)
    })
}

/// Drive a source by supplying its durable prefix before body production.
/// Producers may discover their layouts independently, call the sink for
/// missing units as each becomes available, and return only unit metadata.
pub fn prepare_staged_with(
    root: &StoreDirectory,
    request: PrepareStageBytesRequest<'_>,
    profile: &str,
    extract: impl FnOnce(&[PreparedUnit], &mut PreparedUnitSink<'_>) -> Result<PrepareStageOutput>,
) -> Result<StagedPreparation> {
    if hash_bytes(request.bytes) != request.raw_hash {
        return Err(PipelineError::contract(
            "KIO-E-PREPARE-IDENTITY-001",
            "prepare bytes do not match the declared raw hash",
        ));
    }
    let key =
        hash_bytes(format!("{}\0{}\0{}", request.raw_hash, request.media_type, profile).as_bytes());
    let path = Path::new("extraction-stages/prepared")
        .join(request.raw_hash.trim_start_matches("sha256:"))
        .join(key.trim_start_matches("sha256:"));
    let (output, payloads) = match existing_workspace(root, &path)? {
        Some(directory) => {
            let output = stage_units(&directory, extract)?;
            (output, PreparedPayloads::Staged(directory))
        }
        None => prepare_fresh(root, &path, extract)?,
    };
    if output.prepared_units.iter().any(|unit| {
        unit.preparation_profile_hash != profile || unit.mime.as_deref() != Some(request.media_type)
    }) {
        return Err(stage_corrupt(root, "prepared extraction profile mismatch"));
    }
    Ok(StagedPreparation {
        payloads,
        raw_hash: request.raw_hash.to_owned(),
        output,
    })
}

fn prepare_fresh(
    root: &StoreDirectory,
    path: &Path,
    extract: impl FnOnce(&[PreparedUnit], &mut PreparedUnitSink<'_>) -> Result<PrepareStageOutput>,
) -> Result<(PrepareStageOutput, PreparedPayloads)> {
    let count = Cell::new(None);
    let mut buffered = Vec::<(PreparedUnit, Vec<u8>)>::new();
    let mut size = 0usize;
    let mut workspace = None;
    let mut written = 0;
    let mut keys = BTreeSet::new();
    let output = extract(
        &[],
        &mut PreparedUnitSink {
            declare: &mut |units| {
                if count.replace(Some(units)).is_some() {
                    return Err(stage_corrupt(
                        root,
                        "producer declared extraction layout twice",
                    ));
                }
                Ok(())
            },
            write: &mut |unit, bytes| {
                if unit.order != written as u64
                    || count.get().is_none_or(|units| units <= written)
                    || !keys.insert(unit.unit_key.clone())
                    || !is_hash(&unit.prepared_hash)
                {
                    return Err(stage_corrupt(
                        root,
                        "prepared unit is outside its declared layout",
                    ));
                }
                written += 1;
                size = size.saturating_add(bytes.len());
                if workspace.is_none() && size <= SMALL_EXTRACTION_BYTES {
                    buffered.push((unit.clone(), bytes.to_vec()));
                    return Ok(());
                }
                if workspace.is_none() {
                    let directory = child_directory(root, path)?;
                    write_record(
                        &directory,
                        Path::new(LAYOUT),
                        &count.get().expect("declared layout"),
                        Publication::CreateOnly,
                    )?;
                    for (unit, bytes) in buffered.drain(..) {
                        write_prepared_unit(&directory, &unit, &bytes)?;
                    }
                    workspace = Some(directory);
                }
                write_prepared_unit(workspace.as_ref().expect("opened workspace"), unit, bytes)
            },
        },
    )?;
    if written != output.prepared_units.len() || count.get().is_some_and(|units| units != written) {
        return Err(stage_corrupt(
            root,
            "producer did not finish its declared layout",
        ));
    }
    let payloads = if let Some(directory) = workspace {
        complete_prepared_workspace(&directory, &output)?;
        PreparedPayloads::Staged(directory)
    } else {
        if !buffered
            .iter()
            .map(|(unit, _)| unit)
            .eq(&output.prepared_units)
        {
            return Err(stage_corrupt(
                root,
                "extractor returned a different unit layout",
            ));
        }
        PreparedPayloads::Small(buffered)
    };
    Ok((output, payloads))
}

/// Publish verified source bytes shared by units discovered by an OCR adapter.
pub fn publish_prepared_source(root: &StoreDirectory, hash: &str, bytes: &[u8]) -> Result<()> {
    if hash_bytes(bytes) != hash {
        return Err(stage_corrupt(
            root,
            "prepared source does not match its declared hash",
        ));
    }
    let store = ObjectStore::from_bound_kio_with_control(
        root.root_handle().as_ref(),
        root.control().clone(),
    )
    .map_err(|error| stage_error(root, error))?;
    if bytes.len() <= SMALL_EXTRACTION_BYTES {
        return store
            .write_content_object(ContentObjectKind::Prepared, bytes)
            .map(|_| ())
            .map_err(|error| stage_error(root, error));
    }
    let digest = hash.trim_start_matches("sha256:");
    let key = hash_bytes(b"ocr-discovered-source");
    let workspace = child_directory(
        root,
        &Path::new("extraction-stages/prepared")
            .join(digest)
            .join(key.trim_start_matches("sha256:")),
    )?;
    crate::extraction_stage::publish_payload(
        &workspace,
        Path::new("source.json"),
        |writer| {
            for chunk in bytes.chunks(kio_core::cas::CAS_STREAM_BUFFER_BYTES) {
                writer
                    .write_chunk(chunk)
                    .map_err(|error| stage_error(&workspace, error))?;
            }
            Ok(())
        },
        || Ok(()),
        |stage| {
            crate::extraction_stage::publish_content(
                &workspace,
                &store,
                ContentObjectKind::Prepared,
                hash,
                stage,
            )
        },
    )
}

type UnitWriter<'a> = dyn FnMut(&PreparedUnit, &[u8]) -> Result<()> + 'a;

/// A producer declares its expected unit count before writing any bodies.
/// It can then skip the durable prefix and supply the remaining units in order.
pub struct PreparedUnitSink<'a> {
    pub(super) declare: &'a mut dyn FnMut(usize) -> Result<()>,
    pub(super) write: &'a mut UnitWriter<'a>,
}

impl PreparedUnitSink<'_> {
    pub fn declare(&mut self, count: usize) -> Result<()> {
        (self.declare)(count)
    }

    pub fn write(&mut self, unit: &PreparedUnit, bytes: &[u8]) -> Result<()> {
        (self.write)(unit, bytes)
    }
}

fn stage_units(
    directory: &StoreDirectory,
    extract: impl FnOnce(&[PreparedUnit], &mut PreparedUnitSink<'_>) -> Result<PrepareStageOutput>,
) -> Result<PrepareStageOutput> {
    if let Some(count) = read_record::<usize>(directory, Path::new(COMPLETE), 64)? {
        return Ok(prepared_output(completed_units(directory, count)?));
    }

    // Hand the producer the durable prefix before it generates any unit bodies.
    let mut completed = Vec::new();
    for order in 0.. {
        let Some(record) =
            read_record::<UnitRecord>(directory, &unit_record_name(order), RECORD_BYTES)?
        else {
            break;
        };
        if !record.complete {
            break;
        }
        if record.unit.order != order || !is_hash(&record.unit.prepared_hash) {
            return Err(stage_corrupt(directory, "invalid prepared prefix"));
        }
        completed.push(record.unit);
    }
    let layout = Path::new(LAYOUT);
    let declared = read_record::<usize>(directory, layout, 64)?;
    if declared == Some(completed.len()) {
        let units = completed_units(directory, completed.len())?;
        write_record(
            directory,
            Path::new(COMPLETE),
            &completed.len(),
            Publication::CreateOnly,
        )?;
        return Ok(prepared_output(units));
    }
    let mut declare = |count| {
        if let Some(previous) = declared {
            if previous != count {
                return Err(stage_corrupt(
                    directory,
                    "producer changed the extraction layout",
                ));
            }
            Ok(())
        } else {
            write_record(directory, layout, &count, Publication::CreateOnly)
        }
    };
    let mut write = |unit: &PreparedUnit, bytes: &[u8]| write_prepared_unit(directory, unit, bytes);
    let output = extract(
        &completed,
        &mut PreparedUnitSink {
            declare: &mut declare,
            write: &mut write,
        },
    )?;
    if let Some(count) = read_record::<usize>(directory, layout, 64)?
        && count != output.prepared_units.len()
    {
        return Err(stage_corrupt(
            directory,
            "producer did not finish its declared layout",
        ));
    }
    if !output.prepared_units.starts_with(&completed) {
        return Err(stage_corrupt(
            directory,
            "extractor returned a different unit layout",
        ));
    }
    complete_prepared_workspace(directory, &output)?;
    Ok(output)
}

fn complete_prepared_workspace(
    directory: &StoreDirectory,
    output: &PrepareStageOutput,
) -> Result<()> {
    if completed_units(directory, output.prepared_units.len())? != output.prepared_units {
        return Err(stage_corrupt(
            directory,
            "extractor returned a different unit layout",
        ));
    }
    write_record(
        directory,
        Path::new(COMPLETE),
        &output.prepared_units.len(),
        Publication::CreateOnly,
    )
}

fn write_prepared_unit(
    directory: &StoreDirectory,
    unit: &PreparedUnit,
    bytes: &[u8],
) -> Result<()> {
    let leaf = unit_record_name(unit.order);
    if let Some(record) = read_record::<UnitRecord>(directory, &leaf, RECORD_BYTES)? {
        if record.unit != *unit {
            return Err(stage_corrupt(directory, "staged unit identity changed"));
        }
        if record.complete {
            return Ok(());
        }
        record.stage.discard(directory)?;
    }
    let (stage, mut writer) = SavedStage::create(directory)?;
    let mut record = UnitRecord {
        unit: unit.clone(),
        stage,
        complete: false,
    };
    write_record(directory, &leaf, &record, Publication::Upsert)?;
    directory
        .control()
        .checkpoint(kio_core::durability::DurabilityPoint::ExtractionStageWriting)
        .map_err(|error| stage_error(directory, error))?;
    for chunk in bytes.chunks(kio_core::cas::CAS_STREAM_BUFFER_BYTES) {
        writer
            .write_chunk(chunk)
            .map_err(|error| stage_error(directory, error))?;
    }
    writer
        .sync()
        .map_err(|error| stage_error(directory, error))?;
    record.complete = true;
    write_record(directory, &leaf, &record, Publication::Upsert)?;
    directory
        .control()
        .checkpoint(kio_core::durability::DurabilityPoint::ExtractionUnitReady)
        .map_err(|error| stage_error(directory, error))
}

fn completed_units(directory: &StoreDirectory, count: usize) -> Result<Vec<PreparedUnit>> {
    let mut units = Vec::new();
    let mut keys = BTreeSet::new();
    for order in 0..count {
        let unit = completed_unit(directory, order as u64)?.unit;
        if !keys.insert(unit.unit_key.clone()) {
            return Err(stage_corrupt(directory, "duplicate staged unit key"));
        }
        units.push(unit);
    }
    Ok(units)
}

fn prepared_output(units: Vec<PreparedUnit>) -> PrepareStageOutput {
    PrepareStageOutput {
        prepared_object_hashes: units
            .iter()
            .map(|unit| unit.prepared_hash.clone())
            .collect(),
        prepared_units: units,
        image_object_hashes: Vec::new(),
    }
}

impl StagedPreparation {
    pub fn persist_objects(&self, store: &ObjectStore) -> Result<()> {
        match &self.payloads {
            PreparedPayloads::Small(payloads) => {
                for (unit, bytes) in payloads {
                    if hash_bytes(bytes) != unit.prepared_hash {
                        return Err(PipelineError::contract(
                            "KIO-E-PREPARE-IDENTITY-001",
                            "prepared bytes differ from the declared hash",
                        ));
                    }
                    store
                        .write_content_object(ContentObjectKind::Prepared, bytes)
                        .map_err(|error| {
                            crate::extraction_stage::stage_error_path(
                                Path::new("objects/prepared"),
                                error,
                            )
                        })?;
                }
            }
            PreparedPayloads::Staged(directory) => {
                for unit in &self.output.prepared_units {
                    let record = completed_unit(directory, unit.order)?;
                    if &record.unit != unit {
                        return Err(stage_corrupt(directory, "staged unit metadata changed"));
                    }
                    record.stage.publish_content(
                        directory,
                        store,
                        ContentObjectKind::Prepared,
                        &unit.prepared_hash,
                    )?;
                }
            }
        }
        Ok(())
    }

    pub fn has_staged_payloads(&self) -> bool {
        matches!(self.payloads, PreparedPayloads::Staged(_))
    }

    pub fn raw_hash(&self) -> &str {
        &self.raw_hash
    }
}

fn unit_record_name(order: u64) -> PathBuf {
    PathBuf::from(format!("unit-{order}.json"))
}

fn completed_unit(directory: &StoreDirectory, order: u64) -> Result<UnitRecord> {
    read_record(directory, &unit_record_name(order), RECORD_BYTES)?
        .filter(|record: &UnitRecord| {
            record.complete && record.unit.order == order && is_hash(&record.unit.prepared_hash)
        })
        .ok_or_else(|| stage_corrupt(directory, "missing or invalid completed extraction unit"))
}

#[cfg(test)]
fn read_staged_unit(directory: &StoreDirectory, record: &UnitRecord) -> Result<Vec<u8>> {
    let file = record.stage.open(directory, MAX_CONTENT_OBJECT_BYTES)?;
    let size = file.metadata().pipeline_io(directory.path())?.len();
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size as usize).map_err(|_| {
        PipelineError::ResourceLimit(kio_core::error::ResourceLimit {
            resource: kio_core::error::Resource::Memory,
            stage: "prepared_unit".to_owned(),
            limit: None,
            observed: Some(size),
            resource_tier: None,
        })
    })?;
    file.take(MAX_CONTENT_OBJECT_BYTES + 1)
        .read_to_end(&mut bytes)
        .pipeline_io(directory.path())?;
    if bytes.len() as u64 > MAX_CONTENT_OBJECT_BYTES
        || hash_bytes(&bytes) != record.unit.prepared_hash
    {
        return Err(stage_corrupt(
            directory,
            "extraction stage does not match the declared prepared hash",
        ));
    }
    Ok(bytes)
}

fn stage_corrupt(directory: &StoreDirectory, message: &str) -> PipelineError {
    PipelineError::corrupt(directory.path().display().to_string(), message)
}

/// Remove identified payloads for a raw hash under the scope's purge lock.
pub fn discard_prepared_stages(root: &StoreDirectory, raw_hash: &str) -> Result<()> {
    crate::extraction_stage::visit_raw_workspaces(root, "prepared", raw_hash, discard_workspace)
}

fn discard_workspace(workspace: &StoreDirectory) -> Result<()> {
    crate::extraction_stage::discard_payload(workspace, Path::new("source.json"))?;
    for name in [COMPLETE, LAYOUT] {
        let leaf = Path::new(name);
        if workspace
            .contains_entry(leaf)
            .map_err(|error| stage_error(workspace, error))?
        {
            workspace
                .remove_file(leaf)
                .map_err(|error| stage_error(workspace, error))?;
        }
    }
    for entry in workspace
        .entries(Path::new(""))
        .map_err(|error| stage_error(workspace, error))?
    {
        let Some(order) = entry.name.to_str().and_then(|name| {
            name.strip_prefix("unit-")?
                .strip_suffix(".json")?
                .parse::<u64>()
                .ok()
        }) else {
            continue;
        };
        let leaf = unit_record_name(order);
        if leaf.as_os_str() != entry.name {
            continue;
        }
        if let Some(record) = read_record::<UnitRecord>(workspace, &leaf, RECORD_BYTES)? {
            record.stage.discard(workspace)?;
            workspace
                .remove_file(&leaf)
                .map_err(|error| stage_error(workspace, error))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prepare::{UnitType, fingerprint_for_bytes};
    use kio_core::error::{Resource, ResourceLimit};

    fn unit(order: u64, bytes: &[u8]) -> PreparedUnit {
        PreparedUnit {
            renderer: None,
            order,
            unit_key: format!("page:{}", order + 1),
            unit_type: UnitType::Page,
            prepared_hash: hash_bytes(bytes),
            preparation_profile_hash: hash_bytes(b"profile"),
            fingerprint: fingerprint_for_bytes(bytes, bytes),
            mime: Some("application/pdf".to_owned()),
            page_number: Some(order + 1),
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn p0_producer_cannot_mark_unwritten_units_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let directory = StoreDirectory::open(tmp.path()).unwrap();
        let bytes = b"declared but initially unwritten";
        let hash = hash_bytes(bytes);
        let unit = PreparedUnit {
            renderer: None,
            unit_key: "document".to_owned(),
            unit_type: crate::prepare::UnitType::File,
            order: 0,
            prepared_hash: hash.clone(),
            preparation_profile_hash: hash,
            fingerprint: crate::prepare::fingerprint_for_bytes(bytes, bytes),
            mime: Some("text/plain".into()),
            page_number: None,
        };
        let incomplete = stage_units(&directory, |_, sink| {
            sink.declare(1)?;
            Ok(prepared_output(vec![unit.clone()]))
        });
        assert!(incomplete.is_err());
        assert!(!directory.contains_entry(Path::new(COMPLETE)).unwrap());
        let complete = stage_units(&directory, |completed, sink| {
            assert!(completed.is_empty());
            sink.declare(1)?;
            sink.write(&unit, bytes)?;
            Ok(prepared_output(vec![unit]))
        })
        .unwrap();
        assert_eq!(complete.prepared_units.len(), 1);
    }

    #[test]
    fn p0_prepared_spills_only_after_exceeding_one_buffer() {
        for extra in [0, 1] {
            let temp = tempfile::tempdir().unwrap();
            let repo = kio_core::scope::Repository::init(temp.path()).unwrap();
            let root = StoreDirectory::open(repo.kio_dir()).unwrap();
            let path = Path::new("extraction-stages/prepared-test");
            let first = vec![b'x'; SMALL_EXTRACTION_BYTES / 2];
            let second = vec![b'y'; SMALL_EXTRACTION_BYTES / 2 + extra];
            let (output, payloads) = prepare_fresh(&root, path, |_, emit| {
                let units = vec![unit(0, &first), unit(1, &second)];
                emit.declare(2)?;
                for (entry, bytes) in units.iter().zip([&first, &second]) {
                    emit.write(entry, bytes)?;
                }
                Ok(prepared_output(units))
            })
            .unwrap();
            let result = StagedPreparation {
                payloads,
                raw_hash: hash_bytes(b"source"),
                output,
            };
            assert_eq!(result.has_staged_payloads(), extra == 1);
            assert_eq!(root.path().join(path).exists(), extra == 1);
            result.persist_objects(&repo.object_store()).unwrap();
            for (entry, bytes) in result.output.prepared_units.iter().zip([first, second]) {
                assert_eq!(
                    repo.object_store()
                        .read_content_object_bytes(
                            ContentObjectKind::Prepared,
                            &entry.prepared_hash,
                            SMALL_EXTRACTION_BYTES as u64,
                        )
                        .unwrap(),
                    bytes
                );
            }
        }
    }

    #[test]
    fn p0_prepared_cas_rejects_matching_physical_id_with_wrong_nonce() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let repo = kio_core::scope::Repository::init(temp.path()).unwrap();
        let root = StoreDirectory::open(repo.kio_dir()).unwrap();
        let bytes = vec![b'x'; SMALL_EXTRACTION_BYTES + 1];
        let raw = hash_bytes(&bytes);
        let profile = hash_bytes(b"profile");
        let staged = prepare_staged(
            &root,
            PrepareStageBytesRequest {
                raw_hash: &raw,
                media_type: "text/plain",
                input_path: "doc.txt",
                tool_profile_hash: &profile,
                bytes: &bytes,
            },
            ExtractionBudget::new(kio_core::error::ResourceTier::Standard),
        )
        .unwrap();
        let PreparedPayloads::Staged(directory) = &staged.payloads else {
            panic!("large prepared source must be staged");
        };
        let leaf = unit_record_name(0);
        let record = completed_unit(directory, 0).unwrap();
        let mut forged = serde_json::to_value(&record).unwrap();
        forged["stage"]["identity"]["nonce"][0] =
            serde_json::json!(forged["stage"]["identity"]["nonce"][0].as_u64().unwrap() ^ 1);
        write_record(directory, &leaf, &forged, Publication::Upsert).unwrap();
        assert!(staged.persist_objects(&repo.object_store()).is_err());
        assert_eq!(read_staged_unit(directory, &record).unwrap(), bytes);
    }

    #[test]
    fn p0_resumes_completed_unit_then_reuses_completed_extraction() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let directory = StoreDirectory::open(temp.path()).unwrap();
        let first = unit(0, b"first");
        let second = unit(1, b"second");
        let limit = ResourceLimit {
            resource: Resource::Storage,
            stage: "private_stage".to_owned(),
            limit: None,
            observed: None,
            resource_tier: None,
        };
        let failed = stage_units(&directory, |completed, emit| {
            assert!(completed.is_empty());
            emit.declare(2)?;
            emit.write(&first, b"first")?;
            // An interruption may leave a journal with identity but without a
            // completion marker. Recovery may only reuse the completed prefix.
            let (stage, mut writer) = SavedStage::create(&directory)?;
            write_record(
                &directory,
                &unit_record_name(1),
                &UnitRecord {
                    unit: second.clone(),
                    stage,
                    complete: false,
                },
                Publication::CreateOnly,
            )?;
            writer.write_chunk(b"sec").unwrap();
            Err(PipelineError::ResourceLimit(limit.clone()))
        });
        assert!(matches!(failed, Err(PipelineError::ResourceLimit(actual)) if actual == limit));
        assert!(!directory.contains_entry(Path::new(COMPLETE)).unwrap());
        assert!(completed_unit(&directory, 1).is_err());
        let first_before = directory
            .read_optional(&unit_record_name(0), RECORD_BYTES)
            .unwrap();
        drop(directory);

        let directory = StoreDirectory::open(temp.path()).unwrap();
        let output = stage_units(&directory, |completed, emit| {
            assert_eq!(completed, std::slice::from_ref(&first));
            emit.declare(2)?;
            emit.write(&second, b"second")?;
            Ok(PrepareStageOutput {
                prepared_object_hashes: vec![
                    first.prepared_hash.clone(),
                    second.prepared_hash.clone(),
                ],
                prepared_units: vec![first.clone(), second.clone()],
                image_object_hashes: Vec::new(),
            })
        })
        .unwrap();
        assert_eq!(
            directory
                .read_optional(&unit_record_name(0), RECORD_BYTES)
                .unwrap(),
            first_before
        );
        assert_eq!(
            read_staged_unit(&directory, &completed_unit(&directory, 1).unwrap()).unwrap(),
            b"second"
        );
        directory.remove_file(Path::new(COMPLETE)).unwrap();
        let replay = stage_units(&directory, |_, _| {
            panic!("completed extraction was repeated")
        })
        .unwrap();
        assert_eq!(replay, output);
    }
}
