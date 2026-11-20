//! Capture terminal interrupts only while the CLI is supervising native inference.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

static HANDLER: Mutex<()> = Mutex::new(());
static CANCELLED: AtomicBool = AtomicBool::new(false);

struct InterruptGuard {
    registration: Registration,
    _lock: MutexGuard<'static, ()>,
}

/// Install process handlers for one native call, restoring them on return.
/// The worker runs in its own process group and is stopped by the supervisor.
pub fn cancellation() -> kio_core::Result<Box<dyn Fn() -> bool>> {
    let lock = HANDLER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    CANCELLED.store(false, Ordering::Relaxed);
    let registration = Registration::install().map_err(|error| {
        kio_core::KioError::io(error.to_string(), "embedding interrupt handler")
    })?;
    let guard = InterruptGuard {
        registration,
        _lock: lock,
    };
    Ok(Box::new(move || guard.cancelled()))
}

impl InterruptGuard {
    fn cancelled(&self) -> bool {
        CANCELLED.load(Ordering::Relaxed)
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        self.registration.restore();
    }
}

#[cfg(unix)]
struct Registration {
    interrupt: libc::sigaction,
    terminate: libc::sigaction,
}

#[cfg(unix)]
impl Registration {
    fn install() -> std::io::Result<Self> {
        extern "C" fn notify(_: libc::c_int) {
            CANCELLED.store(true, Ordering::Relaxed);
        }

        // SAFETY: sigaction reads the initialized action and writes the previous
        // action into the provided storage. The handler only sets an atomic.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            let mut interrupt: libc::sigaction = std::mem::zeroed();
            let mut terminate: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = notify as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGINT, &action, &mut interrupt) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::sigaction(libc::SIGTERM, &action, &mut terminate) != 0 {
                let error = std::io::Error::last_os_error();
                libc::sigaction(libc::SIGINT, &interrupt, std::ptr::null_mut());
                return Err(error);
            }
            Ok(Self {
                interrupt,
                terminate,
            })
        }
    }

    fn restore(&self) {
        // SAFETY: both actions were returned by sigaction for these signals.
        unsafe {
            libc::sigaction(libc::SIGTERM, &self.terminate, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &self.interrupt, std::ptr::null_mut());
        }
    }
}

#[cfg(windows)]
struct Registration;

#[cfg(windows)]
unsafe extern "system" fn notify(kind: u32) -> i32 {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    if matches!(kind, CTRL_C_EVENT | CTRL_BREAK_EVENT) {
        CANCELLED.store(true, Ordering::Relaxed);
        1
    } else {
        0
    }
}

#[cfg(windows)]
impl Registration {
    fn install() -> std::io::Result<Self> {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        // SAFETY: the callback is static and only sets an atomic.
        if unsafe { SetConsoleCtrlHandler(Some(notify), 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self)
    }

    fn restore(&self) {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        // SAFETY: this removes the callback installed by this registration.
        unsafe { SetConsoleCtrlHandler(Some(notify), 0) };
    }
}

#[cfg(all(test, unix))]
#[test]
fn terminal_interrupts_set_only_the_current_call() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let cancelled = cancellation().unwrap();
        assert!(!cancelled());
        // SAFETY: this thread raises a signal with the handler installed above.
        assert_eq!(unsafe { libc::raise(signal) }, 0);
        assert!(cancelled());
    }
}
