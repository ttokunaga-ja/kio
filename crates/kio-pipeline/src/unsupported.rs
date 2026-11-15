//! Durable dispositions for archived inputs that cannot be enriched.

use std::collections::BTreeMap;
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

/// Durable owner of the current unsupported-input projection.
///
/// Multiple rows for one path are expected: the final row is authoritative, so
/// a re-index can bind the path to its newest raw object. Compaction retains
/// exactly that final row, including resolved reasons.
#[derive(Debug, Clone)]
pub struct UnsupportedInputStore {
    pub(crate) kio_dir: PathBuf,
    path: PathBuf,
}

impl UnsupportedInputStore {
    #[must_use]
    pub fn new(kio_dir: impl AsRef<Path>) -> Self {
        let kio_dir = kio_dir.as_ref().to_path_buf();
        Self {
            path: kio_dir.join(UNSUPPORTED_INPUTS_FILE),
            kio_dir,
        }
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
            let mut current = self.latest_bound(&directory)?;
            current.retain(|row| row.path != disposition.path);
            current.push(disposition.clone());
            return self.replace_bound(&directory, &current);
        }

        directory
            .append_private(leaf, &line)
            .map_err(|e| crate::journal::error(&self.path, e))
    }

    /// Read the current projection with the last valid row for each path winning.
    /// Returned rows are deterministic (lexicographic path order).
    pub fn latest_by_path(&self) -> Result<Vec<UnsupportedInputDisposition>> {
        self.latest_bound(&self.private_root()?)
    }

    fn latest_bound(
        &self,
        directory: &kio_core::store_dir::StoreDirectory,
    ) -> Result<Vec<UnsupportedInputDisposition>> {
        let leaf = Path::new(UNSUPPORTED_INPUTS_FILE);
        if !directory
            .contains_entry(leaf)
            .map_err(|e| crate::journal::error(&self.path, e))?
        {
            return Ok(Vec::new());
        }
        let file = directory
            .open_regular_read(leaf, u64::MAX)
            .map_err(|e| crate::journal::error(&self.path, e))?;
        kio_core::private_fs::verify_private_regular_file_handle(&file)
            .map_err(|e| crate::journal::error(&self.path, e))?;
        let mut reader = std::io::BufReader::new(file);
        let mut line = Vec::new();
        let mut by_path = BTreeMap::new();
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
            by_path.insert(disposition.path.clone(), disposition);
        }

        Ok(by_path.into_values().collect())
    }

    /// Replace a validated current projection (also used by purge).
    pub fn replace_all(&self, dispositions: &[UnsupportedInputDisposition]) -> Result<()> {
        self.replace_bound(&self.private_root()?, dispositions)
    }

    fn replace_bound(
        &self,
        directory: &kio_core::store_dir::StoreDirectory,
        dispositions: &[UnsupportedInputDisposition],
    ) -> Result<()> {
        for disposition in dispositions {
            validate_disposition(disposition)
                .map_err(|message| PipelineError::contract(INVALID_DISPOSITION_CODE, message))?;
        }
        crate::journal::replace(
            directory,
            Path::new(UNSUPPORTED_INPUTS_FILE),
            dispositions.iter().map(|row| {
                let mut line =
                    serde_json::to_vec(row).map_err(|e| PipelineError::Schema(e.to_string()))?;
                line.push(b'\n');
                if line.len() as u64 > MAX_UNSUPPORTED_INPUT_RECORD_BYTES {
                    return Err(self.corrupt("unsupported-input record exceeds byte limit"));
                }
                Ok(line)
            }),
        )
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

        assert!(store.latest_by_path().unwrap().is_empty());
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

        let latest = store.latest_by_path().unwrap();

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

        assert_eq!(store.latest_by_path().unwrap(), vec![expected]);
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
        assert_eq!(store.latest_by_path().unwrap(), rows);
        let mut current = rows[0].clone();
        current.reason = UNSUPPORTED_REASON_RESOLVED.into();
        store.record(&current).unwrap();
        let updated = store.latest_by_path().unwrap();
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
        assert_eq!(store.latest_by_path().unwrap(), vec![old]);
        let mut current = disposition("photo.bmp", 'b');
        current.reason = UNSUPPORTED_REASON_RESOLVED.into();
        store.record(&current).unwrap();
        assert_eq!(store.latest_by_path().unwrap(), vec![current]);
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
