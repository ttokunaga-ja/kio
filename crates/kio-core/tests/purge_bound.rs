#![cfg(any(unix, windows))]

use kio_core::{
    cas::hash_bytes,
    purge::{LifecycleEvent, PurgeReason, PurgeState},
    store_dir::StoreDirectory,
};

#[test]
fn purge_marker_state_uses_the_retained_store_directory() {
    let fixture = tempfile::tempdir().expect("temporary scope");
    // Resolve the explicit scope root once before naming `.kio`; this avoids
    // macOS's `/var` alias while retaining `.kio` as a no-follow leaf.
    let kio = fixture
        .path()
        .canonicalize()
        .expect("canonical scope root")
        .join(".kio");
    std::fs::create_dir(&kio).expect("scope store");
    let directory = StoreDirectory::open(&kio).expect("retained store");
    let state = PurgeState::from_directory(directory);

    let raw = hash_bytes(b"purge retained raw");
    let commit = hash_bytes(b"purge retained commit");
    let event = LifecycleEvent::purged(
        "2026-09-07T00:00:00Z",
        commit,
        PurgeReason::Privacy,
        "test",
        1,
    );
    let recorded = state
        .append_tombstone_event(&raw, event)
        .expect("write retained tombstone");
    assert!(recorded.is_active());
    assert_eq!(
        state
            .read_tombstone(&raw)
            .expect("read retained tombstone")
            .expect("marker exists"),
        recorded
    );
}
