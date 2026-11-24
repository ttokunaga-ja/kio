//! Durable dispositions for archived inputs that cannot be enriched.

use kio_core::dag::{TreeEntry, TreeObject};
use kio_core::disk_records::{
    DiskRecords, RecordSorter, RecordStageRoot, RecordWriter, SortedRecordCursor,
    merge_sorted_records,
};
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::io::Write;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};

use kio_core::cas::is_hash;
use serde::{Deserialize, Serialize};

use crate::task::is_scope_local_file_name;
use crate::{IoResultExt, PipelineError, Result};

pub const UNSUPPORTED_INPUTS_FILE: &str = "unsupported-inputs.jsonl";
pub const UNSUPPORTED_REASON_UNRECOGNIZED_BINARY: &str = "unrecognized_binary_without_local_text";
pub const UNSUPPORTED_REASON_OFFICE_NO_TEXT: &str = "office_conversion_without_local_text";
pub const OFFICE_CONVERSION_UNAVAILABLE_REASON: &str = "office_conversion_unavailable";
pub const UNSUPPORTED_REASON_SOURCE_CODE: &str = "source_code_is_not_supported";

pub fn unsupported_reason_for_path(path: &Path) -> Option<&'static str> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "rs" | "py" | "js" | "ts" | "go" | "java" | "c" | "h" | "cpp"
            )
        })
        .then_some(UNSUPPORTED_REASON_SOURCE_CODE)
}

pub const UNSUPPORTED_REASON_RESOLVED: &str = "resolved";

/// A compaction trigger, never a lifetime or projection size limit.
const UNSUPPORTED_COMPACTION_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_UNSUPPORTED_INPUT_RECORD_BYTES: u64 = 16 * 1024;

const MAX_PATH_BYTES: usize = 4_096;
const MAX_MEDIA_TYPE_BYTES: usize = 256;
const MAX_REASON_BYTES: usize = 1_024;
const INVALID_DISPOSITION_CODE: &str = "KIO-E-PIPELINE-UNSUPPORTED-INPUT-001";

/// An archived input that has no searchable enrichment representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnsupportedInputDisposition {
    pub path: String,
    pub raw_hash: String,
    pub media_type: String,
    pub size_bytes: u64,
    pub reason: String,
}

impl UnsupportedInputDisposition {
    pub fn is_unsupported(&self) -> bool {
        matches!(
            self.reason.as_str(),
            UNSUPPORTED_REASON_UNRECOGNIZED_BINARY
                | UNSUPPORTED_REASON_SOURCE_CODE
                | UNSUPPORTED_REASON_OFFICE_NO_TEXT
        )
    }
}

/// Durable owner of the current unsupported-input projection.
///
/// Writers retain the latest record and the record selected by published HEAD
/// until a subsequent HEAD makes the previous record unnecessary.
#[derive(Debug, Clone)]
pub struct UnsupportedInputStore {
    pub(crate) kio_dir: PathBuf,
    path: PathBuf,
    published_tree: Option<TreeObject>,
    stages: Option<RecordStageRoot>,
}

impl UnsupportedInputStore {
    #[must_use]
    pub fn new(kio_dir: impl AsRef<Path>) -> Self {
        let kio_dir = kio_dir.as_ref().to_path_buf();
        Self {
            path: kio_dir.join(UNSUPPORTED_INPUTS_FILE),
            kio_dir,
            published_tree: None,
            stages: None,
        }
    }

    /// Select the tree whose published reasons must survive a newer observation.
    #[must_use]
    pub fn with_published_tree(mut self, tree: Option<&TreeObject>) -> Self {
        self.published_tree = tree.cloned();
        self
    }

