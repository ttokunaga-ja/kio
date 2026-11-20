//! Private, identity-bound records and a bounded external merge sort.
//! Small record sequences stay in memory. Larger sequences spill to recoverable
//! stages or anonymous scratch files. The sorter keeps a fixed chunk, bounded
//! merge slots and two heads.
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
    io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
};

pub const SORT_BUFFER_RECORDS: usize = 256;
pub const MERGE_SLOTS: usize = usize::BITS as usize;
const RECORD_BYTES: usize = 64 * 1024;
const FRAME_TAG_BYTES: usize = 32;
// One maximum-sized record and its line terminator.
const MEMORY_RUN_BYTES: usize = RECORD_BYTES + 1;
// Even callers retaining many small runs (e.g. a history walk) only keep
// one sort chunk's worth of serialized run data in memory.
const LIVE_MEMORY_BYTES: usize = SORT_BUFFER_RECORDS * RECORD_BYTES;
const RECORD_STAGES: &str = "record-stages";

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
/// Storage and memory authority shared by a group of record runs.
#[derive(Clone, Debug)]
pub struct RecordStageRoot {
    location: RecordLocation,
    memory: Arc<RecordMemoryBudget>,
}

#[derive(Clone, Debug)]
enum RecordLocation {
    Store(StoreDirectory),
    Temporary(Arc<ScratchDirectory>),
    Anonymous,
}

#[derive(Debug, Default)]
pub(crate) struct RecordMemoryBudget(AtomicUsize);

impl RecordMemoryBudget {
    fn store(self: &Arc<Self>, bytes: Vec<u8>) -> std::result::Result<Arc<MemoryRun>, Vec<u8>> {
        if self
            .0
            .fetch_update(Relaxed, Relaxed, |used| {
                let total = used + bytes.len();
                (total <= LIVE_MEMORY_BYTES).then_some(total)
            })
            .is_err()
        {
            return Err(bytes);
        }
        Ok(Arc::new(MemoryRun {
            bytes: bytes.into_boxed_slice(),
            budget: self.clone(),
        }))
    }
}

struct MemoryRun {
    bytes: Box<[u8]>,
    budget: Arc<RecordMemoryBudget>,
}

impl Drop for MemoryRun {
    fn drop(&mut self) {
        self.budget.0.fetch_sub(self.bytes.len(), Relaxed);
    }
}

#[derive(Debug)]
struct ScratchDirectory {
    parent: StoreDirectory,
    leaf: PathBuf,
    directory: StoreDirectory,
    preserve: std::sync::atomic::AtomicBool,
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        if *self.preserve.get_mut() {
            return;
        }
        // Traverse only within the retained private directory. Retiring its
        // name separately prevents cleanup from descending into a replacement.
        let registry = Path::new(RECORD_STAGES);
        if matches!(self.directory.contains_entry(registry), Ok(true))
            && self.directory.remove_directory_all(registry).is_err()
        {
            return;
        }
        let _ = crate::platform::stage_operations::discard_empty_directory(
            &self.parent,
            &self.leaf,
            &self.directory.root_handle(),
        );
    }
}

fn scratch_name() -> Result<PathBuf> {
    let mut nonce = [0; 32];
    getrandom::fill(&mut nonce)
        .map_err(|error| KioError::io(error.to_string(), "record scratch"))?;
    Ok(PathBuf::from(format!(
        "kio-records-{}",
        crate::cas::lower_hex(&nonce)
    )))
}

fn scratch_parent() -> Result<StoreDirectory> {
    let path = std::path::absolute(std::env::temp_dir()).map_err(scratch_io)?;
    crate::private_fs::retain_external_directory(&path)
}

impl RecordStageRoot {
    pub(crate) fn in_store(directory: StoreDirectory, memory: Arc<RecordMemoryBudget>) -> Self {
        Self {
            location: RecordLocation::Store(directory),
            memory,
        }
    }

    pub(crate) fn anonymous(memory: Arc<RecordMemoryBudget>) -> Self {
        Self {
            location: RecordLocation::Anonymous,
            memory,
        }
    }

