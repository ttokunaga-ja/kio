//! Windows GC mutations keep DELETE authority on the verified victim handle.
use super::*;
use std::fs::File;
use std::io::{Seek, SeekFrom};

fn directory(file: &File) -> Result<StoreDirectory> {
    StoreDirectory::from_retained(
        file.try_clone().map_err(|e| ioerr(e, "GC directory"))?,
        PathBuf::from("<retained GC directory>"),
    )
}

fn observe(file: &mut File, max: u64) -> Result<(Vec<u8>, FileObservation)> {
    let before = cap_fs::Metadata::from_file(file).map_err(|e| ioerr(e, "GC victim"))?;
    valid_file(&before, max)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| ioerr(e, "GC victim"))?;
    let mut bytes = Vec::new();
    (&mut *file)
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| ioerr(e, "GC victim"))?;
    let after = cap_fs::Metadata::from_file(file).map_err(|e| ioerr(e, "GC victim"))?;
    valid_file(&after, max)?;
    if !same_file_state(&before, &after)? || bytes.len() as u64 != before.len() {
        return Err(corrupt("GC retained victim changed while reading"));
    }
    Ok((
        bytes.clone(),
        FileObservation {
            identity: id_meta(&after)?,
            state: file_state(&after),
            digest: hash_bytes(&bytes),
        },
    ))
}

fn matches_stable(actual: &FileObservation, expected: &FileObservation) -> bool {
    actual.identity == expected.identity
        && actual.state.len == expected.state.len
        && actual.digest == expected.digest
}

pub(super) fn retire_expected(
    parent: &File,
    leaf: &str,
    bytes: &[u8],
    expected: &FileObservation,
    max: u64,
) -> Result<()> {
    let parent = directory(parent)?;
    let mut file = parent.open_gc_mutation(Path::new(leaf), max)?;
    let (actual, observed) = observe(&mut file, max)?;
    if actual != bytes || !matches_stable(&observed, expected) {
        return Err(corrupt(
            "GC temporary changed before exact-handle retirement",
        ));
    }
    parent.retire_gc_handle(&file)
}

