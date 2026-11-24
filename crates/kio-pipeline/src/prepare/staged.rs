use super::{
    ExtractionBudget, PrepareStageBytesRequest, PrepareStageOutput, PreparedUnit,
    XlsxExtractionProgress, hash_bytes, office_preparation_profile_hash, prepare_units_emitting,
    resolve_prepare_converter, xlsx_preparation_profile_hash,
};
#[cfg(test)]
use crate::IoResultExt;
use crate::extraction_stage::{
    SMALL_EXTRACTION_BYTES, SavedStage, child_directory, existing_workspace, read_record,
    stage_error, write_record,
};
use crate::{PipelineError, Result};
use kio_adapter::office_convert::OfficeConverter;
use kio_adapter::xlsx_extract::SheetUsage;
#[cfg(test)]
use kio_core::cas::MAX_CONTENT_OBJECT_BYTES;
use kio_core::cas::{ContentObjectKind, ObjectStore, is_hash};
use kio_core::disk_records::RecordStageRoot;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sheet_usage: Option<SheetUsage>,
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
    Small(Vec<BufferedUnit>),
    Staged(StoreDirectory),
}

#[derive(Debug)]
struct BufferedUnit {
    unit: PreparedUnit,
    bytes: Vec<u8>,
    sheet_usage: Option<SheetUsage>,
}

pub fn prepare_staged(
    root: &StoreDirectory,
    request: PrepareStageBytesRequest<'_>,
    budget: ExtractionBudget,
) -> Result<StagedPreparation> {
    let converter = resolve_prepare_converter(request.media_type, root.control());
    prepare_staged_with_converter(root, request, budget, converter.as_ref())
}