    /// Allocate a private workspace for callers without a writable store.
    /// DiskRecords retain it until the last record reader has finished.
    pub fn temporary() -> Result<Self> {
        let parent = scratch_parent()?;
        let leaf = scratch_name()?;
        let file = parent.create_directory(&leaf)?;
        let directory = StoreDirectory::from_retained(file, parent.path().join(&leaf))?;
        let scratch = Arc::new(ScratchDirectory {
            parent,
            leaf,
            directory,
            preserve: false.into(),
        });
        crate::private_fs::verify_private_directory_handle(&scratch.directory)?;
        Ok(Self {
            location: RecordLocation::Temporary(scratch),
            memory: Arc::default(),
        })
    }

    fn registry(&self, create: bool) -> Result<Option<StoreDirectory>> {
        let parent = match &self.location {
            RecordLocation::Store(directory) => {
                crate::private_fs::verify_private_directory_handle(directory)?;
                let Some(internal) = stages::container(directory, "internal", create)? else {
                    return Ok(None);
                };
                internal
            }
            RecordLocation::Temporary(scratch) => {
                crate::private_fs::verify_private_directory_handle(&scratch.directory)?;
                scratch.directory.clone()
            }
            RecordLocation::Anonymous => return Ok(None),
        };
        stages::container(&parent, RECORD_STAGES, create)
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

    fn recover(&self) -> Result<()> {
        if PRESERVE_STAGES.with(|depth| depth.get() == 0)
            && let Some(registry) = self.registry(false)?
        {
            stages::inspect(&registry, true)?;
        }
        Ok(())
    }

    fn preserve_scratch(&self) {
        if let RecordLocation::Temporary(scratch) = &self.location {
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
fn new_run(root: &RecordStageRoot) -> Result<(Arc<RunFile>, StageWriter)> {
    let mut key = [0; 32];
    getrandom::fill(&mut key)
        .map_err(|error| KioError::io(error.to_string(), "record stage key"))?;
    let created = RunDirectory::create(root);
    if created.is_err() {
        root.preserve_scratch();
    }
    let (location, writer) = created?;
    let run = Arc::new(RunFile {
        directory: location.directory.clone(),
        identity: writer.identity().clone(),
        frames: FrameAuthenticator::new(&key),
        _location: location,
    });
    Ok((run, writer))
}

/// A short reference to one serialized value in a private record stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PayloadRef {
    offset: u64,
    size: usize,
    digest: [u8; 32],
}

impl PayloadRef {
    pub fn serialized_size(self) -> usize {
        self.size
    }
}

pub enum PayloadWriteError<E> {
    Admission(E),
    Storage(KioError),
}

/// Pack already size-validated bodies in a bounded buffer, spilling to a
/// private record stage when the buffer or shared memory budget is exhausted.
pub struct PayloadWriter<T> {
    root: RecordStageRoot,
    buffer: PayloadBuffer,
    size: u64,
    _value: PhantomData<T>,
}

enum PayloadBuffer {
    Memory(Vec<u8>),
    Disk(Box<(Arc<RunFile>, StageWriter)>),
}

impl<T: Serialize> PayloadWriter<T> {
    pub fn new(root: &RecordStageRoot) -> Result<Self> {
        if let Err(error) = root.recover() {
            root.preserve_scratch();
            return Err(error);
        }
        Ok(Self {
            root: root.clone(),
            buffer: PayloadBuffer::Memory(Vec::new()),
            size: 0,
            _value: PhantomData,
        })
    }

    pub fn push(&mut self, value: &T) -> Result<PayloadRef> {
        let bytes =
            serde_json::to_vec(value).map_err(|error| KioError::schema(error.to_string()))?;
        self.push_bytes(&bytes)
    }

    /// Admit the serialization before allocating its buffer.
    pub fn push_accounted<E>(
        &mut self,
        value: &T,
        admit: impl FnOnce(usize) -> std::result::Result<(), E>,
    ) -> std::result::Result<PayloadRef, PayloadWriteError<E>> {
        #[derive(Default)]
        struct Counter(usize);
        impl std::io::Write for Counter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self
                    .0
                    .checked_add(bytes.len())
                    .ok_or_else(|| std::io::Error::other("private payload size overflows"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut length = Counter::default();
        let schema = |error: serde_json::Error| {
            PayloadWriteError::Storage(KioError::schema(error.to_string()))
        };
        serde_json::to_writer(&mut length, value).map_err(schema)?;
        admit(length.0).map_err(PayloadWriteError::Admission)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length.0).map_err(|_| {
            PayloadWriteError::Storage(
                ResourceLimit {
                    resource: Resource::Memory,
                    stage: "record_payload".to_owned(),
                    limit: None,
                    observed: Some(length.0 as u64),
                    resource_tier: None,
                    page_progress: None,
                }
                .into_error("cannot allocate a private record payload"),
            )
        })?;
        serde_json::to_writer(&mut bytes, value).map_err(schema)?;
        self.push_bytes(&bytes).map_err(PayloadWriteError::Storage)
    }

    fn push_bytes(&mut self, bytes: &[u8]) -> Result<PayloadRef> {
        let reference = PayloadRef {
            offset: self.size,
            size: bytes.len(),
            digest: Sha256::digest(bytes).into(),
        };
        let end = self
            .size
            .checked_add(bytes.len() as u64)
            .ok_or_else(changed_records)?;
        if let PayloadBuffer::Memory(buffer) = &mut self.buffer {
            if end <= RECORD_BYTES as u64 {
                buffer.extend_from_slice(bytes);
                self.size = end;
                return Ok(reference);
            }
            let (run, mut writer) = new_run(&self.root)?;
            writer.write_chunk(buffer)?;
            self.buffer = PayloadBuffer::Disk(Box::new((run, writer)));
        }
        let PayloadBuffer::Disk(disk) = &mut self.buffer else {
            unreachable!("payloads above the memory bound must spill");
        };
        for part in bytes.chunks(RECORD_BYTES) {
            disk.1.write_chunk(part)?;
        }
        self.size = end;
        Ok(reference)
    }

    pub fn finish(self) -> Result<RecordPayloads<T>> {
        let storage = match self.buffer {
            PayloadBuffer::Memory(bytes) => match self.root.memory.store(bytes) {
                Ok(memory) => PayloadStorage::Memory(memory),
                Err(bytes) => {
                    let (run, mut writer) = new_run(&self.root)?;
                    writer.write_chunk(&bytes)?;
                    seal_payload_file(run, writer, self.size)?
                }
            },
            PayloadBuffer::Disk(disk) => {
                let (run, writer) = *disk;
                seal_payload_file(run, writer, self.size)?
            }
        };
        Ok(RecordPayloads {
            storage,
            size: self.size,
            _value: PhantomData,
        })
    }
}

fn seal_payload_file(run: Arc<RunFile>, writer: StageWriter, size: u64) -> Result<PayloadStorage> {
    writer.sync()?;
    drop(writer);
    let file = run
        .directory
        .open_regular_read(Path::new("records"), size)?;
    run.identity.verify(&file)?;
    Ok(PayloadStorage::Disk {
        file: Mutex::new(file),
        run,
    })
}

enum PayloadStorage {
    Memory(Arc<MemoryRun>),
    Disk {
        file: Mutex<File>,
        run: Arc<RunFile>,
    },
}

/// Sealed payloads with digest-verified random access.
pub struct RecordPayloads<T> {
    storage: PayloadStorage,
    size: u64,
    _value: PhantomData<T>,
}

impl<T> std::fmt::Debug for RecordPayloads<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordPayloads")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl<T: DeserializeOwned> RecordPayloads<T> {
    pub fn read(&self, reference: PayloadRef) -> Result<T> {
        let end = reference
            .offset
            .checked_add(reference.size as u64)
            .filter(|end| *end <= self.size)
            .ok_or_else(changed_records)?;
        let data;
        let bytes = match &self.storage {
            PayloadStorage::Memory(memory) => {
                &memory.bytes[reference.offset as usize..end as usize]
            }
            PayloadStorage::Disk { file, run } => {
                let mut file = file.lock().map_err(|_| changed_records())?;
                run._location.identity.verify(&run.directory)?;
                run.identity.verify(&file)?;
                if file
                    .metadata()
                    .map_err(|error| crate::store_dir::ioerr(Path::new("records"), error))?
                    .len()
                    != self.size
                {
                    return Err(changed_records());
                }
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(reference.size).map_err(|_| {
                    ResourceLimit {
                        resource: Resource::Memory,
                        stage: "record_payload".to_owned(),
                        limit: None,
                        observed: Some(reference.size as u64),
                        resource_tier: None,
                        page_progress: None,
                    }
                    .into_error("cannot allocate a private record payload")
                })?;
                bytes.resize(reference.size, 0);
                file.seek(SeekFrom::Start(reference.offset))
                    .and_then(|_| file.read_exact(&mut bytes))
                    .map_err(|error| crate::store_dir::ioerr(Path::new("records"), error))?;
                data = bytes;
                &data
            }
        };
        if <[u8; 32]>::from(Sha256::digest(bytes)) != reference.digest {
            return Err(changed_records());
        }
        serde_json::from_slice(bytes).map_err(|error| KioError::schema(error.to_string()))
    }
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
struct DiskRun {
    run: Arc<RunFile>,
    size: u64,
    digest: [u8; 32],
}
impl DiskRun {
    fn open(&self) -> Result<BufReader<File>> {
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
        Ok(BufReader::with_capacity(RECORD_BYTES, file))
    }
    fn reader(&self) -> Result<RecordReader> {
        Ok(RecordReader::Disk {
            reader: self.open()?,
            run: self.run.clone(),
            ordinal: 0,
            buffer: Vec::new(),
        })
    }
}

// Each iterator owns its cursor. Seek and read under the same lock so cloned
// records and interleaved iterators can share the anonymous scratch handle.
struct ScratchReader {
    file: Arc<Mutex<File>>,
    offset: u64,
}
impl Read for ScratchReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| std::io::Error::other("record scratch lock poisoned"))?;
        file.seek(SeekFrom::Start(self.offset))?;
        let count = file.read(bytes)?;
        self.offset += count as u64;
        Ok(count)
    }
}

#[derive(Clone)]
enum RecordStorage {
    Memory(Arc<MemoryRun>),
    Disk(Arc<DiskRun>),
    Scratch(Arc<Mutex<File>>),
}

/// Repeatable, bounded storage for a sequence of private records.
pub struct DiskRecords<T> {
    storage: RecordStorage,
    count: usize,
    _value: PhantomData<T>,
}
impl<T> Clone for DiskRecords<T> {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            count: self.count,
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
            storage: RecordStorage::Memory(Arc::new(MemoryRun {
                bytes: Box::default(),
                budget: Arc::default(),
            })),
            count: 0,
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
        let reader = match &self.storage {
            RecordStorage::Memory(bytes) => Ok(RecordReader::memory(bytes)),
            RecordStorage::Disk(run) => run.reader(),
            RecordStorage::Scratch(file) => Ok(RecordReader::Scratch {
                reader: BufReader::with_capacity(
                    RECORD_BYTES,
                    ScratchReader {
                        file: file.clone(),
                        offset: 0,
                    },
                ),
                buffer: Vec::new(),
            }),
        };
        RecordIter::new(reader, self.count)
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

enum WriterStorage {
    Memory(Vec<u8>),
    Disk(Box<DiskWriter>),
    Scratch(BufWriter<File>),
}

impl RecordStageRoot {
    fn spill(&self, bytes: &[u8]) -> Result<WriterStorage> {
        match self.location {
            RecordLocation::Store(_) | RecordLocation::Temporary(_) => {
                let mut writer = DiskWriter::create(self)?;
                writer.write_lines(bytes)?;
                Ok(WriterStorage::Disk(Box::new(writer)))
            }
            RecordLocation::Anonymous => {
                let file = scratch_file()?;
                let mut writer = BufWriter::with_capacity(RECORD_BYTES, file);
                writer.write_all(bytes).map_err(scratch_io)?;
                Ok(WriterStorage::Scratch(writer))
            }
        }
    }
}

fn scratch_io(error: std::io::Error) -> KioError {
    crate::store_dir::ioerr(Path::new("record scratch"), error)
}

#[cfg(unix)]
fn scratch_file() -> Result<File> {
    tempfile::tempfile().map_err(scratch_io)
}

#[cfg(windows)]
fn scratch_file() -> Result<File> {
    use crate::platform::private_fs::{NewPrivateObjectKind, create_new_private_at};
    create_new_private_at(
        &scratch_parent()?.root_handle(),
        scratch_name()?.as_os_str(),
        NewPrivateObjectKind::ScratchFile,
    )
    .map_err(scratch_io)
}

impl WriterStorage {
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Disk(writer) => writer.write_lines(bytes),
            Self::Scratch(writer) => writer.write_all(bytes).map_err(scratch_io),
            Self::Memory(buffer) => {
                buffer.extend_from_slice(bytes);
                Ok(())
            }
        }
    }

