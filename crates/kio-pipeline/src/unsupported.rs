//! Durable dispositions for archived inputs that cannot be enriched.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

use kio_core::cas::is_hash;
use serde::{Deserialize, Serialize};

use crate::platform::configure_no_follow;
use crate::store_path::{StorePathKind, resolve_existing_store_path};
use crate::task::is_scope_local_file_name;
use crate::{IoResultExt, PipelineError, Result};

pub const UNSUPPORTED_INPUTS_FILE: &str = "unsupported-inputs.jsonl";
pub const UNSUPPORTED_REASON_UNRECOGNIZED_BINARY: &str = "unrecognized_binary_without_local_text";
pub const UNSUPPORTED_REASON_RESOLVED: &str = "resolved";

/// Hard limits keep an adopted or corrupted store from selecting allocations.
pub const MAX_UNSUPPORTED_INPUT_STORE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_UNSUPPORTED_INPUT_RECORD_BYTES: u64 = 16 * 1024;
pub const MAX_UNSUPPORTED_INPUT_RECORDS: usize = 100_000;

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

/// Append-only owner of the current unsupported-input projection.
///
/// Multiple rows for one path are expected: the final row is authoritative, so
/// a re-index can bind the path to its newest raw object without rewriting prior
/// audit history.
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

        let mut file = self.open_for_append()?;
        let current_len = file.metadata().pipeline_io(&self.path)?.len();
        let projected_len = current_len.saturating_add(line.len() as u64);
        if projected_len > MAX_UNSUPPORTED_INPUT_STORE_BYTES {
            return Err(PipelineError::corrupt(
                self.path.display().to_string(),
                format!(
                    "{UNSUPPORTED_INPUTS_FILE} exceeds {MAX_UNSUPPORTED_INPUT_STORE_BYTES} byte limit"
                ),
            ));
        }

        // One framed write on O_APPEND prevents byte-wise interleaving. The
        // surrounding repository store lock remains the primary writer lock.
        file.write_all(&line).pipeline_io(&self.path)?;
        file.sync_data().pipeline_io(&self.path)
    }

    /// Read the current projection with the last valid row for each path winning.
    /// Returned rows are deterministic (lexicographic path order).
    pub fn latest_by_path(&self) -> Result<Vec<UnsupportedInputDisposition>> {
        let file = match self.open_for_read()? {
            Some(file) => file,
            None => return Ok(Vec::new()),
        };
        let file_len = file.metadata().pipeline_io(&self.path)?.len();
        if file_len > MAX_UNSUPPORTED_INPUT_STORE_BYTES {
            return Err(self.corrupt(format!(
                "{UNSUPPORTED_INPUTS_FILE} exceeds {MAX_UNSUPPORTED_INPUT_STORE_BYTES} byte limit: {file_len}"
            )));
        }

        let mut reader = std::io::BufReader::new(file);
        let mut line = Vec::new();
        let mut total_bytes = 0_u64;
        let mut record_count = 0_usize;
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
            total_bytes = total_bytes.saturating_add(read as u64);
            if total_bytes > MAX_UNSUPPORTED_INPUT_STORE_BYTES {
                return Err(self.corrupt(format!(
                    "{UNSUPPORTED_INPUTS_FILE} exceeds {MAX_UNSUPPORTED_INPUT_STORE_BYTES} byte limit"
                )));
            }
            if !line.ends_with(b"\n") {
                return Err(self.corrupt("unterminated unsupported-input record"));
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }

            record_count = record_count.saturating_add(1);
            if record_count > MAX_UNSUPPORTED_INPUT_RECORDS {
                return Err(self.corrupt(format!(
                    "{UNSUPPORTED_INPUTS_FILE} exceeds {MAX_UNSUPPORTED_INPUT_RECORDS} record limit"
                )));
            }
            let disposition: UnsupportedInputDisposition =
                serde_json::from_slice(&line).map_err(|error| self.corrupt(error.to_string()))?;
            validate_disposition(&disposition).map_err(|message| self.corrupt(message))?;
            by_path.insert(disposition.path.clone(), disposition);
        }

        Ok(by_path.into_values().collect())
    }

    fn open_for_append(&self) -> Result<fs::File> {
        let root = self.store_root_snapshot()?;
        let directory = kio_core::store_dir::StoreDirectory::open(&self.kio_dir)
            .map_err(|error| self.corrupt(error.to_string()))?;
        kio_core::private_fs::verify_private_directory_handle(&directory)
            .map_err(|error| self.corrupt(error.to_string()))?;
        directory
            .append_private(Path::new(UNSUPPORTED_INPUTS_FILE), &[])
            .map_err(|error| self.corrupt(error.to_string()))?;
        let resolved = resolve_existing_store_path(
            &self.kio_dir,
            Path::new(UNSUPPORTED_INPUTS_FILE),
            StorePathKind::RegularFile,
        )?;
        let open_path = resolved.as_deref().unwrap_or(&self.path);
        let mut options = OpenOptions::new();
        options.append(true);
        configure_no_follow(&mut options);
        let file = options.open(open_path).pipeline_io(open_path)?;
        self.validate_opened_file(&file, open_path, &root)?;
        kio_core::private_fs::verify_private_regular_file_handle(&file)
            .map_err(|error| self.corrupt(error.to_string()))?;
        Ok(file)
    }

    fn open_for_read(&self) -> Result<Option<fs::File>> {
        let root = self.store_root_snapshot()?;
        let Some(resolved) = resolve_existing_store_path(
            &self.kio_dir,
            Path::new(UNSUPPORTED_INPUTS_FILE),
            StorePathKind::RegularFile,
        )?
        else {
            return Ok(None);
        };
        let mut options = OpenOptions::new();
        options.read(true);
        configure_no_follow(&mut options);
        let file = options.open(&resolved).pipeline_io(&resolved)?;
        self.validate_opened_file(&file, &resolved, &root)?;
        kio_core::private_fs::verify_private_regular_file_handle(&file)
            .map_err(|error| self.corrupt(error.to_string()))?;
        Ok(Some(file))
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
    fn r23_cand_014_reader_rejects_oversized_store_before_reading() {
        let directory = crate::test_support::PrivateTempDir::new();
        let store = UnsupportedInputStore::new(directory.path());
        let file = crate::test_support::create_private_file(store.path());
        file.set_len(MAX_UNSUPPORTED_INPUT_STORE_BYTES + 1).unwrap();

        let error = store.latest_by_path().unwrap_err().to_string();
        assert!(error.contains("byte limit"), "unexpected error: {error}");
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
