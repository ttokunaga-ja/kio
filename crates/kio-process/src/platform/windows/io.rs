use crate::{BoundedProcessError, BoundedStdin, OutputPipe, StdinProgress, StreamEvent};
use std::{
    io::{Read, Write},
    process::Child,
    sync::{Arc, atomic::AtomicBool, mpsc},
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

use std::thread::{self, JoinHandle};

fn read_stream_inner<R: Read + Send + 'static>(
    mut reader: R,
    stream: &'static str,
    limit: usize,
    sender: mpsc::Sender<StreamEvent>,
    _cancelled: Arc<AtomicBool>,
) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
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
    writer: Option<JoinHandle<std::io::Result<()>>>,
}

impl StdinPump {
    pub(crate) fn new(child: &mut Child, input: Option<BoundedStdin>) -> Self {
        let writer = input.map(|input| {
            let mut handle = child.stdin.take().expect("stdin was configured as piped");
            thread::spawn(move || handle.write_all(&input.bytes))
        });
        Self { writer }
    }

    pub(crate) fn prepare(&mut self) -> Result<(), BoundedProcessError> {
        Ok(())
    }

    pub(crate) fn progress(
        &mut self,
        _deadline: Instant,
        _timeout: Duration,
    ) -> Result<StdinProgress, BoundedProcessError> {
        Ok(if self.is_complete() {
            StdinProgress::Complete
        } else {
            StdinProgress::Pending
        })
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.writer
            .as_ref()
            .is_none_or(|writer| writer.is_finished())
    }

    pub(crate) fn wait_pending(
        &mut self,
        _deadline: Instant,
        _now: Instant,
    ) -> Result<bool, BoundedProcessError> {
        Ok(false)
    }

    pub(crate) fn finish(&mut self) -> Result<(), BoundedProcessError> {
        self.writer
            .take()
            .map(|writer| {
                writer
                    .join()
                    .map_err(|_| {
                        BoundedProcessError::Write(std::io::Error::other("stdin writer panicked"))
                    })?
                    .map_err(BoundedProcessError::Write)
            })
            .unwrap_or(Ok(()))
    }
}
