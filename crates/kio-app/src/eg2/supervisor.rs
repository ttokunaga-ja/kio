use super::*;
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};

pub(super) struct Worker {
    pub(super) child: Option<Child>,
    pub(super) threads: u8,
    requests: Option<SyncSender<Vec<u8>>>,
    responses: Receiver<Result<Vec<u8>, WorkerError>>,
    io: Option<JoinHandle<()>>,
    limits: Option<platform::ProcessLimits>,
    stopping: Arc<AtomicBool>,
    last_id: u64,
}

impl Worker {
    pub(super) fn spawn(
        command: Command,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
        test_preamble: bool,
    ) -> Result<Self, WorkerError> {
        Self::spawn_limited(command, timeout, cancelled, test_preamble, MEMORY_BYTES)
    }

    pub(super) fn spawn_limited(
        mut command: Command,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
        test_preamble: bool,
        memory_bytes: u64,
    ) -> Result<Self, WorkerError> {
        if cancelled() {
            return Err(WorkerError::Cancelled);
        }
        platform::configure_command(&mut command, memory_bytes);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| WorkerError::Startup)?;
        let limits = match platform::ProcessLimits::attach(&child, memory_bytes) {
            Ok(limits) => limits,
            Err(error) => {
                platform::terminate_group(&child);
                let _ = child.kill();
                reap(child, Instant::now() + STOP_TIMEOUT);
                return Err(error);
            }
        };
        let stdin = child.stdin.take().expect("piped worker stdin");
        let stdout = child.stdout.take().expect("piped worker stdout");
        let stopping = Arc::new(AtomicBool::new(false));
        let (send, receive) = mpsc::sync_channel::<Vec<u8>>(1);
        let (reply, responses) = mpsc::sync_channel(1);
        let mut worker = Self {
            child: Some(child),
            threads: 0,
            requests: Some(send),
            responses,
            io: None,
            limits: Some(limits),
            stopping: Arc::clone(&stopping),
            last_id: 0,
        };
        platform::configure_pipes(&stdin, &stdout).map_err(|_| WorkerError::Startup)?;
        let mut stdin = Interruptible {
            pipe: stdin,
            stopping: Arc::clone(&stopping),
        };
        let mut stdout = Interruptible {
            pipe: stdout,
            stopping,
        };
        worker.io = Some(
            thread::Builder::new()
                .spawn(move || {
                    let ready = (|| {
                        stdin.write_all(&[1]).map_err(|_| WorkerError::Eof)?;
                        if cfg!(test) && test_preamble {
                            let mut preamble = [0; b"\nrunning 1 test\n".len()];
                            stdout
                                .read_exact(&mut preamble)
                                .map_err(|_| WorkerError::Eof)?;
                            if &preamble != b"\nrunning 1 test\n" {
                                return Err(WorkerError::Protocol);
                            }
                        }
                        let mut status = [0];
                        stdout
                            .read_exact(&mut status)
                            .map_err(|_| WorkerError::Eof)?;
                        if status == [1] {
                            let mut threads = [0];
                            stdout
                                .read_exact(&mut threads)
                                .map_err(|_| WorkerError::Eof)?;
                            Ok(vec![1, threads[0]])
                        } else {
                            Ok(status.to_vec())
                        }
                    })();
                    let initialized =
                        matches!(&ready, Ok(bytes) if matches!(bytes.as_slice(), [1, 1..=4]));
                    if reply.send(ready).is_err() || !initialized {
                        return;
                    }
                    for frame in receive {
                        let response = write_frame(&mut stdin, &frame)
                            .map_err(|_| WorkerError::Eof)
                            .and_then(|()| {
                                read_frame(&mut stdout, REQUEST_ID_BYTES + MAX_RESPONSE_BYTES)
                            })
                            .and_then(|frame| frame.ok_or(WorkerError::Eof));
                        let healthy = response.is_ok();
                        if reply.send(response).is_err() || !healthy {
                            return;
                        }
                    }
                })
                .map_err(|_| WorkerError::Startup)?,
        );
        let ready = worker.wait_response(Instant::now() + timeout, cancelled)?;
        match ready.as_slice() {
            [1, threads @ 1..=4] => {
                worker.threads = *threads;
                Ok(worker)
            }
            [status] => Err(WorkerError::from_wire(*status).unwrap_or(WorkerError::Protocol)),
            _ => Err(WorkerError::Protocol),
        }
    }

    pub(super) fn embed(
        &mut self,
        frame: &[u8],
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<f32>, WorkerError> {
        if frame.is_empty() || frame.len() > MAX_REQUEST_BYTES {
            return Err(WorkerError::InputLimit);
        }
        let result = self.exchange(frame, deadline, cancelled);
        if let Err(error) = result
            && error != WorkerError::InputOverflow
        {
            self.stop();
        }
        result
    }

    fn exchange(
        &mut self,
        frame: &[u8],
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<f32>, WorkerError> {
        if cancelled() {
            return Err(WorkerError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(WorkerError::Timeout);
        }
        self.last_id = self.last_id.checked_add(1).ok_or(WorkerError::Protocol)?;
        let id = self.last_id.to_le_bytes();
        let mut message = Vec::with_capacity(REQUEST_ID_BYTES + frame.len());
        message.extend_from_slice(&id);
        message.extend_from_slice(frame);
        self.requests
            .as_ref()
            .ok_or(WorkerError::Eof)?
            .try_send(message)
            .map_err(|_| WorkerError::Eof)?;
        let response = self.wait_response(deadline, cancelled)?;
        let (reply_id, payload) = split_message(&response)?;
        if reply_id != &id {
            return Err(WorkerError::Protocol);
        }
        match payload {
            [0, bytes @ ..] if bytes.len() == DIMENSIONS * 4 => {
                let vector: Vec<_> = bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .copied()
                    .map(f32::from_le_bytes)
                    .collect();
                if valid_vector(&vector) {
                    Ok(vector)
                } else {
                    Err(WorkerError::Protocol)
                }
            }
            [status] => Err(WorkerError::from_wire(*status).unwrap_or(WorkerError::Protocol)),
            _ => Err(WorkerError::Protocol),
        }
    }

    fn wait_response(
        &mut self,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<u8>, WorkerError> {
        loop {
            if cancelled() {
                return Err(WorkerError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(WorkerError::Timeout);
            }
            if let Some(limits) = &self.limits {
                limits.check()?;
            }
            match self.responses.recv_timeout(POLL.min(remaining)) {
                Ok(Err(WorkerError::Eof)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.eof_reason(deadline));
                }
                Ok(response) => {
                    if cancelled() {
                        return Err(WorkerError::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        return Err(WorkerError::Timeout);
                    }
                    if let Some(limits) = &self.limits {
                        limits.check()?;
                    }
                    return response;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn eof_reason(&mut self, deadline: Instant) -> WorkerError {
        let Some(child) = self.child.as_mut() else {
            return WorkerError::Eof;
        };
        // The pipe can close immediately before the exit status is visible.
        let deadline = deadline.min(Instant::now() + Duration::from_millis(50));
        loop {
            match platform::peek_exit(child) {
                Ok(Some(status)) if platform::memory_exit(status) => {
                    return WorkerError::MemoryLimit;
                }
                Ok(Some(status)) if !status.success() => return WorkerError::Crash,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(2)),
                _ => return WorkerError::Eof,
            }
        }
    }

    fn stop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.requests.take();
        // Close the Windows job before waiting on its members or their pipes.
        self.limits.take();
        let deadline = Instant::now() + STOP_TIMEOUT;
        if let Some(mut child) = self.child.take() {
            platform::terminate_group(&child);
            let _ = child.kill();
            reap(child, deadline);
        }
        if let Some(io) = self.io.take() {
            while !io.is_finished() && Instant::now() < deadline {
                platform::interrupt_io(&io);
                thread::sleep(IO_POLL);
            }
            if io.is_finished() {
                let _ = io.join();
            }
        }
    }
}

const STOP_TIMEOUT: Duration = Duration::from_millis(500);
const IO_POLL: Duration = Duration::from_millis(2);

fn reap(mut child: Child, deadline: Instant) {
    while let Ok(None) = child.try_wait() {
        if Instant::now() >= deadline {
            // Even a kernel-delayed exit must not extend the command deadline.
            let _ = thread::Builder::new().spawn(move || child.wait());
            return;
        }
        thread::sleep(IO_POLL);
    }
}

struct Interruptible<T> {
    pipe: T,
    stopping: Arc<AtomicBool>,
}

impl<T> Interruptible<T> {
    fn io<R>(&mut self, mut operation: impl FnMut(&mut T) -> io::Result<R>) -> io::Result<R> {
        loop {
            if self.stopping.load(Ordering::Acquire) {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            match operation(&mut self.pipe) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(IO_POLL),
                result => return result,
            }
        }
    }
}

impl<T: Read> Read for Interruptible<T> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.io(|pipe| pipe.read(buffer))
    }
}

impl<T: Write> Write for Interruptible<T> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.io(|pipe| pipe.write(buffer))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io(Write::flush)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}
