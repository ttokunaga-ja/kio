use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Instant,
};

struct Slot(&'static AtomicBool);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Wait on a synchronous trust provider without extending the probe deadline.
/// A timed-out call retains its slot until the provider has actually returned.
pub(crate) fn verify_until(
    busy: &'static AtomicBool,
    deadline: Instant,
    verify: impl FnOnce() -> io::Result<()> + Send + 'static,
) -> io::Result<()> {
    let timeout = || {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "Office signature verification exceeded its deadline",
        )
    };
    if Instant::now() >= deadline {
        return Err(timeout());
    }
    busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "an Office signature verification is still active",
            )
        })?;
    let slot = Slot(busy);
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("office-signature".to_owned())
        .spawn(move || {
            let result = if Instant::now() < deadline {
                verify()
            } else {
                drop(verify);
                Err(timeout())
            };
            drop(slot);
            let _ = sender.send(result);
        })?;
    let result = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => timeout(),
            mpsc::RecvTimeoutError::Disconnected => {
                io::Error::other("Office signature worker terminated")
            }
        })?;
    if Instant::now() >= deadline {
        return Err(timeout());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn deadline_returns_while_provider_is_blocked() {
        static BUSY: AtomicBool = AtomicBool::new(false);

        let (release, wait_for_release) = mpsc::channel();
        let (finished, wait_for_finish) = mpsc::channel();
        let deadline = Instant::now() + Duration::from_millis(250);
        let error = verify_until(&BUSY, deadline, move || {
            if wait_for_release.recv().is_ok() {
                finished.send(()).unwrap();
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(Instant::now() < deadline + Duration::from_secs(1));
        assert!(matches!(
            wait_for_finish.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(BUSY.load(Ordering::Acquire));
        assert!(
            verify_until(&BUSY, Instant::now() + Duration::from_secs(1), || {
                panic!("a second provider must not start while one is still blocked");
            })
            .is_err()
        );
        release.send(()).unwrap();
        wait_for_finish
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
    }
}
