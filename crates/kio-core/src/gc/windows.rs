//! Windows GC mutations keep DELETE authority on the verified victim handle.
use super::*;
use crate::platform::gc_io::{directory, matches_stable, observe};
pub(super) use crate::platform::gc_io::{publish_staged, remove_completed_marker, retire_expected};
use std::fs::File;

impl GcSweepSession {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn remove_candidate_tree_windows(
        &self,
        permit: &GcTreeRemovalPermit,
        marker: &GcInProgressMarker,
        tree_hash: &str,
        raw: &str,
        canonical_parent: &File,
        archive_parent: &File,
        quarantine: &str,
    ) -> Result<bool> {
        let source = directory(canonical_parent)?;
        let archive = directory(archive_parent)?;
        let (mut victim, needs_move) =
            match crate::platform::open_gc_mutation(&source, Path::new(raw), MAX_TREE_OBJECT_BYTES)
            {
                Ok(file) => (file, true),
                Err(error) if is_io_not_found(&error) => {
                    match crate::platform::open_gc_mutation(
                        &archive,
                        Path::new(quarantine),
                        MAX_TREE_OBJECT_BYTES,
                    ) {
                        Ok(file) => (file, false),
                        // Disposition already removed every name before interruption.
                        Err(error) if is_io_not_found(&error) => return Ok(false),
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            };
        let (bytes, before) = observe(&mut victim, MAX_TREE_OBJECT_BYTES)?;
        self.validate_committed_tree_bytes(marker, tree_hash, &bytes)?;
        if needs_move {
            // This checks any conflicting archive, including unsafe entries;
            // the no-replace move is the final collision authority.
            match read_regular_observed(archive_parent, quarantine, MAX_TREE_OBJECT_BYTES) {
                Ok(_) => return Err(corrupt("GC tree exists at canonical and archive paths")),
                Err(error) if is_io_not_found(&error) => {}
                Err(error) => return Err(error),
            }
            self.require_active_marker(marker)?;
            self.wait_at_gc_pre_quarantine_barrier();
            self.validate_tree_removal_permit(permit, marker)?;
            self.require_live_pre_sweep_index_binding(marker)?;
            crate::platform::move_verified_regular_handle_to_raw(
                &source,
                &victim,
                &archive,
                Path::new(quarantine),
            )?;
        }
        let (moved, observed) = observe(&mut victim, MAX_TREE_OBJECT_BYTES)?;
        if moved != bytes || !matches_stable(&observed, &before) || hash_bytes(&moved) != tree_hash
        {
            return Err(corrupt("tree changed during GC quarantine"));
        }
        self.inject_gc_tree_fault(GcTreeFaultPoint::TreeQuarantine)?;
        self.wait_at_gc_tree_quarantine_barrier();
        self.require_active_marker(marker)?;
        self.validate_tree_removal_permit(permit, marker)?;
        self.require_live_pre_sweep_index_binding(marker)?;
        self.recheck_tree_receipts(marker, tree_hash)?;
        let (final_bytes, final_observation) = observe(&mut victim, MAX_TREE_OBJECT_BYTES)?;
        if final_bytes != bytes || !matches_stable(&final_observation, &before) {
            return Err(corrupt("tree changed immediately before GC erase"));
        }
        self.inject_gc_tree_fault(GcTreeFaultPoint::TreeRetirementCapture)?;
        crate::platform::retire_gc_handle(&archive, &victim)?;
        Ok(true)
    }
}

fn marker_binding() -> String {
    hash_bytes(b"kio GC marker exchange protocol v1")[7..].to_owned()
}

fn marker_from_handle(file: &File) -> Result<GcInProgressMarker> {
    let mut duplicate = file.try_clone().map_err(|e| ioerr(e, "GC marker"))?;
    let (bytes, _) = observe(&mut duplicate, MAX_MARKER_BYTES)?;
    GcInProgressMarker::parse_canonical(&bytes)
}

fn cleanup_marker_temporaries(markers: &File) -> Result<()> {
    let mut names = Vec::new();
    for entry in cap_fs::read_base_dir(markers).map_err(|e| ioerr(e, "GC markers"))? {
        let entry = entry.map_err(|e| ioerr(e, "GC markers"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(suffix) = name.strip_prefix(".gc-retired-marker-") else {
            continue;
        };
        let Some((pid, nanos)) = suffix.split_once('-') else {
            return Err(corrupt("malformed GC retired marker name"));
        };
        if pid.is_empty()
            || nanos.is_empty()
            || !pid.bytes().all(|b| b.is_ascii_digit())
            || !nanos.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(corrupt("malformed GC retired marker name"));
        }
        names.push(name.to_owned());
        if names.len() > MAX_SNAPSHOT_AUTO_STATE_TEMPORARIES {
            return Err(limit("GC retired marker count"));
        }
    }
    for leaf in names {
        let (bytes, observed) = read_regular_observed(markers, &leaf, MAX_MARKER_BYTES)?;
        GcInProgressMarker::parse_canonical(&bytes)?;
        retire_expected(markers, &leaf, &bytes, &observed, MAX_MARKER_BYTES)?;
    }
    Ok(())
}

impl GcSweepSession {
    fn validate_pending_marker_handles(&self, source: Option<&File>, target: &File) -> Result<()> {
        let target = marker_from_handle(target)?;
        if let Some(source) = source {
            validate_marker_transition(&marker_from_handle(source)?, &target)?;
        }
        // In the terminal placement no predecessor remains. Only validate the
        // retained target and current truth; do not reconstruct old authority.
        let receipts = read_shallow_receipts_bound(&self.kio)?;
        let frozen = target
            .candidates
            .iter()
            .map(|candidate| (&candidate.commit_hash, &candidate.tree_hash))
            .collect();
        self.validate_marker_phase_state(&target, &receipts, &frozen)?;
        if let Some(expected) = &target.operation_receipts_digest
            && operation_receipt_observation_digest_bound(&self.kio, &target)? != *expected
        {
            return Err(corrupt("pending marker receipt identity/content changed"));
        }
        self.validate_frozen_marker_truth_without_public_read(&target)
    }

    pub(super) fn exchange_marker_windows(
        &self,
        gc: &File,
        internal: &File,
        markers: &File,
        marker: &GcInProgressMarker,
        expected: &FileObservation,
    ) -> Result<()> {
        use windows_exchange::{ExchangeExpected, OperationKind};
        let owner_file = ensure_child_dir(internal, "marker-exchange")?;
        let owner = directory(&owner_file)?;
        if windows_exchange::inspect_pending(&owner)? {
            return Err(corrupt("GC marker exchange already pending"));
        }
        cleanup_marker_temporaries(markers)?;
        let public = directory(gc)?;
        let prepared = directory(markers)?;
        let temporary = unique_internal_name(".gc-retired-marker");
        create_new_bound(
            markers,
            &temporary,
            &marker.canonical_bytes()?,
            MAX_MARKER_BYTES,
        )?;
        let authority = {
            let mut source = crate::platform::open_gc_mutation(
                &public,
                Path::new("in_progress"),
                MAX_MARKER_BYTES,
            )?;
            let (_, observed) = observe(&mut source, MAX_MARKER_BYTES)?;
            if observed != *expected {
                return Err(corrupt("GC marker changed before replacement"));
            }
            let target = crate::platform::open_gc_mutation(
                &prepared,
                Path::new(&temporary),
                MAX_MARKER_BYTES,
            )?;
            if marker_from_handle(&target)? != *marker {
                return Err(corrupt("GC prepared marker changed before exchange"));
            }
            ExchangeExpected {
                source: windows_exchange::observe(&source, MAX_MARKER_BYTES)?,
                target: windows_exchange::observe(&target, MAX_MARKER_BYTES)?,
            }
        };
        let pending = windows_exchange::begin(
            &owner,
            &public,
            "in_progress",
            &prepared,
            &temporary,
            OperationKind::Marker,
            &authority,
            MAX_MARKER_BYTES,
            &marker_binding(),
            |source, target| self.validate_pending_marker_handles(Some(source), target),
        )?;
        pending.complete(&authority, |_| Ok(()))?;
        Ok(())
    }

    pub(super) fn recover_marker_windows(&self) -> Result<()> {
        use windows_exchange::{ExchangeExpected, OperationKind};
        let Some(gc) = open_optional_dir(&self.kio, "gc")? else {
            return Ok(());
        };
        let Some(internal) = open_optional_dir(&gc, "internal")? else {
            return Ok(());
        };
        let Some(markers) = open_optional_dir(&internal, "markers")? else {
            if has_pending_marker_exchange(&self.kio)? {
                return Err(corrupt("pending marker exchange has no marker directory"));
            }
            return Ok(());
        };
        if let Some(owner_file) = open_optional_dir(&internal, "marker-exchange")? {
            let owner = directory(&owner_file)?;
            let public = directory(&gc)?;
            let prepared = directory(&markers)?;
            if let Some(pending) = windows_exchange::open_pending_under_lock(
                &owner,
                &public,
                &prepared,
                OperationKind::Marker,
                MAX_MARKER_BYTES,
                &marker_binding(),
            )? {
                self.validate_pending_marker_handles(pending.source(), pending.target())?;
                let authority = ExchangeExpected {
                    source: pending.source_record().clone(),
                    target: pending.target_record().clone(),
                };
                pending.complete(&authority, |_| Ok(()))?;
            }
        }
        cleanup_marker_temporaries(&markers)
    }
}
