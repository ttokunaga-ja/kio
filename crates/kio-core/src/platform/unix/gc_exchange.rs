//! Unix atomic GC replacement contracts retain the predecessor until validation.
use super::gc_io::{open_verified_file_handle, sync_bound_directory};
use super::gc_retirement::{
    exchange_capture_verified, remove_sentinel_leaf, remove_verified_leaf,
    retire_snapshot_state_temporary,
};
use crate::Result;
use crate::gc::{
    FileObservation, GcInProgressMarker, MAX_MARKER_BYTES, MAX_METADATA, corrupt, create_new_bound,
    id_meta, ioerr, parse_snapshot_auto_state, read_regular_observed, snapshot_auto_state_changed,
    unique_internal_name,
};
use crate::platform::{
    gc_exchange_between as exchange_between, gc_exchange_bound as exchange_bound,
    gc_rename_noreplace_between as rename_noreplace_between,
};
use cap_primitives::fs as cap_fs;
/// Replace an exact observed scheduler checkpoint without an absent public
/// state window. Darwin/Linux retain the predecessor through atomic exchange,
/// validate that it is the expected inode/bytes, and only then retire it.
pub(crate) fn replace_snapshot_state_expected(
    dir: &std::fs::File,
    leaf: &str,
    temporary: &str,
    replacement: &[u8],
    expected: &FileObservation,
    control: &crate::control::ControlHandle,
) -> Result<()> {
    exchange_bound(dir, leaf, temporary)?;
    sync_bound_directory(dir, leaf)?;
    let (old_bytes, old) = read_regular_observed(dir, temporary, MAX_METADATA)?;
    let old_ok = old.identity == expected.identity
        && old.state.len == expected.state.len
        && old.digest == expected.digest
        && parse_snapshot_auto_state(&old_bytes).is_ok();
    let public = read_regular_observed(dir, leaf, MAX_METADATA);
    let new_ok = public
        .as_ref()
        .is_ok_and(|(bytes, _)| bytes == replacement && parse_snapshot_auto_state(bytes).is_ok());
    if !old_ok || !new_ok {
        // The exchange captured a writer which won after our last observation.
        // Restore that captured checkpoint to the public name before failing.
        // A non-cooperating third writer can still alter either reserved state
        // name in this syscall-width window; the post-exchange observations
        // detect that case, and no pathname is unlinked in either outcome.
        let captured_before = old.clone();
        let replacement_before = public.map(|(_, observation)| observation)?;
        exchange_bound(dir, leaf, temporary)?;
        sync_bound_directory(dir, leaf)?;
        let restored = read_regular_observed(dir, leaf, MAX_METADATA);
        let retired_replacement = read_regular_observed(dir, temporary, MAX_METADATA);
        let restored_ok = restored.as_ref().is_ok_and(|(_, observation)| {
            observation.identity == captured_before.identity
                && observation.state.len == captured_before.state.len
                && observation.digest == captured_before.digest
        });
        let replacement_retired_ok =
            retired_replacement
                .as_ref()
                .is_ok_and(|(bytes, observation)| {
                    bytes == replacement
                        && observation.identity == replacement_before.identity
                        && observation.state.len == replacement_before.state.len
                        && observation.digest == replacement_before.digest
                });
        if !restored_ok || !replacement_retired_ok {
            return Err(corrupt(
                "snapshot auto state changed during failed replacement rollback",
            ));
        }
        retire_snapshot_state_temporary(dir, temporary, replacement, &replacement_before, control)?;
        return Err(snapshot_auto_state_changed());
    }
    let old_handle = open_verified_file_handle(
        dir,
        temporary,
        &old,
        MAX_METADATA,
        "snapshot auto predecessor state",
    )?;
    let (captured, captured_observation) = exchange_capture_verified(
        dir,
        temporary,
        &old,
        MAX_METADATA,
        "snapshot auto predecessor state",
    )?;
    remove_verified_leaf(
        dir,
        &captured,
        &captured_observation,
        MAX_METADATA,
        "snapshot auto captured predecessor state",
    )?;
    remove_sentinel_leaf(dir, temporary)?;
    sync_bound_directory(dir, temporary)?;
    let unlinked =
        cap_fs::Metadata::from_file(&old_handle).map_err(|error| ioerr(error, temporary))?;
    if id_meta(&unlinked)? != old.identity || link_count(&unlinked)? != 0 {
        return Err(corrupt(
            "snapshot auto predecessor state gained another name",
        ));
    }
    Ok(())
}

