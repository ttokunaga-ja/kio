use std::fs::File;

pub(crate) fn try_recover_marked_index_rotation(
    _kio_dir: &File,
    _temp_leaf: &str,
    _expected_private_dir_identity: &str,
    _expected_source_identity: &str,
    _expected_source_state_digest: &str,
    _expected_target_identity: &str,
    _control: &kio_core::control::ControlHandle,
) -> kio_index::Result<bool> {
    Ok(false)
}
