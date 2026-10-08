use super::gc_io::{directory, observe, retire_expected};
use crate::cas::{MAX_TREE_OBJECT_BYTES, hash_bytes};
use crate::error::Result;
use crate::gc::exchange;
use crate::gc::{
    FileObservation, GcInProgressMarker, MAX_MARKER_BYTES, MAX_SNAPSHOT_AUTO_STATE_TEMPORARIES,
    corrupt, create_new_bound, ensure_child_dir, has_pending_marker_exchange, ioerr,
    is_io_not_found, limit, open_optional_dir, read_regular_observed, unique_internal_name,
};
use cap_primitives::fs as cap_fs;
use std::fs::File;
use std::path::{Path, PathBuf};
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

pub(crate) fn exchange_marker(
    gc: &File,
    internal: &File,
    markers: &File,
    marker: &GcInProgressMarker,
    expected: &FileObservation,
    validate_marker: impl Fn(Option<GcInProgressMarker>, GcInProgressMarker) -> Result<()>,
) -> Result<()> {
    use crate::gc::exchange::OperationKind;
    use crate::platform::ExchangeExpected;
    let owner_file = ensure_child_dir(internal, "marker-exchange")?;
    let owner = directory(&owner_file)?;
    if exchange::inspect_pending(&owner)? {
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
        let mut source =
            crate::platform::open_gc_mutation(&public, Path::new("in_progress"), MAX_MARKER_BYTES)?;
        let (_, observed) = observe(&mut source, MAX_MARKER_BYTES)?;
        if observed != *expected {
            return Err(corrupt("GC marker changed before replacement"));
        }
        let target =
            crate::platform::open_gc_mutation(&prepared, Path::new(&temporary), MAX_MARKER_BYTES)?;
        if marker_from_handle(&target)? != *marker {
            return Err(corrupt("GC prepared marker changed before exchange"));
        }
        ExchangeExpected {
            source: crate::platform::observe_gc_exchange(&source, MAX_MARKER_BYTES)?,
            target: crate::platform::observe_gc_exchange(&target, MAX_MARKER_BYTES)?,
        }
    };
    let pending = crate::platform::begin_gc_exchange(
        &owner,
        &public,
        "in_progress",
        &prepared,
        &temporary,
        OperationKind::Marker,
        &authority,
        MAX_MARKER_BYTES,
        &marker_binding(),
        |source, target| validate_marker_handles(Some(source), target, &validate_marker),
    )?;
    pending.complete(&authority, |_| Ok(()))?;
    Ok(())
}
pub(crate) fn recover_marker(
    kio: &File,
    validate_marker: impl Fn(Option<GcInProgressMarker>, GcInProgressMarker) -> Result<()>,
) -> Result<()> {
    use crate::gc::exchange::OperationKind;
    use crate::platform::ExchangeExpected;
    let Some(gc) = open_optional_dir(kio, "gc")? else {
        return Ok(());
    };
    let Some(internal) = open_optional_dir(&gc, "internal")? else {
        return Ok(());
    };
    let Some(markers) = open_optional_dir(&internal, "markers")? else {
        if has_pending_marker_exchange(kio)? {
            return Err(corrupt("pending marker exchange has no marker directory"));
        }
        return Ok(());
    };
    if let Some(owner_file) = open_optional_dir(&internal, "marker-exchange")? {
        let owner = directory(&owner_file)?;
        let public = directory(&gc)?;
        let prepared = directory(&markers)?;
        if let Some(pending) = crate::platform::open_pending_gc_exchange_under_lock(
            &owner,
            &public,
            &prepared,
            OperationKind::Marker,
            MAX_MARKER_BYTES,
            &marker_binding(),
        )? {
            validate_marker_handles(pending.source(), pending.target(), &validate_marker)?;
            let authority = ExchangeExpected {
                source: pending.source_record().clone(),
                target: pending.target_record().clone(),
            };
            pending.complete(&authority, |_| Ok(()))?;
        }
    }
    cleanup_marker_temporaries(&markers)
}
pub(crate) fn marker_archive_presence(
    archives: &File,
    archive: &str,
    _captured_name: &str,
    validate_bytes: impl Fn(&[u8]) -> Result<()>,
) -> Result<(bool, bool)> {
    Ok(
        match read_regular_observed(archives, archive, MAX_TREE_OBJECT_BYTES) {
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
    path.to_path_buf()
}
fn validate_marker_handles(
    source: Option<&File>,
    target: &File,
    validate_marker: &impl Fn(Option<GcInProgressMarker>, GcInProgressMarker) -> Result<()>,
) -> Result<()> {
    let target = marker_from_handle(target)?;
    let source = source.map(marker_from_handle).transpose()?;
    validate_marker(source, target)
}
