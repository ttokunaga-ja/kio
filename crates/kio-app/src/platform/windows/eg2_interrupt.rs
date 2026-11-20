use crate::eg2::interrupt::CANCELLED;
use std::sync::atomic::Ordering;

pub(crate) struct Registration;

unsafe extern "system" fn notify(kind: u32) -> i32 {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    if matches!(kind, CTRL_C_EVENT | CTRL_BREAK_EVENT) {
        CANCELLED.store(true, Ordering::Relaxed);
        1
    } else {
        0
    }
}

impl Registration {
    pub(crate) fn install() -> std::io::Result<Self> {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        // SAFETY: the callback is static and only sets an atomic.
        if unsafe { SetConsoleCtrlHandler(Some(notify), 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self)
    }

    pub(crate) fn restore(&self) {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        // SAFETY: this removes the callback installed by this registration.
        unsafe { SetConsoleCtrlHandler(Some(notify), 0) };
    }
}
