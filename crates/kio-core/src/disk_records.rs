//! Private, identity-bound records and a bounded external merge sort.
//! Runs use the shared stage API. Only a fixed chunk and two merge heads live
//! in memory; the number of merge slots is bounded by the width of usize.
use crate::{
    KioError, Result,
    error::{Resource, ResourceLimit},
    store_dir::{StageIdentity, StageWriter, StoreDirectory},
};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::Arc,
};

pub const SORT_BUFFER_RECORDS: usize = 256;
pub const MERGE_SLOTS: usize = usize::BITS as usize;
const RECORD_BYTES: usize = 64 * 1024;

mod stages;
use stages::RunDirectory;

thread_local! {
    static PRESERVE_STAGES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
/// Keep abandoned record stages visible while an inspection builds its report.
pub struct StageInspection(PhantomData<std::rc::Rc<()>>);
impl StageInspection {
    pub fn begin() -> Self {
        PRESERVE_STAGES.with(|depth| depth.set(depth.get() + 1));
        Self(PhantomData)
    }
}
impl Drop for StageInspection {
    fn drop(&mut self) {
        PRESERVE_STAGES.with(|depth| depth.set(depth.get() - 1));
    }
}
fn stage_root() -> Result<PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(path) = crate::test_control::DebugTestControl::from_env()
        .core
        .record_stage_root
    {
        if !path.is_absolute() {
            return Err(KioError::invalid_usage(
                "record stage root must be absolute",
            ));
        }
        return Ok(path);
    }
    crate::xdg::xdg_dir("XDG_CACHE_HOME")
        .or_else(|| crate::xdg::home_dir().map(|path| path.join(".cache")))
        .map(|path| path.join("kio/record-stages"))
        .ok_or_else(|| KioError::invalid_usage("record stages require an absolute cache directory"))
}
pub(crate) fn inspect_stage_residues() -> Result<Vec<crate::cas::RawStageResidue>> {
    Ok(stages::inspect(&stage_root()?, false)?
        .into_iter()
        .map(|entry| crate::cas::RawStageResidue {
            path: entry.path,
            reason: entry.reason,
        })
        .collect())
}
struct RunFile {
    directory: StoreDirectory,
    identity: StageIdentity,
    _location: RunDirectory,
}
fn changed_records() -> KioError {
    KioError::schema("private record stage changed after sealing")
}
pub struct DiskRecords<T> {
    run: Arc<RunFile>,
    count: usize,
    size: u64,
    digest: [u8; 32],
    _value: PhantomData<T>,
}
impl<T> Clone for DiskRecords<T> {
    fn clone(&self) -> Self {
        Self {
            run: self.run.clone(),
            count: self.count,
            size: self.size,
            digest: self.digest,
            _value: PhantomData,
        }
    }
}
impl<T> std::fmt::Debug for DiskRecords<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskRecords")
            .field("count", &self.count)
            .finish()
    }
}
impl<T> DiskRecords<T> {
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn iter(&self) -> RecordIter<T> {
        let opened = (|| {
            self.run._location.identity.verify(&self.run.directory)?;
            let mut file = self
                .run
                .directory
                .open_regular_read(Path::new("records"), u64::MAX)?;
            self.run.identity.verify(&file)?;
            let metadata = file
                .metadata()
                .map_err(|error| crate::store_dir::ioerr(Path::new("records"), error))?;
            let before = crate::platform::cas_io::observe_regular_file_stability(
                &metadata,
                Path::new("records"),
            )?;
            if metadata.len() != self.size {
                return Err(changed_records());
            }
            let mut hasher = Sha256::new();
            let mut buffer = [0; RECORD_BYTES];
            let mut size = 0u64;
            loop {
                let count = file
                    .read(&mut buffer)
                    .map_err(|error| crate::store_dir::ioerr(Path::new("records"), error))?;
                if count == 0 {
                    break;
                }
                size = size.checked_add(count as u64).ok_or_else(changed_records)?;
                if size > self.size {
                    return Err(changed_records());
                }
                hasher.update(&buffer[..count]);
            }
            let digest: [u8; 32] = hasher.finalize().into();
            let after = file
                .metadata()
                .map_err(|error| crate::store_dir::ioerr(Path::new("records"), error))?;
            if size != self.size
                || digest != self.digest
                || crate::platform::cas_io::observe_regular_file_stability(
                    &after,
                    Path::new("records"),
                )? != before
            {
                return Err(changed_records());
            }
            self.run.identity.verify(&file)?;
            file.seek(SeekFrom::Start(0))
                .map_err(|error| crate::store_dir::ioerr(Path::new("records"), error))?;
            Ok(file)
        })();
        match opened {
            Ok(file) => RecordIter {
                _run: self.run.clone(),
                reader: Some(BufReader::with_capacity(64 * 1024, file)),
                remaining: self.count,
                error: None,
                buffer: Vec::new(),
                _value: PhantomData,
            },
            Err(error) => RecordIter {
                _run: self.run.clone(),
                reader: None,
                remaining: self.count,
                error: Some(error),
                buffer: Vec::new(),
                _value: PhantomData,
            },
        }
    }
}
impl<T: Serialize + DeserializeOwned> DiskRecords<T> {
    pub fn from_records(records: impl IntoIterator<Item = T>) -> Result<Self> {
        let mut out = RecordWriter::new()?;
        for item in records {
            out.push(&item)?;
        }
        out.finish()
    }
    pub fn to_vec(&self) -> Result<Vec<T>> {
        self.iter().collect()
    }
    pub fn find(&self, mut predicate: impl FnMut(&T) -> bool) -> Result<Option<T>> {
        for item in self.iter() {
            let item = item?;
            if predicate(&item) {
                return Ok(Some(item));
            }
        }
        Ok(None)
    }
    pub fn any(&self, predicate: impl FnMut(&T) -> bool) -> Result<bool> {
        self.find(predicate).map(|x| x.is_some())
    }
    pub fn first(&self) -> Result<Option<T>> {
        self.iter().next().transpose()
    }
    pub fn get(&self, index: usize) -> Result<Option<T>> {
        for (i, item) in self.iter().enumerate() {
            let item = item?;
            if i == index {
                return Ok(Some(item));
            }
        }
        Ok(None)
    }
    pub fn map_collect<U>(&self, mut map: impl FnMut(T) -> U) -> Result<Vec<U>> {
        self.iter().map(|item| item.map(&mut map)).collect()
    }
}
impl<T: DeserializeOwned> IntoIterator for &DiskRecords<T> {
    type Item = Result<T>;
    type IntoIter = RecordIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl<T: DeserializeOwned> IntoIterator for DiskRecords<T> {
    type Item = Result<T>;
    type IntoIter = RecordIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

pub struct RecordWriter<T> {
    run: Arc<RunFile>,
    writer: Option<StageWriter>,
    count: usize,
    size: u64,
    hasher: Sha256,
    _value: PhantomData<T>,
}
impl<T: Serialize> RecordWriter<T> {
    pub fn new() -> Result<Self> {
        let reclaim = PRESERVE_STAGES.with(|depth| depth.get() == 0);
        Self::new_at(&stage_root()?, reclaim)
    }
    fn new_at(root: &Path, reclaim: bool) -> Result<Self> {
        let (location, writer) = RunDirectory::create(root, reclaim)?;
        let directory = location.directory.clone();
        let identity = writer.identity().clone();
        Ok(Self {
            run: Arc::new(RunFile {
                directory,
                identity,
                _location: location,
            }),
            writer: Some(writer),
            count: 0,
            size: 0,
            hasher: Sha256::new(),
            _value: PhantomData,
        })
    }
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn snapshot(&self) -> DiskRecords<T> {
        DiskRecords {
            run: self.run.clone(),
            count: self.count,
            size: self.size,
            digest: self.hasher.clone().finalize().into(),
            _value: PhantomData,
        }
    }
    pub fn push(&mut self, value: &T) -> Result<()> {
        struct Buffer {
            bytes: Vec<u8>,
            exceeded: Option<usize>,
        }
        impl std::io::Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let observed = self.bytes.len().saturating_add(bytes.len());
                if observed > RECORD_BYTES {
                    self.exceeded = Some(observed);
                    return Err(std::io::Error::other("record buffer exhausted"));
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut buffer = Buffer {
            bytes: Vec::new(),
            exceeded: None,
        };
        let encoded = serde_json::to_writer(&mut buffer, value);
        if let Some(observed) = buffer.exceeded {
            return Err(record_limit(observed));
        }
        encoded.map_err(|e| KioError::schema(e.to_string()))?;
        let mut bytes = buffer.bytes;
        bytes.push(b'\n');
        self.writer
            .as_mut()
            .expect("record writer")
            .write_chunk(&bytes)?;
        self.hasher.update(&bytes);
        self.size = self
            .size
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| record_limit(usize::MAX))?;
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| record_limit(usize::MAX))?;
        Ok(())
    }
    pub fn finish(mut self) -> Result<DiskRecords<T>> {
        self.writer.as_ref().expect("record writer").sync()?;
        drop(self.writer.take());
        Ok(DiskRecords {
            run: self.run,
            count: self.count,
            size: self.size,
            digest: self.hasher.finalize().into(),
            _value: PhantomData,
        })
    }
}
fn record_limit(observed: usize) -> KioError {
    ResourceLimit {
        resource: Resource::Memory,
        stage: "tree_record".into(),
        limit: Some(RECORD_BYTES as u64),
        observed: Some(observed as u64),
        resource_tier: None,
    }
    .into_error("tree metadata record exceeds the streaming buffer")
}
pub struct RecordIter<T> {
    _run: Arc<RunFile>,
    reader: Option<BufReader<File>>,
    remaining: usize,
    error: Option<KioError>,
    buffer: Vec<u8>,
    _value: PhantomData<T>,
}
impl<T: DeserializeOwned> Iterator for RecordIter<T> {
    type Item = Result<T>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(error) = self.error.take() {
            self.remaining = 0;
            return Some(Err(error));
        }
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        self.buffer.clear();
        let result = (|| {
            let reader = self.reader.as_mut().expect("opened record reader");
            // Do not let a damaged private run grow a read_line allocation.
            loop {
                let bytes = reader
                    .fill_buf()
                    .map_err(|e| crate::store_dir::ioerr(Path::new("records"), e))?;
                if bytes.is_empty() {
                    return Err(KioError::schema("truncated private record stage"));
                }
                let end = bytes.iter().position(|b| *b == b'\n');
                let n = end.map_or(bytes.len(), |i| i + 1);
                if self.buffer.len().saturating_add(n) > RECORD_BYTES + 1 {
                    return Err(record_limit(self.buffer.len().saturating_add(n)));
                }
                self.buffer.extend_from_slice(&bytes[..n]);
                reader.consume(n);
                if end.is_some() {
                    break;
                }
            }
            serde_json::from_slice(&self.buffer).map_err(|e| KioError::schema(e.to_string()))
        })();
        if result.is_err() {
            self.remaining = 0;
        }
        Some(result)
    }
}
impl<T: Serialize + DeserializeOwned> Serialize for DiskRecords<T> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::{Error, SerializeSeq};
        let mut seq = serializer.serialize_seq(Some(self.len()))?;
        for item in self.iter() {
            seq.serialize_element(&item.map_err(S::Error::custom)?)?;
        }
        seq.end()
    }
}
impl<'de, T: Serialize + DeserializeOwned> serde::Deserialize<'de> for DiskRecords<T> {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor<T>(PhantomData<T>);
        impl<'de, T: Serialize + DeserializeOwned> serde::de::Visitor<'de> for Visitor<T> {
            type Value = DiskRecords<T>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a streamed record array")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut writer = RecordWriter::new().map_err(record_decode_error::<A::Error>)?;
                while let Some(item) = seq.next_element::<T>()? {
                    writer
                        .push(&item)
                        .map_err(record_decode_error::<A::Error>)?;
                }
                writer.finish().map_err(record_decode_error::<A::Error>)
            }
        }
        deserializer.deserialize_seq(Visitor(PhantomData))
    }
}
impl<T: DeserializeOwned + PartialEq> PartialEq for DiskRecords<T> {
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() {
            return false;
        }
        self.iter()
            .zip(other.iter())
            .all(|(a, b)| matches!((a,b), (Ok(a),Ok(b)) if a == b))
    }
}
impl<T: DeserializeOwned + Eq> Eq for DiskRecords<T> {}

