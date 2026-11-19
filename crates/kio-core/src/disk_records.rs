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
    path::Path,
    sync::Arc,
};

pub const SORT_BUFFER_RECORDS: usize = 256;
pub const MERGE_SLOTS: usize = usize::BITS as usize;
const RECORD_BYTES: usize = 64 * 1024;
const FRAME_TAG_BYTES: usize = 32;

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
/// Retained authority for a store's registry or an isolated scratch workspace.
#[derive(Clone, Debug)]
pub struct RecordStageRoot {
    directory: StoreDirectory,
    scratch: Option<Arc<ScratchDirectory>>,
}

#[derive(Debug)]
struct ScratchDirectory {
    directory: tempfile::TempDir,
    preserve: std::sync::atomic::AtomicBool,
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        if *self.preserve.get_mut() {
            self.directory.disable_cleanup(true);
        }
    }
}

impl RecordStageRoot {
    pub(crate) fn in_store(directory: StoreDirectory) -> Self {
        Self {
            directory,
            scratch: None,
        }
    }

    /// Allocate a private workspace for callers without a writable store.
    /// DiskRecords retain it until the last record reader has finished.
    pub fn temporary() -> Result<Self> {
        let temp = tempfile::Builder::new()
            .prefix("kio-records-")
            .tempdir()
            .map_err(|error| crate::store_dir::ioerr(Path::new("record scratch"), error))?;
        let directory = StoreDirectory::open(temp.path())?;
        crate::store_dir::restrict_new_private_directory(&directory.root_handle())?;
        Ok(Self {
            directory,
            scratch: Some(Arc::new(ScratchDirectory {
                directory: temp,
                preserve: false.into(),
            })),
        })
    }

    fn registry(&self, create: bool) -> Result<Option<StoreDirectory>> {
        crate::private_fs::verify_private_directory_handle(&self.directory)?;
        let parent = if self.scratch.is_none() {
            let Some(internal) = stages::container(&self.directory, "internal", create)? else {
                return Ok(None);
            };
            internal
        } else {
            self.directory.clone()
        };
        stages::container(&parent, "record-stages", create)
    }

    pub(crate) fn inspect(&self) -> Result<Vec<crate::cas::RawStageResidue>> {
        let Some(registry) = self.registry(false)? else {
            return Ok(Vec::new());
        };
        Ok(stages::inspect(&registry, false)?
            .into_iter()
            .map(|entry| crate::cas::RawStageResidue {
                path: entry.path,
                reason: entry.reason,
            })
            .collect())
    }

