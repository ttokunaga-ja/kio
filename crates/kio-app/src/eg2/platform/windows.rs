use crate::eg2::{EngineSlot, WorkerError};
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus};
use std::ptr::{null, null_mut};
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, INVALID_HANDLE_VALUE, LocalFree, WAIT_ABANDONED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
    SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode,
};
use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, CREATE_NEW_PROCESS_GROUP, CreateMutexExW, GetCurrentProcess,
    MUTEX_MODIFY_STATE, ReleaseMutex, SetPriorityClass, TerminateProcess, WaitForSingleObject,
};

pub(in crate::eg2) fn configure_command(command: &mut Command, _: u64) {
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

pub(in crate::eg2) fn configure_worker(slot: EngineSlot) -> Result<(), WorkerError> {
    // SAFETY: the pseudo-handle refers to this worker.
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
        if slot == EngineSlot::Index
            && SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS) == 0
        {
            return Err(WorkerError::Startup);
        }
    }
    Ok(())
}

pub(in crate::eg2) fn private_output(
    capture_diagnostics: bool,
) -> Result<(File, Option<File>), WorkerError> {
    let null_output = File::options()
        .write(true)
        .open("NUL")
        .map_err(|_| WorkerError::Startup)?;
    // SAFETY: duplicate the protocol pipe and optional test diagnostic sink
    // before replacing both standard handles.
    unsafe {
        let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
        let stderr = GetStdHandle(STD_ERROR_HANDLE);
        if stdout.is_null()
            || stdout == INVALID_HANDLE_VALUE
            || stderr.is_null()
            || stderr == INVALID_HANDLE_VALUE
        {
            return Err(WorkerError::Startup);
        }
        let process = GetCurrentProcess();
        let duplicate = |source| {
            let mut handle = null_mut();
            if DuplicateHandle(
                process,
                source,
                process,
                &mut handle,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            ) == 0
            {
                return Err(WorkerError::Startup);
            }
            Ok(File::from_raw_handle(handle))
        };
        let output = duplicate(stdout)?;
        let diagnostics = capture_diagnostics.then(|| duplicate(stderr)).transpose()?;
        if SetStdHandle(STD_OUTPUT_HANDLE, null_output.as_raw_handle()) == 0 {
            return Err(WorkerError::Startup);
        }
        // Standard handles remain live until the worker exits.
        let null_handle = null_output.into_raw_handle();
        if SetStdHandle(STD_ERROR_HANDLE, null_handle) == 0 {
            return Err(WorkerError::Startup);
        }
        windows_sys::Win32::Foundation::CloseHandle(stdout);
        if stderr != stdout {
            windows_sys::Win32::Foundation::CloseHandle(stderr);
        }
        Ok((output, diagnostics))
    }
}

pub(in crate::eg2) struct ProcessLimits {
    job: OwnedHandle,
    bytes: u64,
}

impl ProcessLimits {
    pub(in crate::eg2) fn attach(child: &Child, memory_bytes: u64) -> Result<Self, WorkerError> {
        // SAFETY: the job handle is transferred to OwnedHandle and its limit
        // structure is passed with the matching information class and size.
        unsafe {
            let job = CreateJobObjectW(null(), null());
            if job.is_null() {
                return Err(WorkerError::Startup);
            }
            let job = OwnedHandle::from_raw_handle(job);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_PROCESS_MEMORY
                | JOB_OBJECT_LIMIT_JOB_MEMORY;
            limits.ProcessMemoryLimit = memory_bytes as usize;
            limits.JobMemoryLimit = memory_bytes as usize;
            if SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
                || AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) == 0
            {
                return Err(WorkerError::Startup);
            }
            Ok(Self {
                job,
                bytes: memory_bytes,
            })
        }
    }

    pub(in crate::eg2) fn check(&self) -> Result<(), WorkerError> {
        // SAFETY: the job is live, and the info class matches the output struct.
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        if unsafe {
            QueryInformationJobObject(
                self.job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(WorkerError::Startup);
        }
        if info.PeakJobMemoryUsed as u64 >= self.bytes {
            return Err(WorkerError::MemoryLimit);
        }
        Ok(())
    }
}

pub(in crate::eg2) fn peek_exit(child: &mut Child) -> io::Result<Option<ExitStatus>> {
    child.try_wait()
}

pub(in crate::eg2) fn terminate_group(_: &Child) {
    // Dropping ProcessLimits closes the job and terminates its descendants.
}

pub(in crate::eg2) fn configure_pipes(_: &ChildStdin, _: &ChildStdout) -> io::Result<()> {
    Ok(())
}

pub(in crate::eg2) fn interrupt_io(thread: &JoinHandle<()>) {
    // Cancellation can race with entry into a syscall. The supervisor polls
    // until the thread exits, and the pipe wrapper checks its stop flag.
    unsafe { CancelSynchronousIo(thread.as_raw_handle()) };
}

pub(in crate::eg2) struct SlotGate {
    mutex: OwnedHandle,
    acquired: bool,
}

impl SlotGate {
    pub(in crate::eg2) fn open(slot: EngineSlot) -> Result<Self, WorkerError> {
        let name: Vec<_> = format!("Global\\Kio-EG2-Engine-{}", slot.name())
            .encode_utf16()
            .chain([0])
            .collect();
        // Give every local caller only SYNCHRONIZE and MUTEX_MODIFY_STATE.
        // Global\ names are shared across interactive/service sessions.
        let sddl: Vec<_> = "D:(A;;0x00100001;;;WD)".encode_utf16().chain([0]).collect();
        let mut descriptor = null_mut();
        // SAFETY: the returned descriptor is owned by LocalAlloc and can be
        // freed after CreateMutexExW consumes the attributes.
        let mutex = unsafe {
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            ) == 0
            {
                return Err(WorkerError::Startup);
            }
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            let handle = CreateMutexExW(
                &attributes,
                name.as_ptr(),
                0,
                SYNCHRONIZE | MUTEX_MODIFY_STATE,
            );
            LocalFree(descriptor);
            if handle.is_null() {
                return Err(WorkerError::Startup);
            }
            OwnedHandle::from_raw_handle(handle)
        };
        Ok(Self {
            mutex,
            acquired: false,
        })
    }

    pub(in crate::eg2) fn try_acquire(&mut self) -> Result<bool, WorkerError> {
        match unsafe { WaitForSingleObject(self.mutex.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => {
                self.acquired = true;
                Ok(true)
            }
            WAIT_TIMEOUT => Ok(false),
            _ => Err(WorkerError::Startup),
        }
    }
}

impl Drop for SlotGate {
    fn drop(&mut self) {
        if self.acquired {
            unsafe { ReleaseMutex(self.mutex.as_raw_handle()) };
        }
    }
}

pub(in crate::eg2) fn memory_exit(status: ExitStatus) -> bool {
    // STATUS_NO_MEMORY and STATUS_COMMITMENT_LIMIT.
    matches!(
        status.code().map(|code| code as u32),
        Some(0xc0000017 | 0xc000012d)
    )
}

pub(in crate::eg2) fn exit_worker(code: i32) -> ! {
    // SAFETY: the pseudo-handle points to this worker; bypass DLL shutdown.
    unsafe { TerminateProcess(GetCurrentProcess(), code as u32) };
    std::process::abort()
}