pub(crate) fn atomic_exchange_marker_expected(
    dir: &std::fs::File,
    leaf: &str,
    archives: &std::fs::File,
    bytes: &[u8],
    expected: &FileObservation,
) -> Result<()> {
    let temporary = unique_internal_name(".gc-retired-marker");
    create_new_bound(archives, &temporary, bytes, MAX_MARKER_BYTES)?;
    // The expected marker is re-read through the same retained descriptor
    // immediately before exchange. The exchange preserves it at `temporary`;
    // no unverified leaf is ever overwritten or unlinked.
    let matches = read_regular_observed(dir, leaf, MAX_MARKER_BYTES)
        .is_ok_and(|(_, observed)| observed == *expected);
    if !matches {
        // The temporary was created by this call, but never remove its name
        // blindly after a failed public-marker comparison. A replacement is
        // retained for diagnosis instead of becoming an unlink victim.
        if let Ok((actual, observation)) =
            read_regular_observed(archives, &temporary, MAX_MARKER_BYTES)
            && actual == bytes
        {
            let _ = remove_verified_leaf(
                archives,
                &temporary,
                &observation,
                MAX_MARKER_BYTES,
                "GC unpublished replacement marker",
            );
        }
        return Err(corrupt("GC marker changed before replacement"));
    }
    exchange_between(dir, leaf, archives, &temporary)?;
    // Exchange legitimately updates ctime, so bind the archived marker by its
    // stable identity, length and digest rather than full metadata state.
    let archived = read_regular_observed(archives, &temporary, MAX_MARKER_BYTES);
    let old_ok = archived.as_ref().is_ok_and(|(actual, observed)| {
        observed.identity == expected.identity
            && observed.state.len == expected.state.len
            && observed.digest == expected.digest
            && GcInProgressMarker::parse_canonical(actual).is_ok()
    });
    let new_ok = read_regular_observed(dir, leaf, MAX_MARKER_BYTES).is_ok_and(|(actual, _)| {
        actual == bytes && GcInProgressMarker::parse_canonical(&actual).is_ok()
    });
    if !old_ok || !new_ok {
        // Do not attempt a rollback after validation fails. A second exchange
        // would itself need a fresh atomic identity precondition for both
        // names and could swap a hostile replacement into the public marker
        // path. Preserve both entries for fail-closed recovery/diagnosis.
        return Err(corrupt("GC marker exchange validation failed"));
    }
    sync_bound_directory(dir, leaf)?;
    sync_bound_directory(archives, &temporary)?;
    let (_, archived_observation) = archived?;
    let retired_handle = open_verified_file_handle(
        archives,
        &temporary,
        &archived_observation,
        MAX_MARKER_BYTES,
        "GC retired marker",
    )?;
    let (captured, captured_observation) = exchange_capture_verified(
        archives,
        &temporary,
        &archived_observation,
        MAX_MARKER_BYTES,
        "GC retired marker",
    )?;
    remove_verified_leaf(
        archives,
        &captured,
        &captured_observation,
        MAX_MARKER_BYTES,
        "GC captured retired marker",
    )?;
    remove_sentinel_leaf(archives, &temporary)?;
    sync_bound_directory(archives, &temporary)?;
    let unlinked =
        cap_fs::Metadata::from_file(&retired_handle).map_err(|error| ioerr(error, &temporary))?;
    if id_meta(&unlinked)? != archived_observation.identity || link_count(&unlinked)? != 0 {
        return Err(corrupt("GC retired marker gained another name"));
    }
    Ok(())
}

pub(crate) fn link_count(metadata: &cap_fs::Metadata) -> Result<u64> {
    #[cfg(unix)]
    {
        use cap_primitives::fs::MetadataExt;
        Ok(metadata.nlink())
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        metadata
            .number_of_links()
            .map(u64::from)
            .ok_or_else(|| corrupt("store link count is unavailable"))
    }
}

pub(crate) fn remove_completed_marker(
    gc: &std::fs::File,
    markers: &std::fs::File,
    _expected: &GcInProgressMarker,
    observation: &FileObservation,
    _control: &crate::control::ControlHandle,
) -> Result<()> {
    let archive = unique_internal_name("completed");
    rename_noreplace_between(gc, "in_progress", markers, &archive)?;
    sync_bound_directory(markers, &archive)?;
    // Commit the source disappearance before retiring the destination,
    // otherwise a power loss could resurrect the public marker after the
    // internal archive has been unlinked.
    sync_bound_directory(gc, "gc")?;
    let (moved_bytes, moved) = read_regular_observed(markers, &archive, MAX_MARKER_BYTES)?;
    if moved.identity != observation.identity
        || moved.state.len != observation.state.len
        || moved.digest != observation.digest
        || GcInProgressMarker::parse_canonical(&moved_bytes).is_err()
    {
        // Never roll back after a failed identity check: a second move
        // could restore a hostile replacement into the public marker
        // name. Preserve the archived entry for fail-closed diagnosis.
        return Err(corrupt("GC marker changed during completion archive"));
    }
    let moved_handle = open_verified_file_handle(
        markers,
        &archive,
        &moved,
        MAX_MARKER_BYTES,
        "GC completed marker",
    )?;
    // The archive name is only a capability-relative quarantine used to
    // prove that the exact active marker left the public name. Remove it
    // in the same completion step so successful sweeps do not retain an
    // unbounded marker history. If another name was added, the retained
    // handle exposes nlink != 0 and completion fails without changing the
    // bytes reachable through that other name.
    // Retire through an atomic exchange so a replacement at the archive
    // name is captured and identity-checked before any unlink. A foreign
    // marker is retained on mismatch rather than deleted.
    let (captured, captured_observation) = exchange_capture_verified(
        markers,
        &archive,
        &moved,
        MAX_MARKER_BYTES,
        "GC completed marker",
    )?;
    remove_verified_leaf(
        markers,
        &captured,
        &captured_observation,
        MAX_MARKER_BYTES,
        "GC captured completed marker",
    )?;
    remove_sentinel_leaf(markers, &archive)?;
    sync_bound_directory(markers, &archive)?;
    let unlinked =
        cap_fs::Metadata::from_file(&moved_handle).map_err(|error| ioerr(error, &archive))?;
    if id_meta(&unlinked)? != moved.identity || link_count(&unlinked)? != 0 {
        return Err(corrupt("GC completed marker gained another name"));
    }
    sync_bound_directory(gc, "gc")
}
