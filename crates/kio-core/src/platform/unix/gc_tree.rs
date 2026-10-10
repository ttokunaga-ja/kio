use super::gc_exchange::link_count;
use super::gc_io::{open_gc_tree_writable, sync_bound_directory};
use super::gc_retirement::{
    GC_RETIRE_SENTINEL, exchange_capture_verified_named, remove_sentinel_leaf, remove_verified_leaf,
};
use crate::cas::{MAX_TREE_OBJECT_BYTES, hash_bytes};
use crate::error::Result;
use crate::gc::{corrupt, id_meta, ioerr, is_io_not_found, read_regular_observed};
use crate::platform::gc_rename_noreplace_between as rename_noreplace_between;
use cap_primitives::fs as cap_fs;
#[allow(clippy::too_many_arguments)]
pub(crate) fn retire_candidate_tree(
    tree_hash: &str,
    raw: &str,
    canonical_parent: &std::fs::File,
    archive_parent: &std::fs::File,
    quarantine: &str,
    captured_name: &str,
    validate_bytes: impl Fn(&[u8]) -> Result<()>,
    require_marker: impl Fn() -> Result<()>,
    before_quarantine: impl Fn() -> Result<()>,
    quarantine_checkpoint: impl Fn() -> Result<()>,
    _before_erase: impl Fn() -> Result<()>,
    retirement_checkpoint: impl Fn() -> Result<()>,
    _control: &crate::control::ControlHandle,
) -> Result<bool> {
    // Either start from the canonical CAS leaf or resume the exact
    // operation-owned quarantine left by a crash after the durable rename.
    // Both paths bind a no-follow writable descriptor before the archive
    // name is removed; no ambient pathname is ever used for deletion.
    let canonical = read_regular_observed(canonical_parent, raw, MAX_TREE_OBJECT_BYTES);
    let (bytes, before, writable, already_captured) = match canonical {
        Ok((bytes, before)) => {
            validate_bytes(&bytes)?;
            let writable = open_gc_tree_writable(canonical_parent, raw, &before)?;
            // A pre-existing operation archive together with the canonical
            // leaf is not a resumable state and must never be overwritten.
            if read_regular_observed(archive_parent, quarantine, MAX_TREE_OBJECT_BYTES).is_ok() {
                return Err(corrupt("GC tree exists at canonical and archive paths"));
            }
            require_marker()?;
            let (final_bytes, final_observation) =
                read_regular_observed(canonical_parent, raw, MAX_TREE_OBJECT_BYTES)?;
            if final_observation != before || final_bytes != bytes {
                return Err(corrupt("tree changed immediately before GC quarantine"));
            }
            before_quarantine()?;
            // The index generation/identity is part of the irreversible
            // deletion authority, not merely an executor preflight.  Bind
            // it again at the final namespace transition after the longer
            // ref/receipt/tree verification above.

            rename_noreplace_between(canonical_parent, raw, archive_parent, quarantine)?;
            // Persist disappearance from CAS and appearance in quarantine
            // before any crash seam or byte reclamation.
            sync_bound_directory(archive_parent, quarantine)?;
            sync_bound_directory(canonical_parent, raw)?;
            (bytes, before, writable, false)
        }
        Err(error) if is_io_not_found(&error) => {
            let (bytes, before, already_captured) =
                match read_regular_observed(archive_parent, quarantine, MAX_TREE_OBJECT_BYTES) {
                    Ok((sentinel, _)) if sentinel == GC_RETIRE_SENTINEL => {
                        match read_regular_observed(
                            archive_parent,
                            captured_name,
                            MAX_TREE_OBJECT_BYTES,
                        ) {
                            Ok((captured, observed)) => (captured, observed, true),
                            Err(error) if is_io_not_found(&error) => {
                                // The captured tree was already unlinked
                                // before a crash. Its sentinel is a durable,
                                // deterministic completion record; remove it
                                // only after the fresh marker binding above.
                                require_marker()?;
                                remove_sentinel_leaf(archive_parent, quarantine)?;
                                sync_bound_directory(archive_parent, quarantine)?;
                                return Ok(true);
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    Ok(value) => (value.0, value.1, false),
                    Err(error) if is_io_not_found(&error) => return Ok(false),
                    Err(error) => return Err(error),
                };
            validate_bytes(&bytes)?;
            let writable = open_gc_tree_writable(
                archive_parent,
                if already_captured {
                    captured_name
                } else {
                    quarantine
                },
                &before,
            )?;
            (bytes, before, writable, already_captured)
        }
        Err(error) => return Err(error),
    };

    let quarantined = read_regular_observed(
        archive_parent,
        if already_captured {
            captured_name
        } else {
            quarantine
        },
        MAX_TREE_OBJECT_BYTES,
    );
    // Rename legitimately updates ctime, so only identity + byte length and
    // hash are stable across the quarantine transition.
    let valid_quarantine = quarantined.as_ref().is_ok_and(|(bytes, observation)| {
        hash_bytes(bytes) == tree_hash
            && observation.identity == before.identity
            && observation.state.len == before.state.len
    });
    if !valid_quarantine {
        // Do not restore: a replacement raw leaf could have appeared after
        // the no-replace move. Leaving the quarantined object is safe and
        // forces explicit recovery rather than risking an overwrite.
        return Err(corrupt("tree changed during GC quarantine"));
    }
    let handle_meta = cap_fs::Metadata::from_file(&writable).map_err(|error| ioerr(error, raw))?;
    if id_meta(&handle_meta)? != before.identity || handle_meta.len() != before.state.len {
        return Err(corrupt("tree writable handle changed during quarantine"));
    }
    if hash_bytes(&bytes) != tree_hash {
        return Err(corrupt("tree bytes changed during GC quarantine"));
    }
    quarantine_checkpoint()?;
    // The test seam is deliberately before this final check. Rebind both
    // authorities immediately before capture: a marker replacement must
    // stop the erase, and a renamed archive victim is captured under a
    // fresh private name then compared before any unlink occurs.
    require_marker()?;
    let erase_leaf = if already_captured {
        captured_name
    } else {
        quarantine
    };
    let (final_bytes, final_observation) =
        read_regular_observed(archive_parent, erase_leaf, MAX_TREE_OBJECT_BYTES)?;
    if final_observation.identity != before.identity
        || final_observation.state.len != before.state.len
        || final_observation.digest != hash_bytes(&bytes)
        || final_bytes != bytes
    {
        return Err(corrupt("tree changed immediately before GC erase"));
    }
    let (captured, captured_observation) = if already_captured {
        (captured_name.to_owned(), final_observation)
    } else {
        exchange_capture_verified_named(
            archive_parent,
            quarantine,
            captured_name,
            &final_observation,
            MAX_TREE_OBJECT_BYTES,
            "GC tree archive",
        )?
    };
    retirement_checkpoint()?;
    // Unlink only the private captured name, then require the retained
    // object to have no remaining names before truncating it. A same-UID
    // hardlink inserted at any point leaves nlink > 0 and aborts without
    // modifying bytes owned by another pathname.
    remove_verified_leaf(
        archive_parent,
        &captured,
        &captured_observation,
        MAX_TREE_OBJECT_BYTES,
        "GC captured tree archive",
    )?;
    remove_sentinel_leaf(archive_parent, quarantine)?;
    sync_bound_directory(archive_parent, quarantine)?;
    let unlinked = cap_fs::Metadata::from_file(&writable).map_err(|error| ioerr(error, raw))?;
    if id_meta(&unlinked)? != before.identity || link_count(&unlinked)? != 0 {
        return Err(corrupt("tree archive gained a hardlink before byte erase"));
    }
    writable.set_len(0).map_err(|error| ioerr(error, raw))?;
    writable.sync_all().map_err(|error| ioerr(error, raw))?;
    let erased = cap_fs::Metadata::from_file(&writable).map_err(|error| ioerr(error, raw))?;
    if id_meta(&erased)? != before.identity || erased.len() != 0 || link_count(&erased)? != 0 {
        return Err(corrupt(
            "tree archive truncate did not bind expected object",
        ));
    }
    sync_bound_directory(canonical_parent, raw)?;
    Ok(true)
}