pub struct RecordSorter<T> {
    chunk: Vec<T>,
    runs: [Option<DiskRecords<T>>; MERGE_SLOTS],
    compare: fn(&T, &T) -> Ordering,
    peak: usize,
}
impl<T: Serialize + DeserializeOwned> RecordSorter<T> {
    pub fn new(compare: fn(&T, &T) -> Ordering) -> Self {
        Self {
            chunk: Vec::with_capacity(SORT_BUFFER_RECORDS),
            runs: std::array::from_fn(|_| None),
            compare,
            peak: 0,
        }
    }
    pub fn push(&mut self, item: T) -> Result<()> {
        self.chunk.push(item);
        self.peak = self.peak.max(self.chunk.len());
        if self.chunk.len() == SORT_BUFFER_RECORDS {
            self.flush()?;
        }
        Ok(())
    }
    fn merge(&mut self, left: DiskRecords<T>, right: DiskRecords<T>) -> Result<DiskRecords<T>> {
        self.peak = self.peak.max(2);
        let mut a = left.iter();
        let mut b = right.iter();
        let mut first = a.next().transpose()?;
        let mut second = b.next().transpose()?;
        let mut out = RecordWriter::new()?;
        loop {
            let take_left = match (&first, &second) {
                (Some(x), Some(y)) => (self.compare)(x, y) != Ordering::Greater,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if take_left {
                out.push(first.as_ref().expect("merge head"))?;
                first = a.next().transpose()?;
            } else {
                out.push(second.as_ref().expect("merge head"))?;
                second = b.next().transpose()?;
            }
        }
        out.finish()
    }
    fn flush(&mut self) -> Result<()> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        self.chunk.sort_unstable_by(self.compare);
        let mut out = RecordWriter::new()?;
        for item in self.chunk.drain(..) {
            out.push(&item)?;
        }
        let mut carry = out.finish()?;
        for level in 0..MERGE_SLOTS {
            if let Some(previous) = self.runs[level].take() {
                carry = self.merge(previous, carry)?;
            } else {
                self.runs[level] = Some(carry);
                return Ok(());
            }
        }
        Err(record_limit(usize::MAX))
    }
    pub fn peak_buffered_records(&self) -> usize {
        self.peak
    }
    pub fn finish(mut self) -> Result<DiskRecords<T>> {
        self.flush()?;
        let mut result = None;
        for level in 0..MERGE_SLOTS {
            if let Some(run) = self.runs[level].take() {
                result = Some(match result {
                    Some(previous) => self.merge(previous, run)?,
                    None => run,
                });
            }
        }
        result.map_or_else(|| RecordWriter::new()?.finish(), Ok)
    }
}

