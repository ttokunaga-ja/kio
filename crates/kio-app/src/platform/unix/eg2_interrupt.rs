use crate::eg2::interrupt::CANCELLED;
use std::sync::atomic::Ordering;

pub(crate) struct Registration {
    interrupt: libc::sigaction,
    terminate: libc::sigaction,
}

impl Registration {
    pub(crate) fn install() -> std::io::Result<Self> {
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

    pub(crate) fn restore(&self) {
        // SAFETY: both actions were returned by sigaction for these signals.
        unsafe {
            libc::sigaction(libc::SIGTERM, &self.terminate, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &self.interrupt, std::ptr::null_mut());
        }
    }
}