    fn finish(self) -> Result<RecordStorage> {
        match self {
            Self::Disk(writer) => writer.finish(),
            Self::Scratch(writer) => {
                let file = writer
                    .into_inner()
                    .map_err(|error| scratch_io(error.into_error()))?;
                Ok(RecordStorage::Scratch(Arc::new(Mutex::new(file))))
            }
            Self::Memory(_) => unreachable!("in-memory records must be accounted before sealing"),
        }
    }
}

struct DiskWriter {
    writer: StageWriter,
    run: Arc<RunFile>,
    size: u64,
    hasher: Sha256,
    ordinal: usize,
    frame: Vec<u8>,
}
impl DiskWriter {
    fn create(root: &RecordStageRoot) -> Result<Self> {
        let (run, writer) = new_run(root)?;
        Ok(Self {
            writer,
            run,
            size: 0,
            hasher: Sha256::new(),
            ordinal: 0,
            frame: Vec::new(),
        })
    }

    fn write_lines(&mut self, bytes: &[u8]) -> Result<()> {
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            self.frame.clear();
            self.frame
                .extend_from_slice(&self.run.frames.tag(self.ordinal, line));
            self.frame.extend_from_slice(line);
            let size = self
                .size
                .checked_add(self.frame.len() as u64)
                .ok_or_else(|| record_limit(usize::MAX))?;
            self.writer.write_chunk(&self.frame)?;
            self.hasher.update(&self.frame);
            self.size = size;
            self.ordinal += 1;
        }
        Ok(())
    }

    fn sealed_run(&self) -> DiskRun {
        DiskRun {
            run: self.run.clone(),
            size: self.size,
            digest: self.hasher.clone().finalize().into(),
        }
    }

    fn finish(self) -> Result<RecordStorage> {
        self.writer.sync()?;
        Ok(RecordStorage::Disk(Arc::new(self.sealed_run())))
    }
}