thread_local! {
    static DECODE_ERRORS: std::cell::RefCell<Vec<Option<KioError>>> = const { std::cell::RefCell::new(Vec::new()) };
}
fn record_decode_error<E: serde::de::Error>(error: KioError) -> E {
    let converted = E::custom(&error);
    DECODE_ERRORS.with(|errors| {
        if let Some(slot) = errors.borrow_mut().last_mut() {
            *slot = Some(error);
        }
    });
    converted
}
/// Preserve typed private-stage failures across serde's schema error boundary.
/// The scope is local to this thread and nests with other JSON decoding scopes.
pub fn decode_json<T: DeserializeOwned>(reader: impl std::io::Read) -> Result<T> {
    struct Scope;
    impl Drop for Scope {
        fn drop(&mut self) {
            DECODE_ERRORS.with(|errors| {
                errors.borrow_mut().pop();
            });
        }
    }
    DECODE_ERRORS.with(|errors| errors.borrow_mut().push(None));
    let scope = Scope;
    let decoded = serde_json::from_reader(reader);
    let error = DECODE_ERRORS.with(|errors| errors.borrow_mut().last_mut().and_then(Option::take));
    drop(scope);
    if let Some(error) = error {
        return Err(error);
    }
    decoded.map_err(|error| KioError::schema(error.to_string()))
}

