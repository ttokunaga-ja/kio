use super::gc_retirement::GC_RETIRE_SENTINEL;
use crate::cas::MAX_TREE_OBJECT_BYTES;
use crate::error::Result;
use crate::gc::{
    FileObservation, GcInProgressMarker, SnapshotExchangeAuthority, corrupt,
    has_pending_marker_exchange, is_io_not_found, read_regular_observed,
    reject_pending_snapshot_state_exchange,
};
use std::fs::File;
use std::path::{Path, PathBuf};
pub(crate) fn recover_marker(
    kio: &File,
    _validate_marker: impl Fn(Option<GcInProgressMarker>, GcInProgressMarker) -> Result<()>,
) -> Result<()> {
    if has_pending_marker_exchange(kio)? {
        return Err(corrupt(
            "Windows GC marker exchange cannot recover on this platform",
        ));
    }
    Ok(())
}
pub(crate) fn exchange_marker(
    gc: &File,
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
    )
}
pub(crate) fn recover_snapshot_state(
    kio: &File,
    _derive: impl Fn() -> Result<SnapshotExchangeAuthority>,
    _admit: impl Fn() -> Result<()>,
) -> Result<()> {
    reject_pending_snapshot_state_exchange(kio)
}
pub(crate) fn marker_archive_presence(
    archives: &File,
    archive: &str,
    captured_name: &str,
    validate_bytes: impl Fn(&[u8]) -> Result<()>,
) -> Result<(bool, bool)> {
    Ok(
        match read_regular_observed(archives, archive, MAX_TREE_OBJECT_BYTES) {
            Ok((sentinel, _)) if sentinel == GC_RETIRE_SENTINEL => {
                match read_regular_observed(archives, captured_name, MAX_TREE_OBJECT_BYTES) {
                    Ok((bytes, _)) => {
                        validate_bytes(&bytes)?;
                        (false, true)
                    }
                    Err(error) if is_io_not_found(&error) => (false, false),
                    Err(error) => return Err(error),
                }
            }
            Ok((bytes, _)) => {
                validate_bytes(&bytes)?;
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
