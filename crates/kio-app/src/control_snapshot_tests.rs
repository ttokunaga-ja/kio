use crate::scope_access::maybe_wait_at_test_search_final_consent_barrier;
use kio_core::control::ControlHandle;
use std::fs;

use kio_core::test_control::{DebugTestControl, TestEnvGuard, test_env_lock};

#[test]
fn search_final_consent_barrier_uses_entry_snapshot() {
    let _lock = test_env_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.ready");
    let second = directory.path().join("second.ready");
    fs::write(first.with_extension("release"), b"release").unwrap();
    fs::write(second.with_extension("release"), b"release").unwrap();
    let _first = TestEnvGuard::set("KIO_TEST_SEARCH_FINAL_CONSENT_BARRIER_READY", &first);
    let control = ControlHandle::new(DebugTestControl::from_env());
    let _second = TestEnvGuard::set("KIO_TEST_SEARCH_FINAL_CONSENT_BARRIER_READY", &second);
    maybe_wait_at_test_search_final_consent_barrier(&control);
    assert!(first.exists(), "the entry snapshot owns the ready marker");
    assert!(
        !second.exists(),
        "a later environment value must not replace the owner snapshot"
    );
}