/// Bound one lexical token before serde can grow its string/number scratch.
/// JSON whitespace and entry count are unbounded; only the live token is capped.
pub fn decode_bounded_json<T: DeserializeOwned>(reader: impl std::io::Read) -> Result<T> {
    struct TokenReader<R> {
        source: R,
        string: bool,
        escape: bool,
        size: usize,
        error: Option<KioError>,
    }
    impl<R: std::io::Read> std::io::Read for TokenReader<R> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let size = self.source.read(bytes)?;
            for byte in &bytes[..size] {
                if self.string {
                    self.size += 1;
                    if self.escape {
                        self.escape = false;
                    } else if *byte == b'\\' {
                        self.escape = true;
                    } else if *byte == b'"' {
                        self.string = false;
                        self.size = 0;
                    }
                } else if *byte == b'"' {
                    self.string = true;
                    self.size = 1;
                } else if byte.is_ascii_whitespace() || b"{}[],:".contains(byte) {
                    self.size = 0;
                } else {
                    self.size += 1;
                }
                if self.size > RECORD_BYTES {
                    self.error = Some(record_limit(self.size));
                    return Err(std::io::Error::other(
                        "JSON token exceeds its streaming buffer",
                    ));
                }
            }
            Ok(size)
        }
    }
    let mut source = TokenReader {
        source: reader,
        string: false,
        escape: false,
        size: 0,
        error: None,
    };
    let parsed = decode_json(&mut source);
    if let Some(error) = source.error {
        return Err(error);
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sealed_records_reject_valid_same_length_edits_and_appends() {
        let records = DiskRecords::from_records([11u64, 22]).unwrap();
        let path = records.run.directory.path().join("records");
        std::fs::write(&path, b"33\n22\n").unwrap();
        assert!(
            records.first().is_err(),
            "valid JSON cannot replace sealed authority"
        );
        std::fs::write(&path, b"11\n22\n33\n").unwrap();
        assert!(
            records.first().is_err(),
            "trailing records cannot be ignored"
        );
    }

    #[test]
    fn snapshots_seal_the_exact_bytes_observed_at_creation() {
        let mut writer = RecordWriter::new().unwrap();
        writer.push(&11u64).unwrap();
        let snapshot = writer.snapshot();
        assert_eq!(snapshot.first().unwrap(), Some(11));
        writer.push(&22).unwrap();
        assert!(snapshot.first().is_err());
        assert_eq!(writer.finish().unwrap().to_vec().unwrap(), vec![11, 22]);
    }

    #[test]
    fn cleanup_removes_only_verified_stages_and_empty_owned_directories() {
        let records = DiskRecords::from_records([11u64]).unwrap();
        let path = records.run.directory.path().to_owned();
        drop(records);
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_preserves_replaced_record_file() {
        let fixture = tempfile::tempdir().unwrap();
        let mut writer = RecordWriter::new_at(&fixture.path().join("registry"), true).unwrap();
        writer.push(&11u64).unwrap();
        let records = writer.finish().unwrap();
        let directory = records.run.directory.path().to_owned();
        let path = directory.join("records");
        std::fs::rename(&path, fixture.path().join("original-records")).unwrap();
        let mut replacement =
            StageWriter::create(&records.run.directory, Path::new("records")).unwrap();
        replacement.write_chunk(b"foreign replacement").unwrap();
        replacement.sync().unwrap();
        drop(replacement);
        assert!(records.first().is_err());
        drop(records);
        assert_eq!(std::fs::read(&path).unwrap(), b"foreign replacement");
        // The fixture owns the replacement and the preserved lease/journal.
    }

    #[test]
    fn cleanup_preserves_replaced_directory_and_its_contents() {
        let fixture = tempfile::tempdir().unwrap();
        let mut writer = RecordWriter::new_at(&fixture.path().join("registry"), true).unwrap();
        writer.push(&11u64).unwrap();
        let records = writer.finish().unwrap();
        let path = records.run.directory.path().to_owned();
        std::fs::rename(&path, fixture.path().join("original-directory")).unwrap();
        std::fs::create_dir(&path).unwrap();
        let foreign = path.join("foreign");
        std::fs::write(&foreign, b"preserved").unwrap();
        drop(records);
        assert_eq!(std::fs::read(&foreign).unwrap(), b"preserved");
        std::fs::remove_file(foreign).unwrap();
        std::fs::remove_dir(path).unwrap();
    }

    #[test]
    fn external_sort_has_a_fixed_record_buffer_as_input_grows() {
        for count in [257, 4097, 32769] {
            let mut sorter = RecordSorter::new(|a: &u64, b: &u64| a.cmp(b));
            for value in (0..count).rev() {
                sorter.push(value).unwrap();
            }
            assert!(sorter.peak_buffered_records() <= SORT_BUFFER_RECORDS);
            let records = sorter.finish().unwrap();
            assert_eq!(records.len(), count as usize);
            for (index, record) in records.iter().enumerate() {
                assert_eq!(record.unwrap(), index as u64);
            }
        }
    }
    #[test]
    fn record_token_buffer_and_read_errors_never_grow_with_record_count() {
        let error = RecordWriter::new()
            .unwrap()
            .push(&"x".repeat(RECORD_BYTES + 1))
            .unwrap_err();
        assert_eq!(error.resource_limit().unwrap().resource, Resource::Memory);
        let records = DiskRecords::from_records(["a".to_owned(), "b".to_owned()]).unwrap();
        std::fs::write(
            records.run.directory.path().join("records"),
            b"not a record\n",
        )
        .unwrap();
        assert!(records.get(1).is_err());
        assert!(records.any(|_| true).is_err());
    }
}