pub(super) fn remove_completed_marker(
    gc: &File,
    markers: &File,
    expected: &GcInProgressMarker,
    observation: &FileObservation,
) -> Result<()> {
    let source = directory(gc)?;
    let target = directory(markers)?;
    let mut file = source.open_gc_mutation(Path::new("in_progress"), MAX_MARKER_BYTES)?;
    let (bytes, actual) = observe(&mut file, MAX_MARKER_BYTES)?;
    if actual != *observation || GcInProgressMarker::parse_canonical(&bytes)? != *expected {
        return Err(corrupt("GC marker changed before completion archive"));
    }
    let archive = unique_internal_name("completed");
    source.move_verified_regular_handle_to_raw(&file, &target, Path::new(&archive))?;
    let (moved, actual) = observe(&mut file, MAX_MARKER_BYTES)?;
    if moved != bytes || !matches_stable(&actual, observation) {
        return Err(corrupt("GC marker changed during completion archive"));
    }
    target.retire_gc_handle(&file)
}

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
            match source.open_gc_mutation(Path::new(raw), MAX_TREE_OBJECT_BYTES) {
                Ok(file) => (file, true),
                Err(error) if is_io_not_found(&error) => {
                    match archive.open_gc_mutation(Path::new(quarantine), MAX_TREE_OBJECT_BYTES) {
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
            source.move_verified_regular_handle_to_raw(&victim, &archive, Path::new(quarantine))?;
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
        archive.retire_gc_handle(&victim)?;
        Ok(true)
    }
}

pub(super) fn move_noreplace(from_dir: &File, from: &str, to_dir: &File, to: &str) -> Result<()> {
    let source = directory(from_dir)?;
    let target = directory(to_dir)?;
    let mut file = source.open_gc_mutation(Path::new(from), MAX_MARKER_BYTES)?;
    let (bytes, before) = observe(&mut file, MAX_MARKER_BYTES)?;
    source.move_verified_regular_handle_to_raw(&file, &target, Path::new(to))?;
    let (after_bytes, after) = observe(&mut file, MAX_MARKER_BYTES)?;
    if bytes != after_bytes || !matches_stable(&after, &before) {
        return Err(corrupt("GC publication changed during exact-handle move"));
    }
    Ok(())
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
            let mut source = public.open_gc_mutation(Path::new("in_progress"), MAX_MARKER_BYTES)?;
            let (_, observed) = observe(&mut source, MAX_MARKER_BYTES)?;
            if observed != *expected {
                return Err(corrupt("GC marker changed before replacement"));
            }
            let target = prepared.open_gc_mutation(Path::new(&temporary), MAX_MARKER_BYTES)?;
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

#[allow(clippy::too_many_arguments)]
pub(super) fn publish_staged(
    from_dir: &File,
    staged: &str,
    to_dir: &File,
    leaf: &str,
    bytes: &[u8],
    max: u64,
    staged_checkpoint: impl FnOnce() -> Result<()>,
) -> Result<()> {
    use cap_fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, SYNCHRONIZE,
    };
    if bytes.len() as u64 > max {
        return Err(limit("GC publication bytes"));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ)
        .create_new(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file =
        cap_fs::open(from_dir, Path::new(staged), &options).map_err(|e| ioerr(e, staged))?;
    file.write_all(bytes).map_err(|e| ioerr(e, staged))?;
    file.sync_all().map_err(|e| ioerr(e, staged))?;
    let (written, before) = observe(&mut file, max)?;
    if written != bytes {
        return Err(corrupt("GC staged publication changed"));
    }
    staged_checkpoint()?;
    let source = directory(from_dir)?;
    let target = directory(to_dir)?;
    source.move_verified_regular_handle_to_raw(&file, &target, Path::new(leaf))?;
    let (published, after) = observe(&mut file, max)?;
    if published != bytes || !matches_stable(&after, &before) {
        return Err(corrupt("GC publication changed during exact-handle move"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent(path: &Path) -> File {
        cap_fs::open_ambient_dir(path, ambient_authority()).unwrap()
    }

    #[test]
    fn staged_publication_collision_preserves_both_files() {
        let temp = tempfile::tempdir().unwrap();
        let directory = parent(temp.path());
        std::fs::write(temp.path().join("public"), b"existing").unwrap();
        assert!(
            publish_staged(
                &directory,
                "stage",
                &directory,
                "public",
                b"replacement",
                64,
                || Ok(())
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(temp.path().join("public")).unwrap(),
            b"existing"
        );
        assert_eq!(
            std::fs::read(temp.path().join("stage")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn staged_publication_interruption_leaves_no_public_name() {
        let temp = tempfile::tempdir().unwrap();
        let directory = parent(temp.path());
        assert!(
            publish_staged(
                &directory,
                "stage",
                &directory,
                "public",
                b"record",
                64,
                || Err(corrupt("test interruption"))
            )
            .is_err()
        );
        assert!(!temp.path().join("public").exists());
        assert_eq!(std::fs::read(temp.path().join("stage")).unwrap(), b"record");
    }

    #[test]
    fn exact_retirement_rejects_substituted_identity() {
        let temp = tempfile::tempdir().unwrap();
        let directory = parent(temp.path());
        std::fs::write(temp.path().join("victim"), b"record").unwrap();
        let (bytes, expected) = read_regular_observed(&directory, "victim", 64).unwrap();
        std::fs::rename(temp.path().join("victim"), temp.path().join("original")).unwrap();
        std::fs::write(temp.path().join("victim"), &bytes).unwrap();
        assert!(retire_expected(&directory, "victim", &bytes, &expected, 64).is_err());
        assert_eq!(std::fs::read(temp.path().join("victim")).unwrap(), bytes);
        assert_eq!(std::fs::read(temp.path().join("original")).unwrap(), bytes);
    }
}