    fn preserve_scratch(&self) {
        if let Some(scratch) = &self.scratch {
            scratch
                .preserve
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

struct RunFile {
    directory: StoreDirectory,
    identity: StageIdentity,
    frames: FrameAuthenticator,
    _location: RunDirectory,
}
// HMAC-SHA256 states for a per-run key that never goes to disk.
struct FrameAuthenticator {
    inner: Sha256,
    outer: Sha256,
}

impl FrameAuthenticator {
    fn new(key: &[u8; 32]) -> Self {
        let mut inner_pad = [0x36; 64];
        let mut outer_pad = [0x5c; 64];
        for (index, byte) in key.iter().enumerate() {
            inner_pad[index] ^= byte;
            outer_pad[index] ^= byte;
        }
        Self {
            inner: Sha256::new().chain_update(inner_pad),
            outer: Sha256::new().chain_update(outer_pad),
        }
    }

    fn tag(&self, ordinal: usize, frame: &[u8]) -> [u8; FRAME_TAG_BYTES] {
        // Binding the ordinal prevents replay at a different position.
        let inner = self
            .inner
            .clone()
            .chain_update(ordinal.to_le_bytes())
            .chain_update(frame)
            .finalize();
        self.outer.clone().chain_update(inner).finalize().into()
    }

    fn verify(&self, ordinal: usize, frame: &[u8], tag: [u8; FRAME_TAG_BYTES]) -> Result<()> {
        let difference = self
            .tag(ordinal, frame)
            .into_iter()
            .zip(tag)
            .fold(0, |difference, (expected, actual)| {
                difference | (expected ^ actual)
            });
        if difference != 0 {
            return Err(changed_records());
        }
        Ok(())
    }
}

fn changed_records() -> KioError {
    KioError::schema("private record stage changed after sealing")
}
pub struct DiskRecords<T> {
    run: Option<Arc<RunFile>>,
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
    pub fn empty() -> Self {
        Self {
            run: None,
            count: 0,
            size: 0,
            digest: Sha256::digest([]).into(),
            _value: PhantomData,
        }
    }

    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn iter(&self) -> RecordIter<T> {
        let opened = self
            .run
            .as_ref()
            .map(|run| {
                run._location.identity.verify(&run.directory)?;
                let mut file = run
                    .directory
                    .open_regular_read(Path::new("records"), u64::MAX)?;
                run.identity.verify(&file)?;
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
                run.identity.verify(&file)?;
                file.seek(SeekFrom::Start(0))
                    .map_err(|error| crate::store_dir::ioerr(Path::new("records"), error))?;
                Ok(file)
            })
            .transpose();
        match opened {
            Ok(file) => RecordIter {
                run: self.run.clone(),
                reader: file.map(|file| BufReader::with_capacity(64 * 1024, file)),
                remaining: self.count,
                ordinal: 0,
                error: None,
                buffer: Vec::new(),
                _value: PhantomData,
            },
            Err(error) => RecordIter {
                run: self.run.clone(),
                reader: None,
                remaining: self.count,
                ordinal: 0,
                error: Some(error),
                buffer: Vec::new(),
                _value: PhantomData,
            },
        }
    }
}
impl<T: Serialize + DeserializeOwned> DiskRecords<T> {
    pub fn from_records(
        root: &RecordStageRoot,
        records: impl IntoIterator<Item = T>,
    ) -> Result<Self> {
        let mut out = RecordWriter::new(root)?;
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
    root: RecordStageRoot,
    run: Option<Arc<RunFile>>,
    writer: Option<StageWriter>,
    count: usize,
    size: u64,
    hasher: Sha256,
    _value: PhantomData<T>,
}
impl<T: Serialize> RecordWriter<T> {
    pub fn new(root: &RecordStageRoot) -> Result<Self> {
        let mut out = Self::new_lazy(root);
        out.open()?;
        Ok(out)
    }

    /// Allocate a stage on the first push. Empty output has no on-disk run.
    pub fn new_lazy(root: &RecordStageRoot) -> Self {
        Self {
            root: root.clone(),
            run: None,
            writer: None,
            count: 0,
            size: 0,
            hasher: Sha256::new(),
            _value: PhantomData,
        }
    }

    fn open(&mut self) -> Result<()> {
        if self.writer.is_some() {
            return Ok(());
        }
        let mut key = [0; 32];
        getrandom::fill(&mut key)
            .map_err(|error| KioError::io(error.to_string(), "record stage key"))?;
        let reclaim = PRESERVE_STAGES.with(|depth| depth.get() == 0);
        let created = RunDirectory::create(&self.root, reclaim);
        if created.is_err() {
            self.root.preserve_scratch();
        }
        let (location, writer) = created?;
        self.run = Some(Arc::new(RunFile {
            directory: location.directory.clone(),
            identity: writer.identity().clone(),
            frames: FrameAuthenticator::new(&key),
            _location: location,
        }));
        self.writer = Some(writer);
        Ok(())
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
                let observed = (self.bytes.len() - FRAME_TAG_BYTES).saturating_add(bytes.len());
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
            bytes: vec![0; FRAME_TAG_BYTES],
            exceeded: None,
        };
        let encoded = serde_json::to_writer(&mut buffer, value);
        if let Some(observed) = buffer.exceeded {
            return Err(record_limit(observed));
        }
        encoded.map_err(|e| KioError::schema(e.to_string()))?;
        let mut bytes = buffer.bytes;
        bytes.push(b'\n');
        self.open()?;
        let frames = &self.run.as_ref().expect("open run").frames;
        let tag = frames.tag(self.count, &bytes[FRAME_TAG_BYTES..]);
        bytes[..FRAME_TAG_BYTES].copy_from_slice(&tag);
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
        if let Some(writer) = self.writer.take() {
            writer.sync()?;
        }
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
    run: Option<Arc<RunFile>>,
    reader: Option<BufReader<File>>,
    remaining: usize,
    ordinal: usize,
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
            let mut tag = [0; FRAME_TAG_BYTES];
            reader
                .read_exact(&mut tag)
                .map_err(|e| crate::store_dir::ioerr(Path::new("records"), e))?;
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
            let frames = &self.run.as_ref().expect("opened record run").frames;
            frames.verify(self.ordinal, &self.buffer, tag)?;
            self.ordinal += 1;
            serde_json::from_slice(&self.buffer).map_err(|e| KioError::schema(e.to_string()))
        })();
        if result.is_err() {
            self.remaining = 0;
        }
        Some(result)
    }
}
/// Merge two ordered streams while keeping at most one record from each.
pub fn merge_sorted_records<T>(
    left: impl Iterator<Item = Result<T>>,
    right: impl Iterator<Item = Result<T>>,
    compare: impl Fn(&T, &T) -> Ordering,
) -> impl Iterator<Item = Result<T>> {
    let mut left = left.peekable();
    let mut right = right.peekable();
    std::iter::from_fn(move || match (left.peek(), right.peek()) {
        (Some(Err(_)), _) | (Some(Ok(_)), None) => left.next(),
        (_, Some(Err(_))) | (None, Some(Ok(_))) => right.next(),
        (Some(Ok(a)), Some(Ok(b))) if compare(a, b) != Ordering::Greater => left.next(),
        (Some(Ok(_)), Some(Ok(_))) => right.next(),
        (None, None) => None,
    })
}

/// Look up increasing keys in a path-sorted stream while retaining one record.
pub struct SortedRecordCursor<T, I, F> {
    records: I,
    current: Option<T>,
    key: F,
}

impl<T, I, F> SortedRecordCursor<T, I, F>
where
    I: Iterator<Item = Result<T>>,
    F: Fn(&T) -> &str,
{
    pub fn new(records: I, key: F) -> Self {
        Self {
            records,
            current: None,
            key,
        }
    }

    pub fn at(&mut self, path: &str) -> Result<Option<&T>> {
        while self
            .current
            .as_ref()
            .is_none_or(|item| (self.key)(item) < path)
        {
            self.current = self.records.next().transpose()?;
            if self.current.is_none() {
                break;
            }
        }
        Ok(self
            .current
            .as_ref()
            .filter(|item| (self.key)(item) == path))
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
                let root = DECODE_CONTEXT
                    .with(|context| context.borrow().last().map(|context| context.root.clone()))
                    .ok_or_else(|| {
                        serde::de::Error::custom("disk record decoding requires a stage root")
                    })?;
                let mut writer =
                    RecordWriter::new(&root).map_err(record_decode_error::<A::Error>)?;
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

/// Stable external sort with one fixed-size run buffer and two merge heads.
pub struct RecordSorter<T> {
    root: RecordStageRoot,
    chunk: Vec<T>,
    runs: [Option<DiskRecords<T>>; MERGE_SLOTS],
    compare: fn(&T, &T) -> Ordering,
    peak: usize,
}
impl<T: Serialize + DeserializeOwned> RecordSorter<T> {
    pub fn new(root: &RecordStageRoot, compare: fn(&T, &T) -> Ordering) -> Self {
        Self {
            root: root.clone(),
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
        let mut out = RecordWriter::new(&self.root)?;
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
        self.chunk.sort_by(self.compare);
        let mut out = RecordWriter::new(&self.root)?;
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
                    Some(recent) => self.merge(run, recent)?,
                    None => run,
                });
            }
        }
        Ok(result.unwrap_or_else(DiskRecords::empty))
    }
}

