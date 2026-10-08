pub(crate) const GC_RETIRE_SENTINEL: &[u8] = b"kio gc retirement sentinel\n";
use super::gc_io::{open_verified_file_handle, sync_bound_directory};
use crate::error::Result;
use crate::gc::{
    FileObservation, MAX_METADATA, corrupt, create_new_bound, id_meta, ioerr,
    read_regular_observed, unique_internal_name,
};
use crate::platform::{
    gc_exchange_bound as exchange_bound, gc_remove_reserved_leaf as remove_reserved_leaf,
    gc_rename_noreplace_bound as rename_noreplace_bound,
};
use cap_primitives::fs as cap_fs;

pub(crate) fn rename_snapshot_state_noreplace(
    dir: &std::fs::File,
    from: &str,
    to: &str,
) -> Result<()> {
    rename_noreplace_bound(dir, from, to)
}

/// Retire only the scheduler temporary created by this invocation.  The
/// exchange first captures the current name and compares its identity/bytes;
/// a substituted entry is preserved and fails closed rather than being
/// removed as cleanup.  This keeps repeated CAS conflicts from accumulating
/// unbounded private state files.
pub(crate) fn retire_snapshot_state_temporary(
    dir: &std::fs::File,
    temporary: &str,
    expected_bytes: &[u8],
    expected: &FileObservation,
) -> Result<()> {
    let (bytes, current) = read_regular_observed(dir, temporary, MAX_METADATA)?;
    if bytes != expected_bytes
        || current.identity != expected.identity
        || current.state.len != expected.state.len
        || current.digest != expected.digest
    {
        return Err(corrupt("snapshot auto temporary changed before retirement"));
    }
    let (captured, captured_observation) = exchange_capture_verified(
        dir,
        temporary,
        &current,
        MAX_METADATA,
        "snapshot auto temporary",
    )?;
    remove_verified_leaf(
        dir,
        &captured,
        &captured_observation,
        MAX_METADATA,
        "snapshot auto captured temporary",
    )?;
    remove_sentinel_leaf(dir, temporary)?;
    sync_bound_directory(dir, temporary)
}

/// Atomically exchange a public leaf with an operation-private sentinel, then
/// bind the captured entry's identity before a caller unlinks it. If a
/// replacement won the race, that replacement remains captured and is never
/// removed by this operation.
pub(crate) fn exchange_capture_verified(
    dir: &std::fs::File,
    leaf: &str,
    expected: &FileObservation,
    max: u64,
    label: &str,
) -> Result<(String, FileObservation)> {
    let captured = unique_internal_name(".gc-captured");
    exchange_capture_verified_named(dir, leaf, &captured, expected, max, label)
}

pub(crate) fn exchange_capture_verified_named(
    dir: &std::fs::File,
    leaf: &str,
    captured: &str,
    expected: &FileObservation,
    max: u64,
    label: &str,
) -> Result<(String, FileObservation)> {
    create_new_bound(dir, captured, GC_RETIRE_SENTINEL, MAX_METADATA)?;
    exchange_bound(dir, leaf, captured)?;
    // The deterministic captured state is a recovery state, so it must reach
    // stable storage before any caller can expose a crash seam after capture.
    sync_bound_directory(dir, leaf)?;
    let (_bytes, observed) = read_regular_observed(dir, captured, max)?;
    if observed.identity != expected.identity
        || observed.state.len != expected.state.len
        || observed.digest != expected.digest
    {
        return Err(corrupt(&format!(
            "{label} changed during retirement capture"
        )));
    }
    // Make both sides of the exchange durable before the caller may inject a
    // crash or remove the captured object.  Without this directory fsync, a
    // power loss could resurrect the pre-exchange archive name while recovery
    // observes marker progress that assumes the deterministic capture exists.
    sync_bound_directory(dir, captured)?;
    // The exchange changes ctime, but this exact observation is used by the
    // immediate descriptor-relative unlink below.
    Ok((captured.to_owned(), observed))
}

pub(crate) fn remove_verified_leaf(
    dir: &std::fs::File,
    leaf: &str,
    expected: &FileObservation,
    max: u64,
    label: &str,
) -> Result<()> {
    let (bytes, observed) = read_regular_observed(dir, leaf, max)?;
    if observed != *expected {
        return Err(corrupt(&format!("{label} changed before unlink")));
    }
    let handle = open_verified_file_handle(dir, leaf, &observed, max, label)?;
    let handle_observed =
        cap_fs::Metadata::from_file(&handle).map_err(|error| ioerr(error, leaf))?;
    if id_meta(&handle_observed)? != observed.identity
        || handle_observed.len() != bytes.len() as u64
    {
        return Err(corrupt(&format!(
            "{label} changed while binding unlink handle"
        )));
    }
    // This name lives only in the operation-reserved `.kio/gc/internal/`
    // namespace.  On Darwin, make the final descriptor-relative unlink also
    // reject a symlinked path, an escape, or a vnode which gained another
    // hardlink in the last syscall-width window.  POSIX still has no
    // unlink-if-inode-equals primitive; the remaining replacement window is
    // the explicitly documented reserved-namespace residual shared with the
    // restore quarantine protocol.
    remove_reserved_leaf(dir, leaf)
}

pub(crate) fn remove_sentinel_leaf(dir: &std::fs::File, leaf: &str) -> Result<()> {
    let (bytes, observed) = read_regular_observed(dir, leaf, MAX_METADATA)?;
    if bytes != GC_RETIRE_SENTINEL {
        return Err(corrupt(
            "GC retirement sentinel was replaced before cleanup",
        ));
    }
    remove_verified_leaf(dir, leaf, &observed, MAX_METADATA, "GC retirement sentinel")
}