/// Prepare with the renderer selected for this index invocation.
pub fn prepare_staged_with_converter(
    root: &StoreDirectory,
    request: PrepareStageBytesRequest<'_>,
    budget: ExtractionBudget,
    converter: Option<&OfficeConverter>,
) -> Result<StagedPreparation> {
    let profile = converter.map_or_else(
        || {
            if kio_adapter::xlsx_extract::is_xlsx_media(request.media_type) {
                xlsx_preparation_profile_hash(request.tool_profile_hash)
            } else {
                request.tool_profile_hash.to_owned()
            }
        },
        |converter| {
            office_preparation_profile_hash(
                request.tool_profile_hash,
                &converter.profile_identity(),
            )
        },
    );
    prepare_staged_with(root, request, &profile, |completed, emit| {
        let record_stages = kio_adapter::xlsx_extract::is_xlsx_media(request.media_type)
            .then(|| {
                ObjectStore::from_bound_kio_with_control(
                    root.root_handle().as_ref(),
                    root.control().clone(),
                )?
                .record_stages()
            })
            .transpose()
            .map_err(|error| match error.resource_limit() {
                Some(limit) => PipelineError::ResourceLimit(limit.clone()),
                None => PipelineError::RecordStage(error),
            })?;
        prepare_units_emitting(
            request,
            converter,
            budget,
            completed,
            &mut XlsxExtractionProgress::default(),
            &mut PreparedUnitSink {
                declare: emit.declare,
                write: emit.write,
                read_sheet_usage: emit.read_sheet_usage,
                record_stages: record_stages.as_ref(),
            },
        )
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
    let check_sheet_usage = kio_adapter::xlsx_extract::is_xlsx_media(request.media_type);
    let (output, payloads) = match existing_workspace(root, &path)? {
        Some(directory) => {
            let output = stage_units_checked(&directory, check_sheet_usage, extract)?;
            (output, PreparedPayloads::Staged(directory))
        }
        None => prepare_fresh(root, &path, check_sheet_usage, extract)?,
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
    check_sheet_usage: bool,
    extract: impl FnOnce(&[PreparedUnit], &mut PreparedUnitSink<'_>) -> Result<PrepareStageOutput>,
) -> Result<(PrepareStageOutput, PreparedPayloads)> {
    let count = Cell::new(None);
    let mut buffered = Vec::<BufferedUnit>::new();
    let mut size = 0usize;
    let mut workspace = None;
    let mut written = 0;
    let mut ready_through = 0;
    let mut keys = BTreeSet::new();
    let result = extract(
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
            write: &mut |unit, bytes, sheet_usage| {
                validate_sheet_usage(root, check_sheet_usage, bytes, sheet_usage)?;
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
                    buffered.push(BufferedUnit {
                        unit: unit.clone(),
                        bytes: bytes.to_vec(),
                        sheet_usage,
                    });
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
                    workspace = Some(directory);
                    let directory = workspace.as_ref().expect("opened workspace");
                    for buffered in buffered.drain(..) {
                        write_prepared_unit(
                            directory,
                            &buffered.unit,
                            &buffered.bytes,
                            buffered.sheet_usage,
                            &mut ready_through,
                        )?;
                    }
                }
                write_prepared_unit(
                    workspace.as_ref().expect("opened workspace"),
                    unit,
                    bytes,
                    sheet_usage,
                    &mut ready_through,
                )
            },
            record_stages: None,
            read_sheet_usage: &|_| {
                Err(stage_corrupt(
                    root,
                    "fresh extraction has no completed worksheets",
                ))
            },
        },
    );
    let output = result
        .map_err(|error| with_staged_progress(error, workspace.as_ref().map(|_| ready_through)))?;
    if written != output.prepared_units.len() || count.get().is_some_and(|units| units != written) {
        return Err(stage_corrupt(
            root,
            "producer did not finish its declared layout",
        ));
    }
    let payloads = if let Some(directory) = workspace {
        complete_prepared_workspace(&directory, &output, false)?;
        PreparedPayloads::Staged(directory)
    } else {
        if !buffered
            .iter()
            .map(|buffered| &buffered.unit)
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

type UnitWriter<'a> = dyn FnMut(&PreparedUnit, &[u8], Option<SheetUsage>) -> Result<()> + 'a;

/// A producer declares its expected unit count before writing any bodies.
/// It can then skip the durable prefix and supply the remaining units in order.
pub struct PreparedUnitSink<'a> {
    pub(super) declare: &'a mut dyn FnMut(usize) -> Result<()>,
    pub(super) write: &'a mut UnitWriter<'a>,
    pub(super) record_stages: Option<&'a RecordStageRoot>,
    pub(super) read_sheet_usage: &'a dyn Fn(u64) -> Result<SheetUsage>,
}

impl PreparedUnitSink<'_> {
    pub fn declare(&mut self, count: usize) -> Result<()> {
        (self.declare)(count)
    }

    pub fn write(&mut self, unit: &PreparedUnit, bytes: &[u8]) -> Result<()> {
        (self.write)(unit, bytes, None)
    }

    pub(super) fn write_sheet(
        &mut self,
        unit: &PreparedUnit,
        bytes: &[u8],
        usage: SheetUsage,
    ) -> Result<()> {
        (self.write)(unit, bytes, Some(usage))
    }
}

#[cfg(test)]
fn stage_units(
    directory: &StoreDirectory,
    extract: impl FnOnce(&[PreparedUnit], &mut PreparedUnitSink<'_>) -> Result<PrepareStageOutput>,
) -> Result<PrepareStageOutput> {
    stage_units_checked(directory, false, extract)
}

fn stage_units_checked(
    directory: &StoreDirectory,
    check_sheet_usage: bool,
    extract: impl FnOnce(&[PreparedUnit], &mut PreparedUnitSink<'_>) -> Result<PrepareStageOutput>,
) -> Result<PrepareStageOutput> {
    let already_complete = read_record::<usize>(directory, Path::new(COMPLETE), 64)?;
    if let Some(count) = already_complete
        && !check_sheet_usage
    {
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
    if already_complete.is_some_and(|count| count != completed.len() || declared != Some(count)) {
        return Err(stage_corrupt(
            directory,
            "prepared completion does not match its layout",
        ));
    }
    if declared == Some(completed.len()) && !check_sheet_usage {
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
    let mut ready_through = completed.len() as u64;
    let mut write = |unit: &PreparedUnit, bytes: &[u8], sheet_usage| {
        validate_sheet_usage(directory, check_sheet_usage, bytes, sheet_usage)?;
        write_prepared_unit(directory, unit, bytes, sheet_usage, &mut ready_through)
    };
    let result = extract(
        &completed,
        &mut PreparedUnitSink {
            declare: &mut declare,
            write: &mut write,
            record_stages: None,
            read_sheet_usage: &|order| {
                completed_unit(directory, order)?
                    .sheet_usage
                    .ok_or_else(|| {
                        stage_corrupt(directory, "missing worksheet resource accounting")
                    })
            },
        },
    );
    let output = result.map_err(|error| with_staged_progress(error, Some(ready_through)))?;
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
    complete_prepared_workspace(directory, &output, already_complete.is_some())?;
    Ok(output)
}

fn complete_prepared_workspace(
    directory: &StoreDirectory,
    output: &PrepareStageOutput,
    already_complete: bool,
) -> Result<()> {
    if completed_units(directory, output.prepared_units.len())? != output.prepared_units {
        return Err(stage_corrupt(
            directory,
            "extractor returned a different unit layout",
        ));
    }
    if !already_complete {
        write_record(
            directory,
            Path::new(COMPLETE),
            &output.prepared_units.len(),
            Publication::CreateOnly,
        )?;
    }
    Ok(())
}

fn with_staged_progress(mut error: PipelineError, staged_through: Option<u64>) -> PipelineError {
    if let PipelineError::ResourceLimit(limit) = &mut error
        && let Some(progress) = &mut limit.page_progress
    {
        progress.staged_through = staged_through;
    }
    error
}

fn validate_sheet_usage(
    directory: &StoreDirectory,
    check_sheet_usage: bool,
    bytes: &[u8],
    sheet_usage: Option<SheetUsage>,
) -> Result<()> {
    if check_sheet_usage != sheet_usage.is_some()
        || sheet_usage.is_some_and(|usage| usage.output_bytes != bytes.len() as u64)
    {
        return Err(stage_corrupt(
            directory,
            "invalid worksheet resource accounting",
        ));
    }
    Ok(())
}

fn write_prepared_unit(
    directory: &StoreDirectory,
    unit: &PreparedUnit,
    bytes: &[u8],
    sheet_usage: Option<SheetUsage>,
    ready_through: &mut u64,
) -> Result<()> {
    let leaf = unit_record_name(unit.order);
    if let Some(record) = read_record::<UnitRecord>(directory, &leaf, RECORD_BYTES)? {
        if record.unit != *unit || record.sheet_usage != sheet_usage {
            return Err(stage_corrupt(directory, "staged unit identity changed"));
        }
        if record.complete {
            *ready_through = (*ready_through).max(unit.order + 1);
            return Ok(());
        }
        record.stage.discard(directory)?;
    }
    let (stage, mut writer) = SavedStage::create(directory)?;
    let mut record = UnitRecord {
        unit: unit.clone(),
        stage,
        complete: false,
        sheet_usage,
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
    drop(writer);
    record.complete = true;
    write_record(directory, &leaf, &record, Publication::Upsert)?;
    *ready_through = unit.order + 1;
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
                for BufferedUnit { unit, bytes, .. } in payloads {
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
            page_progress: None,
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

    #[test]
    fn pdf_prepare_stages_only_above_the_combined_output_boundary() {
        use std::io::Write as _;

        for extra in [0, 1] {
            let temp = tempfile::tempdir().unwrap();
            let root = StoreDirectory::open(temp.path()).unwrap();
            let mut pdf = b"%PDF-1.4\n".to_vec();
            let lengths = [
                SMALL_EXTRACTION_BYTES / 2,
                SMALL_EXTRACTION_BYTES / 2 + extra,
            ];
            for (i, length) in lengths.into_iter().enumerate() {
                let page = i * 2 + 1;
                let contents = page + 1;
                write!(
                    pdf,
                    "{page} 0 obj << /Type /Page /Contents {contents} 0 R >> endobj\n\
                     {contents} 0 obj << /Length {} >> stream\nBT (",
                    length + 8,
                )
                .unwrap();
                pdf.resize(pdf.len() + length, b'x');
                pdf.extend_from_slice(b") Tj\nendstream\nendobj\n");
            }
            let result = prepare_staged(
                &root,
                PrepareStageBytesRequest {
                    raw_hash: &hash_bytes(&pdf),
                    media_type: "application/pdf",
                    input_path: "two-pages.pdf",
                    tool_profile_hash: &hash_bytes(b"profile"),
                    bytes: &pdf,
                },
                ExtractionBudget::new(kio_core::error::ResourceTier::Standard),
            )
            .unwrap();
            assert_eq!(result.output.prepared_units.len(), 2);
            for (index, (unit, length)) in
                result.output.prepared_units.iter().zip(lengths).enumerate()
            {
                assert_eq!(unit.page_number, Some(index as u64 + 1));
                assert_eq!(unit.prepared_hash, hash_bytes(&vec![b'x'; length]));
            }
            assert_eq!(result.has_staged_payloads(), extra == 1);
            assert_eq!(root.path().join("extraction-stages").exists(), extra == 1);
        }
    }

    #[test]
    fn xlsx_prepare_small_workbook_needs_no_identified_workspace() {
        let raw =
            include_bytes!("../../../kio-eval/acceptance-fixtures/v1/office-public/table.xlsx");
        let temp = tempfile::tempdir().unwrap();
        let repo = kio_core::scope::Repository::init(temp.path()).unwrap();
        let root = StoreDirectory::open(repo.kio_dir()).unwrap();
        let prepared = prepare_staged(
            &root,
            PrepareStageBytesRequest {
                raw_hash: &hash_bytes(raw),
                media_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                input_path: "table.xlsx",
                tool_profile_hash: &hash_bytes(b"profile"),
                bytes: raw,
            },
            ExtractionBudget::new(kio_core::error::ResourceTier::Standard),
        )
        .unwrap();
        assert_eq!(prepared.output.prepared_units.len(), 1);
        assert!(!prepared.has_staged_payloads());
        assert!(!root.path().join("extraction-stages").exists());
        prepared.persist_objects(&repo.object_store()).unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn review_completed_sheet_stage_still_runs_budget_admission() {
        use kio_adapter::xlsx_extract::{SheetProgress, XlsxWorkbook};
        use kio_core::error::ResourceTier;

        let raw =
            include_bytes!("../../../kio-eval/acceptance-fixtures/v1/office-public/table.xlsx");
        let mut book = XlsxWorkbook::open(raw, ResourceTier::Standard).unwrap();
        let (markdown, usage) = book.read_sheet(0, &mut SheetProgress::default()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let directory = StoreDirectory::open(tmp.path()).unwrap();

        stage_units_checked(&directory, true, |_, sink| {
            sink.declare(1)?;
            let mut prepared = unit(0, markdown.as_bytes());
            prepared.unit_type = UnitType::Sheet;
            sink.write_sheet(&prepared, markdown.as_bytes(), usage)?;
            Ok(prepared_output(vec![prepared]))
        })
        .unwrap();

        let limit = ResourceLimit {
            resource: Resource::Work,
            stage: "xlsx_extract".to_owned(),
            limit: Some(5),
            observed: Some(6),
            resource_tier: Some(ResourceTier::Standard),
            page_progress: None,
        };
        let outcome = stage_units_checked(&directory, true, |completed, sink| {
            assert_eq!(completed.len(), 1);
            assert_eq!((sink.read_sheet_usage)(0)?, usage);
            Err(PipelineError::ResourceLimit(limit.clone()))
        });
        assert!(matches!(outcome, Err(PipelineError::ResourceLimit(actual)) if actual == limit));
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
            let (output, payloads) = prepare_fresh(&root, path, false, |_, emit| {
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
            page_progress: None,
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
                    sheet_usage: None,
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