struct DecodeContext {
    root: RecordStageRoot,
    error: Option<KioError>,
}

thread_local! {
    static DECODE_CONTEXT: std::cell::RefCell<Vec<DecodeContext>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn record_decode_error<E: serde::de::Error>(error: KioError) -> E {
    let converted = E::custom(&error);
    DECODE_CONTEXT.with(|context| {
        if let Some(context) = context.borrow_mut().last_mut() {
            context.error = Some(error);
        }
    });
    converted
}

/// Pass the caller's retained stage authority through derived serde visitors.
/// The scope is local to this thread and nests with other JSON decoding scopes.
pub fn decode_json<T: DeserializeOwned>(root: &RecordStageRoot, reader: impl Read) -> Result<T> {
    struct Scope;
    impl Drop for Scope {
        fn drop(&mut self) {
            DECODE_CONTEXT.with(|context| {
                context.borrow_mut().pop();
            });
        }
    }
    DECODE_CONTEXT.with(|context| {
        context.borrow_mut().push(DecodeContext {
            root: root.clone(),
            error: None,
        })
    });
    let scope = Scope;
    let decoded = serde_json::from_reader(reader);
    let error = DECODE_CONTEXT.with(|context| {
        context
            .borrow_mut()
            .last_mut()
            .and_then(|context| context.error.take())
    });
    drop(scope);
    if let Some(error) = error {
        return Err(error);
    }
    decoded.map_err(|error| KioError::schema(error.to_string()))
}

/// Bound one lexical token before serde can grow its string/number scratch.
/// JSON whitespace and entry count are unbounded; only the live token is capped.
pub fn decode_bounded_json<T: DeserializeOwned>(
    root: &RecordStageRoot,
    reader: impl Read,
) -> Result<T> {
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
    let parsed = decode_json(root, &mut source);
    if let Some(error) = source.error {
        return Err(error);
    }
    parsed
}

#[cfg(test)]
fn test_store_root(fixture: &tempfile::TempDir) -> RecordStageRoot {
    let directory = StoreDirectory::open(fixture.path()).unwrap();
    crate::store_dir::restrict_new_private_directory(&directory.root_handle()).unwrap();
    RecordStageRoot::in_store(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sealed_records_reject_valid_same_length_edits_and_appends() {
        let records =
            DiskRecords::from_records(&RecordStageRoot::temporary().unwrap(), [11u64, 22]).unwrap();
        let path = records
            .run
            .as_ref()
            .unwrap()
            .directory
            .path()
            .join("records");
        let mut same_length = std::fs::read(&path).unwrap();
        same_length[FRAME_TAG_BYTES..FRAME_TAG_BYTES + 2].copy_from_slice(b"33");
        std::fs::write(&path, same_length).unwrap();
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
    fn cursor_rejects_an_edit_after_opening_a_verified_run() {
        let records =
            DiskRecords::from_records(&RecordStageRoot::temporary().unwrap(), [11u64, 22]).unwrap();
        let path = records
            .run
            .as_ref()
            .unwrap()
            .directory
            .path()
            .join("records");
        let mut cursor = SortedRecordCursor::new(records.iter(), |_: &u64| "path");
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        file.seek(SeekFrom::Start(FRAME_TAG_BYTES as u64)).unwrap();
        std::io::Write::write_all(&mut file, b"33").unwrap();
        assert!(
            cursor.at("path").is_err(),
            "the cursor used bytes changed after open"
        );
    }

    #[test]
    fn snapshots_seal_the_exact_bytes_observed_at_creation() {
        let mut writer = RecordWriter::new(&RecordStageRoot::temporary().unwrap()).unwrap();
        writer.push(&11u64).unwrap();
        let snapshot = writer.snapshot();
        assert_eq!(snapshot.first().unwrap(), Some(11));
        writer.push(&22).unwrap();
        assert!(snapshot.first().is_err());
        assert_eq!(writer.finish().unwrap().to_vec().unwrap(), vec![11, 22]);
    }

    #[test]
    fn cleanup_removes_only_verified_stages_and_empty_owned_directories() {
        let records =
            DiskRecords::from_records(&RecordStageRoot::temporary().unwrap(), [11u64]).unwrap();
        let path = records.run.as_ref().unwrap().directory.path().to_owned();
        drop(records);
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_preserves_replaced_record_file() {
        let fixture = tempfile::tempdir().unwrap();
        let root = test_store_root(&fixture);
        let mut writer = RecordWriter::new(&root).unwrap();
        writer.push(&11u64).unwrap();
        let records = writer.finish().unwrap();
        let directory = records.run.as_ref().unwrap().directory.path().to_owned();
        let path = directory.join("records");
        std::fs::rename(&path, fixture.path().join("original-records")).unwrap();
        let mut replacement = StageWriter::create(
            &records.run.as_ref().unwrap().directory,
            Path::new("records"),
        )
        .unwrap();
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
        let root = test_store_root(&fixture);
        let mut writer = RecordWriter::new(&root).unwrap();
        writer.push(&11u64).unwrap();
        let records = writer.finish().unwrap();
        let path = records.run.as_ref().unwrap().directory.path().to_owned();
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
            let mut sorter = RecordSorter::new(
                &RecordStageRoot::temporary().unwrap(),
                |a: &u64, b: &u64| a.cmp(b),
            );
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
    fn external_sort_preserves_equal_key_order_across_runs() {
        let root = RecordStageRoot::temporary().unwrap();
        let mut sorter = RecordSorter::new(&root, |a: &(usize, usize), b| a.0.cmp(&b.0));
        let count = SORT_BUFFER_RECORDS * 2 + 1;
        for position in 0..count {
            sorter.push((position % 2, position)).unwrap();
        }
        let sorted = sorter.finish().unwrap();
        let expected =
            (0..2).flat_map(|key| (key..count).step_by(2).map(move |position| (key, position)));
        assert!(sorted.iter().map(|row| row.unwrap()).eq(expected));
    }

    #[test]
    fn record_token_buffer_and_read_errors_never_grow_with_record_count() {
        let error = RecordWriter::new(&RecordStageRoot::temporary().unwrap())
            .unwrap()
            .push(&"x".repeat(RECORD_BYTES + 1))
            .unwrap_err();
        assert_eq!(error.resource_limit().unwrap().resource, Resource::Memory);
        let records = DiskRecords::from_records(
            &RecordStageRoot::temporary().unwrap(),
            ["a".to_owned(), "b".to_owned()],
        )
        .unwrap();
        std::fs::write(
            records
                .run
                .as_ref()
                .unwrap()
                .directory
                .path()
                .join("records"),
            b"not a record\n",
        )
        .unwrap();
        assert!(records.get(1).is_err());
        assert!(records.any(|_| true).is_err());
    }
}
