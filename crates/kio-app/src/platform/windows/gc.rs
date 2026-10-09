//! Bridge to Windows index recovery for the marker-bound namespace.
use std::fs::File;

pub(crate) fn recover_prepared_bound_gc_index(
    kio_dir: &File,
    temp_leaf: &str,
    expected_private_dir_identity: &str,
    expected_source_identity: &str,
    expected_source_state_digest: &str,
    expected_target_identity: &str,
) -> kio_index::Result<bool> {
    kio_index::platform::recover_prepared_bound_gc_index(
        kio_dir,
        temp_leaf,
        expected_private_dir_identity,
        expected_source_identity,
        expected_source_state_digest,
        expected_target_identity,
    )
}