pub struct RecordWriter<T> {
    root: RecordStageRoot,
    storage: WriterStorage,
    count: usize,
    _value: PhantomData<T>,
}
impl<T: Serialize> RecordWriter<T> {
    pub fn new(root: &RecordStageRoot) -> Result<Self> {
        if let Err(error) = root.recover() {
            root.preserve_scratch();
            return Err(error);
        }
        Ok(Self {
            root: root.clone(),
            storage: WriterStorage::Memory(Vec::new()),
            count: 0,
            _value: PhantomData,
        })
    }

    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub(crate) fn iter(&self) -> Box<dyn Iterator<Item = Result<T>> + '_>
    where
        T: DeserializeOwned,
    {
        match &self.storage {
            WriterStorage::Memory(bytes) => {
                Box::new(bytes.split_inclusive(|b| *b == b'\n').map(decode_record))
            }
            WriterStorage::Disk(writer) => {
                Box::new(RecordIter::new(writer.sealed_run().reader(), self.count))
            }
            WriterStorage::Scratch(_) => {
                unreachable!("anonymous record runs are read after sealing")
            }
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
        let count = self
            .count
            .checked_add(1)
            .ok_or_else(|| record_limit(usize::MAX))?;
        if let WriterStorage::Memory(buffer) = &self.storage
            && (count > SORT_BUFFER_RECORDS || buffer.len() + bytes.len() > MEMORY_RUN_BYTES)
        {
            let mut storage = self.root.spill(buffer)?;
            storage.append(&bytes)?;
            self.storage = storage;
        } else {
            self.storage.append(&bytes)?;
        }
        self.count = count;
        Ok(())
    }
    pub fn finish(self) -> Result<DiskRecords<T>> {
        let storage = match self.storage {
            WriterStorage::Memory(bytes) => match self.root.memory.store(bytes) {
                Ok(run) => RecordStorage::Memory(run),
                Err(bytes) => self.root.spill(&bytes)?.finish()?,
            },
            spilled => spilled.finish()?,
        };
        Ok(DiskRecords {
            storage,
            count: self.count,
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
        page_progress: None,
    }
    .into_error("tree metadata record exceeds the streaming buffer")
}
enum RecordReader {
    Memory {
        run: Arc<MemoryRun>,
        position: usize,
    },
    Disk {
        reader: BufReader<File>,
        run: Arc<RunFile>,
        ordinal: usize,
        buffer: Vec<u8>,
    },
    Scratch {
        reader: BufReader<ScratchReader>,
        buffer: Vec<u8>,
    },
}
impl RecordReader {
    fn memory(run: &Arc<MemoryRun>) -> Self {
        Self::Memory {
            run: run.clone(),
            position: 0,
        }
    }

    fn next_line(&mut self) -> Result<&[u8]> {
        fn truncated() -> KioError {
            KioError::schema("truncated private record stage")
        }

        match self {
            Self::Memory { run, position } => {
                let tail = &run.bytes[*position..];
                let end = tail
                    .iter()
                    .position(|b| *b == b'\n')
                    .ok_or_else(truncated)?;
                *position += end + 1;
                Ok(&tail[..end])
            }
            Self::Disk {
                reader,
                run,
                ordinal,
                buffer,
            } => {
                let mut tag = [0; FRAME_TAG_BYTES];
                reader
                    .read_exact(&mut tag)
                    .map_err(|error| crate::store_dir::ioerr(Path::new("records"), error))?;
                let line = read_record_line(reader, buffer)?;
                run.frames.verify(*ordinal, line, tag)?;
                *ordinal += 1;
                Ok(line)
            }
            Self::Scratch { reader, buffer } => read_record_line(reader, buffer),
        }
    }
}

fn read_record_line<'a>(reader: &mut impl BufRead, buffer: &'a mut Vec<u8>) -> Result<&'a [u8]> {
    buffer.clear();
    // A malformed run cannot grow a reader's line buffer beyond one record.
    loop {
        let bytes = reader
            .fill_buf()
            .map_err(|error| crate::store_dir::ioerr(Path::new("records"), error))?;
        if bytes.is_empty() {
            return Err(KioError::schema("truncated private record stage"));
        }
        let end = bytes.iter().position(|byte| *byte == b'\n');
        let count = end.map_or(bytes.len(), |index| index + 1);
        if buffer.len().saturating_add(count) > RECORD_BYTES + 1 {
            return Err(record_limit(buffer.len().saturating_add(count)));
        }
        buffer.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(buffer);
        }
    }
}