    #[must_use]
    pub fn with_record_stages(mut self, stages: RecordStageRoot) -> Self {
        self.stages = Some(stages);
        self
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Persist one complete JSONL frame and synchronize it before reporting
    /// success, so a successful index cannot expose only an in-memory counter.
    pub fn record(&self, disposition: &UnsupportedInputDisposition) -> Result<()> {
        validate_disposition(disposition)
            .map_err(|message| PipelineError::contract(INVALID_DISPOSITION_CODE, message))?;

        let mut line = serde_json::to_vec(disposition)
            .map_err(|error| PipelineError::Schema(error.to_string()))?;
        line.push(b'\n');
        if line.len() as u64 > MAX_UNSUPPORTED_INPUT_RECORD_BYTES {
            return Err(PipelineError::contract(
                INVALID_DISPOSITION_CODE,
                format!(
                    "unsupported-input record exceeds {MAX_UNSUPPORTED_INPUT_RECORD_BYTES} byte limit"
                ),
            ));
        }

        let directory = self.private_root()?;
        let leaf = Path::new(UNSUPPORTED_INPUTS_FILE);
        crate::journal::recover(&directory, leaf)?;
        let current_len = if directory
            .contains_entry(leaf)
            .map_err(|e| crate::journal::error(&self.path, e))?
        {
            let file = directory
                .open_regular_read(leaf, u64::MAX)
                .map_err(|e| crate::journal::error(&self.path, e))?;
            kio_core::private_fs::verify_private_regular_file_handle(&file)
                .map_err(|e| crate::journal::error(&self.path, e))?;
            file.metadata().pipeline_io(&self.path)?.len()
        } else {
            0
        };
        let projected_len = current_len.saturating_add(line.len() as u64);
        if projected_len > UNSUPPORTED_COMPACTION_BYTES {
            let projected = self.project(
                &directory,
                ProjectionKind::Retained,
                [Ok(disposition.clone())],
            )?;
            return self.replace_records(&directory, projected.rows);
        }

        directory
            .append_private(leaf, &line)
            .map_err(|e| crate::journal::error(&self.path, e))
    }

    /// Durably stage a scan's new reasons in path order while retaining those visible at HEAD.
    pub fn record_observations(
        &self,
        observations: impl IntoIterator<Item = kio_core::Result<UnsupportedInputDisposition>>,
    ) -> Result<()> {
        let directory = self.private_root()?;
        let mut observations = observations.into_iter().peekable();
        if observations.peek().is_none() {
            self.read_bound(&directory, |_| Ok(()))?;
            return Ok(());
        }
        let projected = self.project(&directory, ProjectionKind::Retained, observations)?;
        if projected.changed {
            self.replace_records(&directory, projected.rows)?;
        }
        Ok(())
    }

    /// Read the latest observation for each path, for writer deduplication.
    pub fn latest_by_path(&self) -> Result<DiskRecords<UnsupportedInputDisposition>> {
        Ok(self
            .project(&self.private_root()?, ProjectionKind::Latest, [])?
            .rows)
    }

    /// Read the latest HEAD dispositions and show unsupported reasons only for raw-only entries.
    pub fn published_by_path(&self) -> Result<DiskRecords<UnsupportedInputDisposition>> {
        Ok(self
            .project(&self.private_root()?, ProjectionKind::Published, [])?
            .rows)
    }

    /// Include both current HEAD reasons and newer observations when rewriting.
    pub fn retained_rows(&self) -> Result<DiskRecords<UnsupportedInputDisposition>> {
        Ok(self
            .project(&self.private_root()?, ProjectionKind::Retained, [])?
            .rows)
    }

    fn stage_error(&self, error: kio_core::KioError) -> PipelineError {
        crate::journal::error(&self.path, error)
    }

    fn project(
        &self,
        directory: &kio_core::store_dir::StoreDirectory,
        kind: ProjectionKind,
        observations: impl IntoIterator<Item = kio_core::Result<UnsupportedInputDisposition>>,
    ) -> Result<DispositionProjection> {
        let stages = match &self.stages {
            Some(stages) => stages.clone(),
            None => RecordStageRoot::temporary().map_err(|e| self.stage_error(e))?,
        };
        let mut previous = String::new();
        let mut ordered = true;
        let mut sequence = 0;
        let mut spool = RecordWriter::new(&stages).map_err(|e| self.stage_error(e))?;
        let old_count = self.read_bound(directory, |row| {
            ordered &= previous <= row.path;
            previous.clone_from(&row.path);
            spool
                .push(&(sequence, row))
                .map_err(|e| self.stage_error(e))?;
            sequence += 1;
            Ok(())
        })?;
        let mut journal = spool.finish().map_err(|e| self.stage_error(e))?;
        // A compacted journal is already ordered unless it has older HEAD
        // reasons at the front. Use the ordered frames directly in that case.
        if !ordered {
            let mut sorter = RecordSorter::new(&stages, compare_observations);
            for row in journal {
                sorter
                    .push(row.map_err(|e| self.stage_error(e))?)
                    .map_err(|e| self.stage_error(e))?;
            }
            journal = sorter.finish().map_err(|e| self.stage_error(e))?;
        }
        let mut additions = RecordWriter::new(&stages).map_err(|e| self.stage_error(e))?;
        let mut previous = String::new();
        for row in observations {
            let row = row.map_err(|e| self.stage_error(e))?;
            if row.path < previous {
                return Err(PipelineError::Schema(
                    "unsupported observations are not ordered by path".to_owned(),
                ));
            }
            previous.clone_from(&row.path);
            additions
                .push(&(sequence, row))
                .map_err(|e| self.stage_error(e))?;
            sequence += 1;
        }
        let additions = additions.finish().map_err(|e| self.stage_error(e))?;
        let mut input =
            merge_sorted_records(journal.iter(), additions.iter(), compare_observations).peekable();
        let mut head = SortedRecordCursor::new(
            self.published_tree
                .iter()
                .flat_map(|tree| tree.entries.iter()),
            |entry: &TreeEntry| &entry.path,
        );
        let mut rows = RecordWriter::new(&stages).map_err(|e| self.stage_error(e))?;
        let mut retained_latest = match kind {
            ProjectionKind::Retained => {
                Some(RecordWriter::new(&stages).map_err(|e| self.stage_error(e))?)
            }
            _ => None,
        };
        let mut changed = false;
        while let Some(row) = input.next() {
            let (position, mut latest) = row.map_err(|e| self.stage_error(e))?;
            let entry = if matches!(kind, ProjectionKind::Latest) {
                None
            } else {
                head.at(&latest.path).map_err(|e| self.stage_error(e))?
            };
            let mut published = entry
                .filter(|entry| entry.raw_hash == latest.raw_hash)
                .map(|_| latest.clone());
            changed |= position >= old_count;
            while input
                .peek()
                .is_some_and(|row| row.as_ref().is_ok_and(|(_, row)| row.path == latest.path))
            {
                let (position, row) = input
                    .next()
                    .expect("peeked row")
                    .map_err(|e| self.stage_error(e))?;
                changed |= position >= old_count && row != latest;
                if entry.is_some_and(|entry| entry.raw_hash == row.raw_hash) {
                    published = Some(row.clone());
                }
                latest = row;
            }
            match kind {
                ProjectionKind::Latest => rows.push(&latest),
                ProjectionKind::Published => {
                    if let Some(row) = published
                        && (!(row.is_unsupported()
                            || row.reason == OFFICE_CONVERSION_UNAVAILABLE_REASON)
                            || entry.is_some_and(|entry| entry.normalize.is_none()))
                    {
                        rows.push(&row)
                    } else {
                        Ok(())
                    }
                }
                ProjectionKind::Retained => {
                    if let Some(row) = published.filter(|row| row.raw_hash != latest.raw_hash) {
                        rows.push(&row).map_err(|e| self.stage_error(e))?;
                    }
                    retained_latest
                        .as_mut()
                        .expect("retained projection")
                        .push(&latest)
                }
            }
            .map_err(|e| self.stage_error(e))?;
        }
        if let Some(latest) = retained_latest {
            for row in latest.finish().map_err(|e| self.stage_error(e))? {
                rows.push(&row.map_err(|e| self.stage_error(e))?)
                    .map_err(|e| self.stage_error(e))?;
            }
        }
        let rows = rows.finish().map_err(|e| self.stage_error(e))?;
        Ok(DispositionProjection { rows, changed })
    }

    fn read_bound(
        &self,
        directory: &kio_core::store_dir::StoreDirectory,
        mut push: impl FnMut(UnsupportedInputDisposition) -> Result<()>,
    ) -> Result<usize> {
        let mut count = 0;
        let leaf = Path::new(UNSUPPORTED_INPUTS_FILE);
        if !directory
            .contains_entry(leaf)
            .map_err(|e| crate::journal::error(&self.path, e))?
        {
            return Ok(0);
        }
        let file = directory
            .open_regular_read(leaf, u64::MAX)
            .map_err(|e| crate::journal::error(&self.path, e))?;
        kio_core::private_fs::verify_private_regular_file_handle(&file)
            .map_err(|e| crate::journal::error(&self.path, e))?;
        let mut reader = std::io::BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader
                .by_ref()
                .take(MAX_UNSUPPORTED_INPUT_RECORD_BYTES.saturating_add(1))
                .read_until(b'\n', &mut line)
                .pipeline_io(&self.path)?;
            if read == 0 {
                break;
            }
            if read as u64 > MAX_UNSUPPORTED_INPUT_RECORD_BYTES {
                return Err(self.corrupt(format!(
                    "unsupported-input record exceeds {MAX_UNSUPPORTED_INPUT_RECORD_BYTES} byte limit"
                )));
            }
            if !line.ends_with(b"\n") {
                return Err(self.corrupt("unterminated unsupported-input record"));
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }

            let disposition: UnsupportedInputDisposition =
                serde_json::from_slice(&line).map_err(|error| self.corrupt(error.to_string()))?;
            validate_disposition(&disposition).map_err(|message| self.corrupt(message))?;
            push(disposition)?;
            count += 1;
        }

        Ok(count)
    }

