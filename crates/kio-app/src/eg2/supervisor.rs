use super::*;
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::{self, JoinHandle};

pub(super) struct Worker {
    pub(super) child: Option<Child>,
    pub(super) threads: u8,
    requests: Option<SyncSender<Vec<u8>>>,
    responses: Receiver<Result<Vec<u8>, WorkerError>>,
    io: Option<JoinHandle<()>>,
    pub(super) limits: platform::ProcessLimits,
}

impl Worker {
    pub(super) fn spawn(
        mut command: Command,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
        test_preamble: bool,
    ) -> Result<Self, WorkerError> {
        if cancelled() {
            return Err(WorkerError::Cancelled);
        }
        platform::configure_command(&mut command);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| WorkerError::Startup)?;
        let limits = match platform::ProcessLimits::attach(&child) {
            Ok(limits) => limits,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let mut stdin = child.stdin.take().expect("piped worker stdin");
        let mut stdout = child.stdout.take().expect("piped worker stdout");
        let (send, receive) = mpsc::sync_channel::<Vec<u8>>(1);
        let (reply, responses) = mpsc::sync_channel(1);
        let mut worker = Self {
            child: Some(child),
            threads: 0,
            requests: Some(send),
            responses,
            io: None,
            limits,
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
                            .and_then(|()| read_frame(&mut stdout, MAX_RESPONSE_BYTES))
                            .and_then(|frame| frame.ok_or(WorkerError::Eof));
                        let healthy = response.is_ok();
                        if reply.send(response).is_err() || !healthy {
                            return;
                        }
                    }
                })
                .map_err(|_| WorkerError::Startup)?,
        );
        let ready = worker.wait_response(timeout, cancelled)?;
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
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<f32>, WorkerError> {
        if frame.is_empty() || frame.len() > MAX_REQUEST_BYTES {
            return Err(WorkerError::InputLimit);
        }
        let result = self.exchange(frame, timeout, cancelled);
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
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<f32>, WorkerError> {
        self.requests
            .as_ref()
            .ok_or(WorkerError::Eof)?
            .try_send(frame.to_vec())
            .map_err(|_| WorkerError::Eof)?;
        let response = self.wait_response(timeout, cancelled)?;
        match response.as_slice() {
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
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<u8>, WorkerError> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancelled() {
                return Err(WorkerError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(WorkerError::Timeout);
            }
            if let Some(child) = self.child.as_ref() {
                self.limits.check(child)?;
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
                    if let Some(child) = self.child.as_ref() {
                        self.limits.check(child)?;
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
            match child.try_wait() {
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
        self.requests.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(io) = self.io.take() {
            let _ = io.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}