pub struct RecordIter<T> {
    reader: Option<RecordReader>,
    remaining: usize,
    error: Option<KioError>,
    _value: PhantomData<T>,
}
impl<T> RecordIter<T> {
    fn new(opened: Result<RecordReader>, remaining: usize) -> Self {
        let (reader, error) = match opened {
            Ok(reader) => (Some(reader), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            reader,
            remaining,
            error,
            _value: PhantomData,
        }
    }
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
        let result = self
            .reader
            .as_mut()
            .expect("opened record reader")
            .next_line()
            .and_then(decode_record);
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

fn decode_record<T: DeserializeOwned>(line: &[u8]) -> Result<T> {
    serde_json::from_slice(line).map_err(|e| KioError::schema(e.to_string()))
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
        result.map_or_else(|| RecordWriter::new(&self.root)?.finish(), Ok)
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
    let parent = crate::private_fs::retain_external_directory(fixture.path()).unwrap();
    let directory = StoreDirectory::from_retained(
        parent.create_directory(Path::new("store")).unwrap(),
        parent.path().join("store"),
    )
    .unwrap();
    RecordStageRoot::in_store(directory, Arc::default())
}

#[cfg(test)]
fn test_store_directory(root: &RecordStageRoot) -> &StoreDirectory {
    let RecordLocation::Store(directory) = &root.location else {
        panic!("expected a store directory");
    };
    directory
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_record_root_is_private_and_removed_after_last_reader() {
        let root = RecordStageRoot::temporary().unwrap();
        let RecordLocation::Temporary(scratch) = &root.location else {
            panic!("expected a retained temporary directory");
        };
        crate::private_fs::verify_private_directory_handle(&scratch.directory).unwrap();
        let path = scratch.directory.path().to_owned();
        let records = spilled_records(&root);
        let mut reader = records.iter();
        drop(root);
        drop(records);
        assert!(path.is_dir());
        assert_eq!(reader.next().unwrap().unwrap(), 11);
        drop(reader);
        assert!(!path.exists());
    }

    #[test]
    fn anonymous_record_spill_uses_disposable_scratch() {
        let root = RecordStageRoot::anonymous(Arc::default());
        let records = spilled_records(&root);
        assert!(matches!(&records.storage, RecordStorage::Scratch(_)));
        #[cfg(windows)]
        if let RecordStorage::Scratch(file) = &records.storage {
            crate::platform::verify_owner_private_handle(&file.lock().unwrap()).unwrap();
        }
        assert_eq!(records.first().unwrap(), Some(11));
        assert!(root.inspect().unwrap().is_empty());
    }

    fn spilled_records(root: &RecordStageRoot) -> DiskRecords<usize> {
        DiskRecords::from_records(root, 11..=11 + SORT_BUFFER_RECORDS).unwrap()
    }

    fn disk_run<T>(records: &DiskRecords<T>) -> &RunFile {
        match &records.storage {
            RecordStorage::Disk(run) => &run.run,
            RecordStorage::Memory(_) | RecordStorage::Scratch(_) => {
                panic!("expected a durable record stage")
            }
        }
    }

    #[test]
    fn small_records_stay_in_memory_and_larger_inputs_spill() {
        let fixture = tempfile::tempdir().unwrap();
        let root = test_store_root(&fixture);
        let stage_dir = test_store_directory(&root)
            .path()
            .join("internal/record-stages/stages");
        let mut writer = RecordWriter::new(&root).unwrap();
        for value in 0..SORT_BUFFER_RECORDS {
            writer.push(&value).unwrap();
        }
        assert!(!stage_dir.exists(), "a full in-memory chunk needs no stage");
        writer.push(&SORT_BUFFER_RECORDS).unwrap();
        let records = writer.finish().unwrap();
        assert_eq!(
            records.to_vec().unwrap(),
            (0..=SORT_BUFFER_RECORDS).collect::<Vec<_>>()
        );
        assert_eq!(std::fs::read_dir(&stage_dir).unwrap().count(), 1);
        drop(records);
        assert_eq!(std::fs::read_dir(&stage_dir).unwrap().count(), 0);
    }

    #[test]
    fn record_byte_limit_spills_even_below_the_count_limit() {
        let fixture = tempfile::tempdir().unwrap();
        let root = test_store_root(&fixture);
        let stage_dir = test_store_directory(&root)
            .path()
            .join("internal/record-stages/stages");
        let value = "x".repeat(RECORD_BYTES - 2);
        let mut writer = RecordWriter::new(&root).unwrap();
        writer.push(&value).unwrap();
        assert!(!stage_dir.exists(), "one maximum-sized record fits");
        writer.push(&String::new()).unwrap();
        let records = writer.finish().unwrap();
        assert_eq!(records.to_vec().unwrap(), vec![value, String::new()]);
        assert_eq!(std::fs::read_dir(&stage_dir).unwrap().count(), 1);
    }

    #[test]
    fn small_payloads_use_memory_and_large_payloads_spill() {
        let fixture = tempfile::tempdir().unwrap();
        let root = test_store_root(&fixture);
        let stage_dir = test_store_directory(&root)
            .path()
            .join("internal/record-stages/stages");
        let mut writer = PayloadWriter::new(&root).unwrap();
        let small = "x".repeat(100);
        let first = writer.push(&small).unwrap();
        let payloads = writer.finish().unwrap();
        assert_eq!(payloads.read(first).unwrap(), small);
        assert!(!stage_dir.exists(), "small payloads need no stage");
        let mut writer = PayloadWriter::new(&root).unwrap();
        let first = writer.push(&small).unwrap();
        let large = "y".repeat(RECORD_BYTES);
        let second = writer.push(&large).unwrap();
        let payloads = writer.finish().unwrap();
        assert_eq!(payloads.read(second).unwrap(), large);
        assert_eq!(payloads.read(first).unwrap(), small);
        assert_eq!(std::fs::read_dir(&stage_dir).unwrap().count(), 1);
    }

    #[test]
    fn small_sort_stays_in_memory_and_multiple_runs_spill() {
        let fixture = tempfile::tempdir().unwrap();
        let root = test_store_root(&fixture);
        let stage_dir = test_store_directory(&root)
            .path()
            .join("internal/record-stages/stages");
        for count in [SORT_BUFFER_RECORDS, SORT_BUFFER_RECORDS + 1] {
            let mut sorter = RecordSorter::new(&root, usize::cmp);
            for value in (0..count).rev() {
                sorter.push(value).unwrap();
            }
            let records = sorter.finish().unwrap();
            assert_eq!(records.to_vec().unwrap(), (0..count).collect::<Vec<_>>());
            if count <= SORT_BUFFER_RECORDS {
                assert!(!stage_dir.exists(), "one sorted chunk needs no stage");
            } else {
                assert_eq!(std::fs::read_dir(&stage_dir).unwrap().count(), 1);
            }
        }
    }

    #[test]
    fn live_small_runs_share_a_budget_until_the_last_reader_drops() {
        let fixture = tempfile::tempdir().unwrap();
        let root = test_store_root(&fixture);
        let store = crate::cas::ObjectStore::new(test_store_directory(&root).path());
        let value = "x".repeat(RECORD_BYTES - 3);
        let mut readers = Vec::new();
        for _ in 0..LIVE_MEMORY_BYTES / RECORD_BYTES {
            let root = store.clone().record_stages().unwrap();
            let records = DiskRecords::from_records(&root, [value.clone()]).unwrap();
            assert!(matches!(records.storage, RecordStorage::Memory(_)));
            readers.push(records.iter());
        }
        let root = store.record_stages().unwrap();
        let spilled = DiskRecords::from_records(&root, [42u64]).unwrap();
        assert!(disk_run(&spilled).directory.path().exists());
        assert_eq!(spilled.first().unwrap(), Some(42));

        drop(readers.pop());
        let records = DiskRecords::from_records(&root, [43u64]).unwrap();
        assert!(matches!(records.storage, RecordStorage::Memory(_)));
        assert_eq!(records.first().unwrap(), Some(43));
    }

    #[test]
    fn sealed_records_reject_valid_same_length_edits_and_appends() {
        let fixture = tempfile::tempdir().unwrap();
        let records = spilled_records(&test_store_root(&fixture));
        let path = disk_run(&records).directory.path().join("records");
        let original = std::fs::read(&path).unwrap();
        let mut changed = original.clone();
        changed[FRAME_TAG_BYTES..FRAME_TAG_BYTES + 2].copy_from_slice(b"33");
        std::fs::write(&path, changed).unwrap();
        assert!(
            records.first().is_err(),
            "valid JSON cannot replace sealed authority"
        );
        std::fs::write(&path, [original.as_slice(), b"33\n"].concat()).unwrap();
        assert!(
            records.first().is_err(),
            "trailing records cannot be ignored"
        );
    }

    #[test]
    fn cursor_rejects_an_edit_after_opening_a_verified_run() {
        let fixture = tempfile::tempdir().unwrap();
        let records = spilled_records(&test_store_root(&fixture));
        let path = disk_run(&records).directory.path().join("records");
        let mut cursor = SortedRecordCursor::new(records.iter(), |_: &usize| "path");
        let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(FRAME_TAG_BYTES as u64)).unwrap();
        std::io::Write::write_all(&mut file, b"33").unwrap();
        let error = cursor
            .at("path")
            .expect_err("the cursor used bytes changed after open");
        assert_eq!(error.message(), changed_records().message());
    }

    #[test]
    fn writer_iteration_reads_the_current_records_before_and_after_spilling() {
        let fixture = tempfile::tempdir().unwrap();
        let root = test_store_root(&fixture);
        let mut writer = RecordWriter::new(&root).unwrap();
        writer.push(&0).unwrap();
        assert_eq!(writer.iter().collect::<Result<Vec<_>>>().unwrap(), [0]);
        for value in 1..=SORT_BUFFER_RECORDS {
            writer.push(&value).unwrap();
        }
        let expected = (0..=SORT_BUFFER_RECORDS).collect::<Vec<_>>();
        assert_eq!(writer.iter().collect::<Result<Vec<_>>>().unwrap(), expected);
        assert_eq!(writer.finish().unwrap().to_vec().unwrap(), expected);
    }

    #[test]
    fn cleanup_removes_only_verified_stages_and_empty_owned_directories() {
        let fixture = tempfile::tempdir().unwrap();
        let records = spilled_records(&test_store_root(&fixture));
        let path = disk_run(&records).directory.path().to_owned();
        drop(records);
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_preserves_replaced_record_file() {
        let fixture = tempfile::tempdir().unwrap();
        let records = spilled_records(&test_store_root(&fixture));
        let run = disk_run(&records);
        let directory = run.directory.path().to_owned();
        let path = directory.join("records");
        std::fs::rename(&path, fixture.path().join("original-records")).unwrap();
        let mut replacement = StageWriter::create(&run.directory, Path::new("records")).unwrap();
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
        let records = spilled_records(&test_store_root(&fixture));
        let path = disk_run(&records).directory.path().to_owned();
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
        let fixture = tempfile::tempdir().unwrap();
        let records = spilled_records(&test_store_root(&fixture));
        std::fs::write(
            disk_run(&records).directory.path().join("records"),
            b"not a record\n",
        )
        .unwrap();
        assert!(records.get(1).is_err());
        assert!(records.any(|_| true).is_err());
    }
}
