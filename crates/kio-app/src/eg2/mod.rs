//! Command-scoped access to the bundled EG2 engine. The application sends one
//! prepared input at a time and can publish only a validated complete vector.

mod input;
mod native;
mod platform;
mod profile;
mod runtime;
mod supervisor;
#[cfg(test)]
mod tests;
mod window;

use std::fs::File;
use std::io::{self, Read, Write};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use kio_core::control::ControlHandle;

pub use input::EmbeddingInput;
use input::PreparedInput;
pub use profile::{profile_hash, profile_value};
use supervisor::Worker;

pub const WORKER_ARGUMENT: &str = "--internal-eg2-worker";
pub const DIMENSIONS: usize = 768;
const TOKEN_CAP: u32 = 2048;
const MAX_TEXT_BYTES: usize = 32 * 1024;
const MAX_REQUEST_BYTES: usize = 1 + kio_adapter::types::MAX_EMBEDDING_ITEM_BYTES;
const REQUEST_ID_BYTES: usize = size_of::<u64>();
const MAX_RESPONSE_BYTES: usize = 1 + DIMENSIONS * 4;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const INFERENCE_TIMEOUT: Duration = Duration::from_secs(30);
const SLOT_WAIT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);
const MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineSlot {
    Index,
    Query,
}

impl EngineSlot {
    fn name(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::Query => "query",
        }
    }

    fn threads(self) -> u8 {
        match self {
            Self::Index => 2,
            Self::Query => {
                std::thread::available_parallelism().map_or(1, |cpus| cpus.get().min(4) as u8)
            }
        }
    }
}

