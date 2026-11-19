use crate::cas::CasReadAccess;
use crate::error::Result;
use crate::gc::{
    FileObservation, GcInProgressMarker, SnapshotExchangeAuthority, corrupt,
    has_pending_marker_exchange, is_io_not_found, reject_pending_snapshot_state_exchange,
};
use std::fs::File;
use std::path::{Path, PathBuf};
pub(crate) fn recover_marker(
    kio: &File,
    control: &crate::control::ControlHandle,
    _validate_marker: impl Fn(Option<GcInProgressMarker>, GcInProgressMarker) -> Result<()>,
) -> Result<()> {
    if has_pending_marker_exchange(kio, control)? {
        return Err(corrupt(
            "Windows GC marker exchange cannot recover on this platform",
        ));
    }
    Ok(())
}
pub(crate) fn exchange_marker(
    gc: &File,
    _control: &crate::control::ControlHandle,
    _internal: &File,
    markers: &File,
    marker: &GcInProgressMarker,
    expected: &FileObservation,
    _validate_marker: impl Fn(Option<GcInProgressMarker>, GcInProgressMarker) -> Result<()>,
) -> Result<()> {
    super::gc_exchange::atomic_exchange_marker_expected(
        gc,
        "in_progress",
        markers,
        &marker.canonical_bytes()?,
        expected,
    )
}
pub(crate) fn replace_snapshot_state<D, R>(
    kio: &File,
    _control: &crate::control::ControlHandle,
    temporary: &str,
    bytes: &[u8],
    expected: &FileObservation,
    _temporary_observation: &FileObservation,
    _derive: D,
) -> Result<()>
where
    D: FnOnce() -> Result<(SnapshotExchangeAuthority, R)>,
    R: Fn() -> Result<SnapshotExchangeAuthority>,
{
    super::gc_exchange::replace_snapshot_state_expected(
        kio,
        crate::gc::SNAPSHOT_AUTO_STATE_LEAF,
        temporary,
        bytes,
        expected,
        _control,
    )
}
pub(crate) fn recover_snapshot_state(
    kio: &File,
    control: &crate::control::ControlHandle,
    _derive: impl Fn() -> Result<SnapshotExchangeAuthority>,
    _admit: impl Fn() -> Result<()>,
) -> Result<()> {
    reject_pending_snapshot_state_exchange(kio, control)
}
pub(crate) fn marker_archive_presence(
    reads: &CasReadAccess,
    archives: &File,
    archive: &str,
    captured_name: &str,
    expected_size: u64,
    validate_observation: impl Fn(&FileObservation) -> Result<()>,
) -> Result<(bool, bool)> {
    Ok(
        match crate::gc::read_tree_archive_observed(reads, archives, archive, expected_size.max(64))
        {
            Ok((None, _)) => {
                match crate::gc::read_cas_tree_observed(
                    reads,
                    archives,
                    captured_name,
                    expected_size.max(64),
                ) {
                    Ok((_, observation)) => {
                        validate_observation(&observation)?;
                        (false, true)
                    }
                    Err(error) if is_io_not_found(&error) => (false, false),
                    Err(error) => return Err(error),
                }
            }
            Ok((Some(_), observation)) => {
                validate_observation(&observation)?;
                (true, false)
            }
            Err(error) if is_io_not_found(&error) => (false, false),
            Err(error) => return Err(error),
        },
    )
}
pub(crate) fn normalize_gc_scope_path(path: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Ok(rest) = path.strip_prefix("/var") {
            return Path::new("/private/var").join(rest);
        }
    }
    path.to_path_buf()
}
