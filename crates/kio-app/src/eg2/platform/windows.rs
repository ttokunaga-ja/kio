use crate::eg2::{EngineSlot, MEMORY_BYTES, WorkerError};
use std::fs::File;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode,
};
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, CREATE_NEW_PROCESS_GROUP, GetCurrentProcess, SetPriorityClass,
    TerminateProcess,
};

pub(in crate::eg2) fn configure_command(command: &mut Command) {
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

pub(in crate::eg2) fn private_stdout() -> Result<File, WorkerError> {
    // SAFETY: duplicate the pipe before replacing stdout with the worker's
    // existing NUL stderr handle. The duplicate is owned by the returned File.
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
        let mut output = null_mut();
        if DuplicateHandle(
            process,
            stdout,
            process,
            &mut output,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        ) == 0
        {
            return Err(WorkerError::Startup);
        }
        let output = File::from_raw_handle(output);
        if SetStdHandle(STD_OUTPUT_HANDLE, stderr) == 0 {
            return Err(WorkerError::Startup);
        }
        // SAFETY: the standard handle was replaced above and is no longer used.
        windows_sys::Win32::Foundation::CloseHandle(stdout);
        Ok(output)
    }
}

pub(in crate::eg2) struct ProcessLimits(OwnedHandle);

impl ProcessLimits {
    pub(in crate::eg2) fn attach(child: &Child) -> Result<Self, WorkerError> {
        // SAFETY: the job handle is transferred to OwnedHandle and its limit
        // structure is passed with the matching information class and size.
        unsafe {
            let job = CreateJobObjectW(null(), null());
            if job.is_null() {
                return Err(WorkerError::Startup);
            }
            let job = OwnedHandle::from_raw_handle(job);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            limits.ProcessMemoryLimit = MEMORY_BYTES as usize;
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
            Ok(Self(job))
        }
    }

    pub(in crate::eg2) fn check(&self, _: &Child) -> Result<(), WorkerError> {
        // SAFETY: the job is live, and the info class matches the output struct.
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        if unsafe {
            QueryInformationJobObject(
                self.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(WorkerError::Startup);
        }
        if info.PeakProcessMemoryUsed as u64 >= MEMORY_BYTES {
            return Err(WorkerError::MemoryLimit);
        }
        Ok(())
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