    /// Replace a validated current projection (also used by purge).
    pub fn replace_all(&self, dispositions: &[UnsupportedInputDisposition]) -> Result<()> {
        let directory = self.private_root()?;
        self.replace_frames(
            &directory,
            dispositions.iter().map(|row| self.encode_record(row)),
        )
    }

    fn replace_records(
        &self,
        directory: &kio_core::store_dir::StoreDirectory,
        records: DiskRecords<UnsupportedInputDisposition>,
    ) -> Result<()> {
        self.replace_frames(
            directory,
            records
                .iter()
                .map(|row| self.encode_record(&row.map_err(|e| self.stage_error(e))?)),
        )
    }

    fn replace_frames(
        &self,
        directory: &kio_core::store_dir::StoreDirectory,
        records: impl IntoIterator<Item = Result<Vec<u8>>>,
    ) -> Result<()> {
        crate::journal::replace_streamed(directory, Path::new(UNSUPPORTED_INPUTS_FILE), records)
    }

    fn encode_record(&self, row: &UnsupportedInputDisposition) -> Result<Vec<u8>> {
        validate_disposition(row)
            .map_err(|message| PipelineError::contract(INVALID_DISPOSITION_CODE, message))?;
        let mut line = serde_json::to_vec(row).map_err(|e| PipelineError::Schema(e.to_string()))?;
        line.push(b'\n');
        if line.len() as u64 > MAX_UNSUPPORTED_INPUT_RECORD_BYTES {
            return Err(self.corrupt("unsupported-input record exceeds byte limit"));
        }
        Ok(line)
    }

