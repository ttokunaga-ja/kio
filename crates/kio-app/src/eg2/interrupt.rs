//! Capture terminal interrupts only while the CLI is supervising native inference.

use crate::platform::Eg2InterruptRegistration as Registration;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

static HANDLER: Mutex<()> = Mutex::new(());
pub(crate) static CANCELLED: AtomicBool = AtomicBool::new(false);

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
