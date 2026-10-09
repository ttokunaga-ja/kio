//! Windows notification subscription, cancellation and worker lifetime.

use super::watch_native_model::{Channel, modified_kind, parse_records};
use crate::watch::{Reconcile, WatchEngine, WatchError};
use notify::EventKind;
use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use std::os::windows::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::*;
use windows_sys::Win32::System::Threading::*;

struct Handle(HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn last_error() -> WatchError {
    WatchError::invariant(format!("Windows native watcher error {}", unsafe {
        GetLastError()
    }))
}
fn event() -> Result<Handle, WatchError> {
    let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if raw.is_null() {
        Err(last_error())
    } else {
        Ok(Handle(raw))
    }
}
fn directory(path: &Path) -> Result<Handle, WatchError> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    let handle = Handle(raw);
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle.0, &mut info) } == 0 {
        return Err(last_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(WatchError::invariant(
            "Windows watcher root must be an ordinary directory",
        ));
    }
    Ok(handle)
}
struct Stop {
    event: Handle,
    callback: Mutex<()>,
}
fn stopped(stop: &Stop) -> bool {
    unsafe { WaitForSingleObject(stop.event.0, 0) == WAIT_OBJECT_0 }
}
// Cancellation is not completion: keep the buffer and OVERLAPPED alive
// until GetOverlappedResult has observed the terminal operation result.
fn finish_pending(directory: &Handle, overlapped: &mut OVERLAPPED) {
    unsafe {
        CancelIoEx(directory.0, overlapped);
        let mut ignored = 0;
        GetOverlappedResult(directory.0, overlapped, &mut ignored, 1);
    }
}
pub(crate) struct NativeWatchBackend {
    stop: Arc<Stop>,
    workers: Vec<JoinHandle<()>>,
}
impl Drop for NativeWatchBackend {
    fn drop(&mut self) {
        {
            let _gate = self.stop.callback.lock().unwrap_or_else(|p| p.into_inner());
            unsafe {
                SetEvent(self.stop.event.0);
            }
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
impl NativeWatchBackend {
    fn start_subscriptions<R: Reconcile>(engine: Arc<WatchEngine<R>>) -> Result<Self, WatchError> {
        let mut backend = Self {
            stop: Arc::new(Stop {
                event: event()?,
                callback: Mutex::new(()),
            }),
            workers: Vec::new(),
        };
        for root in engine.roots() {
            for channel in [Channel::Data, Channel::Authority] {
                let directory = directory(&root.canonical_root)?;
                let completion = event()?;
                let stop = Arc::clone(&backend.stop);
                let engine = Arc::clone(&engine);
                let root = root.canonical_root.clone();
                // Establish the subscription before returning start, so
                // immediate mutations cannot fall in a startup gap.
                let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
                let worker = thread::Builder::new()
                    .name("kio-windows-watch".into())
                    .spawn(move || {
                        run(engine, root, channel, directory, completion, stop, ready_tx);
                    })
                    .map_err(|_| WatchError::invariant("cannot create Windows watcher worker"))?;
                backend.workers.push(worker);
                ready_rx.recv().map_err(|_| {
                    WatchError::invariant("Windows watcher worker startup failed")
                })??;
            }
        }
        Ok(backend)
    }
}
fn dispatch<R: Reconcile>(
    engine: &WatchEngine<R>,
    stop: &Stop,
    event: notify::Result<notify::Event>,
) {
    let _gate = stop.callback.lock().unwrap_or_else(|p| p.into_inner());
    if !stopped(stop) {
        crate::watch::native::handle_event(engine, event);
    }
}

fn run<R: Reconcile>(
    engine: Arc<WatchEngine<R>>,
    root: PathBuf,
    channel: Channel,
    directory: Handle,
    completion: Handle,
    stop: Arc<Stop>,
    ready: std::sync::mpsc::SyncSender<Result<(), WatchError>>,
) {
    let mask = match channel {
        Channel::Data => {
            FILE_NOTIFY_CHANGE_FILE_NAME
                | FILE_NOTIFY_CHANGE_DIR_NAME
                | FILE_NOTIFY_CHANGE_SIZE
                | FILE_NOTIFY_CHANGE_LAST_WRITE
                | FILE_NOTIFY_CHANGE_CREATION
        }
        Channel::Authority => FILE_NOTIFY_CHANGE_ATTRIBUTES | FILE_NOTIFY_CHANGE_SECURITY,
    };
    let mut ready = Some(ready);
    // u64 storage ensures at least eight-byte alignment; never resized.
    let mut buffer = Box::new([0u64; 2048]);
    let mut overlapped: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
    overlapped.hEvent = completion.0;
    loop {
        if stopped(&stop) {
            return;
        }
        unsafe {
            ResetEvent(completion.0);
        }
        let issued = unsafe {
            ReadDirectoryChangesW(
                directory.0,
                buffer.as_mut_ptr().cast(),
                16384,
                1,
                mask,
                std::ptr::null_mut(),
                &mut *overlapped,
                None,
            )
        };
        if issued == 0 {
            let error = last_error();
            if let Some(ready) = ready.take() {
                let _ = ready.send(Err(error));
            }
            dispatch(
                &engine,
                &stop,
                Err(notify::Error::generic("Windows native subscription failed")),
            );
            return;
        }
        if let Some(ready) = ready.take() {
            let _ = ready.send(Ok(()));
        }
        let waits = [stop.event.0, completion.0];
        let wait = unsafe { WaitForMultipleObjects(2, waits.as_ptr(), 0, INFINITE) };
        if wait != WAIT_OBJECT_0 + 1 || stopped(&stop) {
            finish_pending(&directory, &mut overlapped);
            if !stopped(&stop) {
                dispatch(
                    &engine,
                    &stop,
                    Err(notify::Error::generic("Windows native wait failed")),
                );
            }
            return;
        }
        let mut count = 0;
        let completed = unsafe { GetOverlappedResult(directory.0, &*overlapped, &mut count, 0) };
        if completed == 0 {
            let code = unsafe { GetLastError() };
            finish_pending(&directory, &mut overlapped);
            if stopped(&stop) {
                return;
            }
            if code == ERROR_NOTIFY_ENUM_DIR {
                dispatch(
                    &engine,
                    &stop,
                    Ok(notify::Event::new(EventKind::Any).set_flag(notify::event::Flag::Rescan)),
                );
                continue;
            }
            dispatch(
                &engine,
                &stop,
                Err(notify::Error::generic("Windows native completion failed")),
            );
            return;
        }
        if stopped(&stop) {
            return;
        }
        let bytes = unsafe {
            std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), count.min(16384) as usize)
        };
        let records = if count > 16384 {
            Err(())
        } else {
            parse_records(bytes)
        };
        let Ok(records) = records else {
            dispatch(
                &engine,
                &stop,
                Ok(notify::Event::new(EventKind::Any).set_flag(notify::event::Flag::Rescan)),
            );
            continue;
        };
        for (action, relative) in records {
            if stopped(&stop) {
                return;
            }
            let path = root.join(&relative);
            let kind = match action {
                FILE_ACTION_ADDED => Some(EventKind::Create(CreateKind::Any)),
                FILE_ACTION_REMOVED => Some(EventKind::Remove(RemoveKind::Any)),
                FILE_ACTION_RENAMED_OLD_NAME => {
                    Some(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
                }
                FILE_ACTION_RENAMED_NEW_NAME => {
                    Some(EventKind::Modify(ModifyKind::Name(RenameMode::To)))
                }
                FILE_ACTION_MODIFIED => {
                    let ordinary = channel == Channel::Data
                        && std::fs::symlink_metadata(&path).is_ok_and(|m| {
                            m.is_dir() && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
                        });
                    modified_kind(
                        channel,
                        ordinary,
                        crate::watch::is_control_path(Path::new(&relative)),
                    )
                }
                _ => unreachable!("parser validates actions"),
            };
            if let Some(kind) = kind {
                dispatch(&engine, &stop, Ok(notify::Event::new(kind).add_path(path)));
            }
        }
    }
}

pub(crate) fn watch_backend_name() -> &'static str {
    "windows/read-directory-changes"
}

impl NativeWatchBackend {
    pub(crate) fn start<R: Reconcile>(engine: Arc<WatchEngine<R>>) -> Result<Self, WatchError> {
        engine.set_backend(watch_backend_name(), false, None);
        Self::start_subscriptions(Arc::clone(&engine)).inspect_err(|error| {
            engine.set_backend(watch_backend_name(), true, Some(error.to_string()));
            crate::watch::native::enqueue_all(&engine, crate::watch::DirtyReason::BackendError);
        })
    }
}