#[derive(Clone, Copy)]
enum WireInput<'a> {
    Text(&'a str),
    Image(&'a [u8]),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[repr(u8)]
pub enum WorkerError {
    #[error("runtime_missing")]
    RuntimeMissing = 2,
    #[error("runtime_hash_mismatch")]
    RuntimeHashMismatch,
    #[error("runtime_unsupported_target")]
    RuntimeUnsupported,
    #[error("runtime_unavailable")]
    RuntimeUnavailable,
    #[error("model_missing")]
    ModelMissing,
    #[error("model_hash_mismatch")]
    ModelHashMismatch,
    #[error("model_unavailable")]
    ModelUnavailable,
    #[error("resource_busy")]
    ResourceBusy,
    #[error("worker_startup")]
    Startup,
    #[error("worker_crash")]
    Crash,
    #[error("worker_eof")]
    Eof,
    #[error("worker_protocol")]
    Protocol,
    #[error("worker_timeout")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
    #[error("worker_memory_limit")]
    MemoryLimit,
    #[error("input_limit")]
    InputLimit,
    #[error("input_overflow")]
    InputOverflow,
    #[error("native_inference")]
    Inference,
    #[error("invalid_input")]
    InvalidInput,
}

impl WorkerError {
    fn wire(self) -> u8 {
        self as u8
    }

    fn from_wire(byte: u8) -> Option<Self> {
        use WorkerError::*;
        [
            RuntimeMissing,
            RuntimeHashMismatch,
            RuntimeUnsupported,
            RuntimeUnavailable,
            ModelMissing,
            ModelHashMismatch,
            ModelUnavailable,
            ResourceBusy,
            Startup,
            Protocol,
            MemoryLimit,
            InputLimit,
            InputOverflow,
            Inference,
        ]
        .into_iter()
        .find(|reason| reason.wire() == byte)
    }
}

/// Runtime provenance from the worker that actually produced the vector.
#[derive(Debug)]
pub struct Producer {
    pub os: &'static str,
    pub arch: &'static str,
    pub runtime_version: &'static str,
    pub library_sha256: &'static str,
    pub cpu_backend: &'static str,
    pub threads: u8,
    pub profile_hash: &'static str,
}

#[derive(Debug)]
pub struct Embedding {
    pub vector: Vec<f32>,
    pub producer: Producer,
}

pub(crate) fn verify_runtime() -> Result<(), WorkerError> {
    let executable = std::env::current_exe().map_err(|_| WorkerError::RuntimeUnavailable)?;
    runtime::verify(&executable).map(|_| ())
}

/// Construct one session per command. No model work happens until the first
/// member. Fatal failures release the worker before returning to the caller.
pub struct EmbeddingSession {
    slot: EngineSlot,
    worker: Option<Worker>,
}

impl EmbeddingSession {
    pub fn new(slot: EngineSlot) -> Self {
        Self { slot, worker: None }
    }

    pub fn embed(
        &mut self,
        input: EmbeddingInput<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Embedding, WorkerError> {
        if cancelled() {
            self.worker.take();
            return Err(WorkerError::Cancelled);
        }
        let input = input.prepare()?;
        let pin = kio_core::litert::Bundle::for_target(runtime::TARGET)
            .ok_or(WorkerError::RuntimeUnsupported)?;
        if self.worker.is_none() {
            let executable = std::env::current_exe().map_err(|_| WorkerError::Startup)?;
            let mut command = Command::new(executable);
            command.args([WORKER_ARGUMENT, self.slot.name()]);
            self.worker = Some(Worker::spawn(command, STARTUP_TIMEOUT, cancelled, false)?);
        }
        let worker = self.worker.as_mut().expect("worker started");
        let threads = worker.threads;
        let result =
            embed_with_worker(worker, input, Instant::now() + INFERENCE_TIMEOUT, cancelled);
        if worker.child.is_none()
            || matches!(result, Err(WorkerError::Timeout | WorkerError::Cancelled))
        {
            self.worker.take();
        }
        result.map(|vector| Embedding {
            vector,
            producer: Producer {
                os: std::env::consts::OS,
                arch: std::env::consts::ARCH,
                runtime_version: kio_core::litert::VERSION,
                library_sha256: pin.library.sha256,
                cpu_backend: "cpu",
                threads,
                profile_hash: profile_hash(),
            },
        })
    }
}

fn embed_with_worker(
    worker: &mut Worker,
    input: PreparedInput<'_>,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<f32>, WorkerError> {
    // Every window shares the member deadline, including IPC and aggregation.
    let vector = input.embed(|payload| worker.embed(payload, deadline, cancelled))?;
    if cancelled() {
        return Err(WorkerError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(WorkerError::Timeout);
    }
    Ok(vector)
}

fn request(input: WireInput<'_>) -> Result<Vec<u8>, WorkerError> {
    let (tag, data, limit) = match input {
        WireInput::Text(text) => (0, text.as_bytes(), MAX_TEXT_BYTES),
        WireInput::Image(image) => (1, image, MAX_REQUEST_BYTES - 1),
    };
    if data.is_empty() || data.len() > limit {
        return Err(WorkerError::InputLimit);
    }
    let mut payload = Vec::with_capacity(data.len() + 1);
    payload.push(tag);
    payload.extend_from_slice(data);
    Ok(payload)
}

fn parse_request(frame: &[u8]) -> Result<WireInput<'_>, WorkerError> {
    match frame.split_first() {
        Some((0, bytes)) if !bytes.is_empty() && bytes.len() <= MAX_TEXT_BYTES => {
            std::str::from_utf8(bytes)
                .map(WireInput::Text)
                .map_err(|_| WorkerError::Protocol)
        }
        Some((1, bytes)) if !bytes.is_empty() => Ok(WireInput::Image(bytes)),
        _ => Err(WorkerError::Protocol),
    }
}

fn read_frame(reader: &mut impl Read, limit: usize) -> Result<Option<Vec<u8>>, WorkerError> {
    let mut length = [0; 4];
    match reader.read_exact(&mut length[..1]) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(_) => return Err(WorkerError::Protocol),
    }
    reader
        .read_exact(&mut length[1..])
        .map_err(|_| WorkerError::Protocol)?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > limit {
        return Err(WorkerError::Protocol);
    }
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .map_err(|_| WorkerError::Protocol)?;
    Ok(Some(bytes))
}

fn write_frame(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()
}

fn valid_vector(vector: &[f32]) -> bool {
    vector.len() == DIMENSIONS
        && vector.iter().all(|x| x.is_finite())
        && vector.iter().any(|x| *x != 0.0)
}

fn split_message(frame: &[u8]) -> Result<(&[u8; REQUEST_ID_BYTES], &[u8]), WorkerError> {
    frame.split_first_chunk().ok_or(WorkerError::Protocol)
}

fn acquire_slot(slot: EngineSlot, wait: Duration) -> Result<platform::SlotGate, WorkerError> {
    let deadline = std::time::Instant::now() + wait;
    let mut gate = platform::SlotGate::open(slot)?;
    loop {
        if gate.try_acquire()? {
            return Ok(gate);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(WorkerError::ResourceBusy);
        }
        std::thread::sleep(POLL.min(remaining));
    }
}

/// Called only by the executable's private worker entry point.
pub fn run_worker(slot: &str) -> Result<(), WorkerError> {
    let slot = match slot {
        "index" => EngineSlot::Index,
        "query" => EngineSlot::Query,
        _ => return Err(WorkerError::Protocol),
    };
    let mut input = std::io::stdin().lock();
    let mut start = [0];
    input.read_exact(&mut start).map_err(|_| WorkerError::Eof)?;
    drop(input);
    if start != [1] {
        return Err(WorkerError::Protocol);
    }

    let mut output = platform::private_stdout()?;
    let (sender, requests) = mpsc::sync_channel(0);
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            match read_frame(&mut input, REQUEST_ID_BYTES + MAX_REQUEST_BYTES) {
                Ok(Some(frame)) => {
                    if sender.send(frame).is_err() {
                        return;
                    }
                }
                // EOF also releases the engine if the parent dies while
                // inside native code. Failed framing is fatal to this worker.
                Ok(None) => platform::exit_worker(0),
                _ => platform::exit_worker(1),
            }
        }
    });

    let threads = slot.threads();
    let result = initialize(slot, threads);
    let ready = [1, threads];
    let failure = [result.as_ref().err().map_or(0, |error| error.wire())];
    output
        .write_all(if result.is_ok() {
            &ready[..]
        } else {
            &failure[..]
        })
        .and_then(|()| output.flush())
        .map_err(|_| WorkerError::Eof)?;
    let (_gate, mut engine, _model) = result?;
    for frame in requests {
        let (id, payload) = split_message(&frame)?;
        let result = parse_request(payload).and_then(|input| engine.embed(input));
        let mut response = Vec::with_capacity(REQUEST_ID_BYTES + MAX_RESPONSE_BYTES);
        response.extend_from_slice(id);
        match result {
            Ok(vector) => {
                response.push(0);
                response.extend(vector.into_iter().flat_map(f32::to_le_bytes));
            }
            Err(error) => response.push(error.wire()),
        }
        write_frame(&mut output, &response).map_err(|_| WorkerError::Eof)?;
    }
    Ok(())
}

fn initialize(
    slot: EngineSlot,
    threads: u8,
) -> Result<(platform::SlotGate, native::Engine, File), WorkerError> {
    platform::configure_worker(slot)?;
    let executable = std::env::current_exe().map_err(|_| WorkerError::Startup)?;
    let runtime = native::Api::open(runtime::verify(&executable)?)?;
    let control = ControlHandle::default();
    let model = crate::model::open_engine_model(&control)?;
    let cache = crate::settings::cache_home(&control);
    if !cache.is_absolute() {
        return Err(WorkerError::Startup);
    }
    let gate = acquire_slot(slot, SLOT_WAIT)?;
    let cache_dir = cache.join("kio/eg2").join(slot.name());
    let _cache = crate::private_state::ensure_private_directory(&cache_dir, &cache, &control)
        .map_err(|_| WorkerError::Startup)?;
    let (model_path, model_file) = model;
    let engine = native::Engine::open(runtime, &model_path, &cache_dir, i32::from(threads))?;
    Ok((gate, engine, model_file))
}