    fn private_root(&self) -> Result<kio_core::store_dir::StoreDirectory> {
        let directory = kio_core::store_dir::StoreDirectory::open(&self.kio_dir)
            .map_err(|e| crate::journal::error(&self.path, e))?;
        kio_core::private_fs::verify_private_directory_handle(&directory)
            .map_err(|e| crate::journal::error(&self.path, e))?;
        Ok(directory)
    }

    pub(crate) fn corrupt(&self, message: impl Into<String>) -> PipelineError {
        PipelineError::corrupt(self.path.display().to_string(), message)
    }
}

fn compare_observations(
    left: &(usize, UnsupportedInputDisposition),
    right: &(usize, UnsupportedInputDisposition),
) -> std::cmp::Ordering {
    left.1.path.cmp(&right.1.path).then(left.0.cmp(&right.0))
}

enum ProjectionKind {
    Latest,
    Published,
    Retained,
}

struct DispositionProjection {
    rows: DiskRecords<UnsupportedInputDisposition>,
    changed: bool,
}

fn validate_disposition(
    disposition: &UnsupportedInputDisposition,
) -> std::result::Result<(), String> {
    if !is_scope_local_file_name(&disposition.path) {
        return Err(format!(
            "unsupported input path is not a scope-local file name: {}",
            disposition.path
        ));
    }
    if disposition.path.len() > MAX_PATH_BYTES {
        return Err(format!(
            "unsupported input path exceeds {MAX_PATH_BYTES} byte limit"
        ));
    }
    if !is_hash(&disposition.raw_hash) {
        return Err("unsupported input raw_hash must be a full lowercase SHA-256 hash".to_owned());
    }
    if disposition.media_type.trim().is_empty()
        || disposition.media_type.len() > MAX_MEDIA_TYPE_BYTES
    {
        return Err(format!(
            "unsupported input media_type must contain 1..={MAX_MEDIA_TYPE_BYTES} bytes"
        ));
    }
    if disposition.reason.trim().is_empty() || disposition.reason.len() > MAX_REASON_BYTES {
        return Err(format!(
            "unsupported input reason must contain 1..={MAX_REASON_BYTES} bytes"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disposition(path: &str, digit: char) -> UnsupportedInputDisposition {
        UnsupportedInputDisposition {
            path: path.to_owned(),
            raw_hash: format!("sha256:{}", digit.to_string().repeat(64)),
            media_type: "application/octet-stream".to_owned(),
            size_bytes: 2_002,
            reason: UNSUPPORTED_REASON_UNRECOGNIZED_BINARY.to_owned(),
        }
    }

    #[test]
    fn r23_cand_014_missing_store_has_no_unsupported_inputs() {
        let directory = crate::test_support::PrivateTempDir::new();
        let store = UnsupportedInputStore::new(directory.path());

        assert!(store.latest_by_path().unwrap().to_vec().unwrap().is_empty());
    }

    #[test]
    fn r23_cand_014_record_roundtrips_all_fields_and_latest_path_wins() {
        let directory = crate::test_support::PrivateTempDir::new();
        let store = UnsupportedInputStore::new(directory.path());
        store.record(&disposition("photo.bmp", 'a')).unwrap();
        store.record(&disposition("archive.zip", 'b')).unwrap();
        let mut latest_photo = disposition("photo.bmp", 'c');
        latest_photo.media_type = "image/bmp".to_owned();
        latest_photo.size_bytes = 4_004;
        latest_photo.reason = "unsupported_image_codec".to_owned();
        store.record(&latest_photo).unwrap();

        let latest = store.latest_by_path().unwrap().to_vec().unwrap();

        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].path, "archive.zip");
        assert_eq!(latest[1], latest_photo);
        assert_eq!(latest[1].raw_hash, format!("sha256:{}", "c".repeat(64)));
    }

    #[test]
    fn r23_cand_014_real_single_link_store_allows_append_and_read() {
        let directory = crate::test_support::PrivateTempDir::new();
        let store = UnsupportedInputStore::new(directory.path());
        crate::test_support::create_private_file(store.path());

        let expected = disposition("manual.pdf", 'd');
        store.record(&expected).unwrap();

        assert_eq!(
            store.latest_by_path().unwrap().to_vec().unwrap(),
            vec![expected]
        );
    }

    #[cfg(unix)]
    #[test]
    fn r23_cand_014_symlink_store_never_alters_external_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let external = outside.path().join("external.jsonl");
        let original = b"external state must remain unchanged\n";
        fs::write(&external, original).unwrap();
        let store = UnsupportedInputStore::new(directory.path());
        symlink(&external, store.path()).unwrap();

        assert!(store.record(&disposition("photo.bmp", 'a')).is_err());
        assert!(store.latest_by_path().is_err());
        assert_eq!(fs::read(&external).unwrap(), original);
    }

    #[test]
    fn r23_cand_014_hardlinked_store_never_alters_external_target() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let external = outside.path().join("external.jsonl");
        let original = b"external hardlink state must remain unchanged\n";
        fs::write(&external, original).unwrap();
        let store = UnsupportedInputStore::new(directory.path());
        fs::hard_link(&external, store.path()).unwrap();

        assert!(store.record(&disposition("photo.bmp", 'a')).is_err());
        assert!(store.latest_by_path().is_err());
        assert_eq!(fs::read(&external).unwrap(), original);
    }

    #[test]
    fn r23_cand_014_non_regular_store_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let store = UnsupportedInputStore::new(directory.path());
        fs::create_dir(store.path()).unwrap();

        assert!(store.record(&disposition("photo.bmp", 'a')).is_err());
        assert!(store.latest_by_path().is_err());
    }

    #[test]
    fn r23_cand_014_record_rejects_invalid_path_hash_and_reason() {
        let directory = tempfile::tempdir().unwrap();
        let store = UnsupportedInputStore::new(directory.path());

        for invalid in [
            UnsupportedInputDisposition {
                path: "../photo.bmp".to_owned(),
                ..disposition("photo.bmp", 'a')
            },
            UnsupportedInputDisposition {
                raw_hash: "sha256:short".to_owned(),
                ..disposition("photo.bmp", 'a')
            },
            UnsupportedInputDisposition {
                reason: String::new(),
                ..disposition("photo.bmp", 'a')
            },
        ] {
            assert!(matches!(
                store.record(&invalid),
                Err(PipelineError::Contract { .. })
            ));
        }
        assert!(!store.path().exists());
    }

    #[test]
    fn r23_cand_014_reader_rejects_semantically_poisoned_record() {
        let directory = tempfile::tempdir().unwrap();
        let store = UnsupportedInputStore::new(directory.path());
        let mut value = serde_json::to_value(disposition("photo.bmp", 'a')).unwrap();
        value["path"] = serde_json::Value::String("../../photo.bmp".to_owned());
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        fs::write(store.path(), bytes).unwrap();

        assert!(matches!(
            store.latest_by_path(),
            Err(PipelineError::Corrupt { .. })
        ));
    }

    #[test]
    fn r23_cand_014_reader_rejects_oversized_record_before_json_parse() {
        let directory = crate::test_support::PrivateTempDir::new();
        let store = UnsupportedInputStore::new(directory.path());
        crate::test_support::write_private_file(
            store.path(),
            &vec![b'x'; MAX_UNSUPPORTED_INPUT_RECORD_BYTES as usize + 1],
        );

        let error = store.latest_by_path().unwrap_err().to_string();
        assert!(
            error.contains("record exceeds"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn j1_current_unsupported_projection_can_exceed_old_byte_and_record_limits() {
        let directory = crate::test_support::PrivateTempDir::new();
        let store = UnsupportedInputStore::new(directory.path());
        let rows: Vec<_> = (0..100_001)
            .map(|i| disposition(&format!("input_{i:06}.bmp"), 'a'))
            .collect();
        store.replace_all(&rows).unwrap();
        assert!(fs::metadata(store.path()).unwrap().len() > 16 * 1024 * 1024);
        assert_eq!(store.latest_by_path().unwrap().to_vec().unwrap(), rows);
        let mut current = rows[0].clone();
        current.reason = UNSUPPORTED_REASON_RESOLVED.into();
        store.record(&current).unwrap();
        let updated = store.latest_by_path().unwrap().to_vec().unwrap();
        assert_eq!(updated.len(), rows.len());
        assert_eq!(updated[0], current);
        assert_eq!(&updated[1..], &rows[1..]);
    }

    #[test]
    fn j1_compaction_does_not_hide_a_corrupt_unsupported_record() {
        let directory = crate::test_support::PrivateTempDir::new();
        let store = UnsupportedInputStore::new(directory.path());
        let mut file = crate::test_support::create_private_file(store.path());
        file.write_all(b"{broken}\n").unwrap();
        file.set_len(16 * 1024 * 1024 + 1).unwrap();
        assert!(matches!(
            store.record(&disposition("photo.bmp", 'a')),
            Err(PipelineError::Corrupt { .. })
        ));
        assert_eq!(file.metadata().unwrap().len(), 16 * 1024 * 1024 + 1);
    }

    #[test]
    fn j1_unsupported_journal_outlives_cumulative_limits_and_keeps_latest_path() {
        let directory = crate::test_support::PrivateTempDir::new();
        let store = UnsupportedInputStore::new(directory.path());
        let old = disposition("photo.bmp", 'a');
        let mut line = serde_json::to_vec(&old).unwrap();
        line.push(b'\n');
        let mut writer =
            std::io::BufWriter::new(crate::test_support::create_private_file(store.path()));
        for _ in 0..100_001 {
            writer.write_all(&line).unwrap();
        }
        drop(writer);
        assert!(fs::metadata(store.path()).unwrap().len() > 16 * 1024 * 1024);
        assert_eq!(store.latest_by_path().unwrap().to_vec().unwrap(), vec![old]);
        let mut current = disposition("photo.bmp", 'b');
        current.reason = UNSUPPORTED_REASON_RESOLVED.into();
        store.record(&current).unwrap();
        assert_eq!(
            store.latest_by_path().unwrap().to_vec().unwrap(),
            vec![current]
        );
        assert!(fs::metadata(store.path()).unwrap().len() < 16 * 1024 * 1024);
    }

    #[test]
    fn r23_cand_014_reader_rejects_unterminated_append_frame() {
        let directory = crate::test_support::PrivateTempDir::new();
        let store = UnsupportedInputStore::new(directory.path());
        let bytes = serde_json::to_vec(&disposition("photo.bmp", 'a')).unwrap();
        crate::test_support::write_private_file(store.path(), &bytes);

        let error = store.latest_by_path().unwrap_err().to_string();
        assert!(error.contains("unterminated"), "unexpected error: {error}");
    }
}
