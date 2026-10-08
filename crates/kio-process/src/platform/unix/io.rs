use crate::{BoundedProcessError, BoundedStdin, OutputPipe, StdinProgress, StreamEvent};
use std::{
    io::{Read, Write},
    process::{Child, ChildStdin},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub(crate) fn read_stream(
    reader: OutputPipe,
    stream: &'static str,
    limit: usize,
    sender: mpsc::Sender<StreamEvent>,
    cancelled: Arc<AtomicBool>,
) {
    match reader {
        OutputPipe::Stdout(reader) => read_stream_inner(reader, stream, limit, sender, cancelled),
        OutputPipe::Stderr(reader) => read_stream_inner(reader, stream, limit, sender, cancelled),
    }
}

fn read_stream_inner<R: Read + std::os::fd::AsRawFd + Send + 'static>(
    mut reader: R,
    stream: &'static str,
    limit: usize,
    sender: mpsc::Sender<StreamEvent>,
    cancelled: Arc<AtomicBool>,
) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let mut descriptor = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Poll rather than blocking in `read` so cancellation can also release
        // readers whose pipe write end escaped the bounded process group.
        let polled = unsafe { libc::poll(&mut descriptor, 1, 10) };
        if polled == 0 {
            continue;
        }
        if polled < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            let _ = sender.send(StreamEvent::ReadError(stream, error));
            return;
        }
        match reader.read(&mut chunk) {
            Ok(0) => {
                let _ = sender.send(StreamEvent::Data(stream, bytes));
                let _ = sender.send(StreamEvent::End);
                return;
            }
            Ok(count) => {
                let Some(total) = bytes.len().checked_add(count) else {
                    let _ = sender.send(StreamEvent::OutputLimit(stream, limit));
                    return;
                };
                if total > limit {
                    let _ = sender.send(StreamEvent::OutputLimit(stream, limit));
                    return;
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) => {
                let _ = sender.send(StreamEvent::ReadError(stream, error));
                return;
            }
        }
    }
}

pub(crate) struct StdinPump {
    input: Option<BoundedStdin>,
    handle: Option<ChildStdin>,
    offset: usize,
    wait_pending: bool,
}

impl StdinPump {
    pub(crate) fn new(child: &mut Child, input: Option<BoundedStdin>) -> Self {
        Self {
            input,
            handle: child.stdin.take(),
            offset: 0,
            wait_pending: false,
        }
    }

    pub(crate) fn prepare(&mut self) -> Result<(), BoundedProcessError> {
        if let Some(pipe) = self.handle.as_ref() {
            use std::os::fd::AsRawFd;
            let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
            if flags < 0
                || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                    < 0
            {
                let error = std::io::Error::last_os_error();
                return Err(BoundedProcessError::Write(error));
            }
        }
        Ok(())
    }

    pub(crate) fn progress(
        &mut self,
        deadline: Instant,
        timeout: Duration,
    ) -> Result<StdinProgress, BoundedProcessError> {
        self.wait_pending = false;
        let mut stdin_burst = 0_usize;
        if let (Some(input), Some(handle)) = (self.input.as_ref(), self.handle.as_mut()) {
            while self.offset < input.bytes.len() {
                if Instant::now() >= deadline {
                    return Err(BoundedProcessError::Timeout {
                        timeout_ms: timeout.as_millis(),
                    });
                }
                use std::os::fd::AsRawFd;
                let mut descriptor = libc::pollfd {
                    fd: handle.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let polled = unsafe { libc::poll(&mut descriptor, 1, 0) };
                if polled < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        return Err(BoundedProcessError::Write(error));
                    }
                    continue;
                }
                if polled == 0 {
                    self.wait_pending = true;
                    break;
                }
                if descriptor.revents
                    & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)
                    != 0
                {
                    match handle.write(&input.bytes[self.offset..]) {
                        Ok(0) => {
                            return Err(BoundedProcessError::Write(std::io::Error::new(
                                std::io::ErrorKind::WriteZero,
                                "bounded subprocess stdin accepted no bytes",
                            )));
                        }
                        Ok(count) => {
                            self.offset += count;
                            stdin_burst += count;
                            // A continuously writable pipe must not starve
                            // output-limit/read events. Yield after a bounded
                            // burst, but use readiness polling below so this
                            // is not the former fixed-delay write throttle.
                            if stdin_burst >= 1024 * 1024 {
                                self.wait_pending = true;
                                break;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            self.wait_pending = true;
                            break;
                        }
                        Err(error) => return Err(BoundedProcessError::Write(error)),
                    }
                } else {
                    self.wait_pending = true;
                    break;
                }
            }
            if self.offset == input.bytes.len() {
                // EOF is part of the protocol, but never wait on it outside
                // the shared deadline.
                self.wait_pending = false;
                self.handle.take();
            }
        }
        Ok(if self.is_complete() {
            StdinProgress::Complete
        } else {
            StdinProgress::Pending
        })
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.input
            .as_ref()
            .is_none_or(|input| self.offset == input.bytes.len())
    }

    pub(crate) fn wait_pending(
        &mut self,
        deadline: Instant,
        now: Instant,
    ) -> Result<bool, BoundedProcessError> {
        if !self.wait_pending {
            return Ok(false);
        }
        use std::os::fd::AsRawFd;
        let mut descriptor = libc::pollfd {
            fd: self
                .handle
                .as_ref()
                .expect("stdin remains open while a write is pending")
                .as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // Waiting on the writable pipe prevents a fixed 10 ms delay per
        // write phase. Output events are still consumed below when already
        // available; otherwise the next status/output poll remains bounded
        // to the same short interval and shared deadline.
        let wait = (deadline - now).min(Duration::from_millis(10));
        let wait_ms = wait
            .as_millis()
            .clamp(1, i32::MAX as u128)
            .try_into()
            .expect("clamped poll timeout fits i32");
        let polled = unsafe { libc::poll(&mut descriptor, 1, wait_ms) };
        if polled < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(BoundedProcessError::Write(error));
            }
        }
        Ok(true)
    }

    pub(crate) fn finish(&mut self) -> Result<(), BoundedProcessError> {
        Ok(())
    }
}
