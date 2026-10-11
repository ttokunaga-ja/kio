use super::*;

/// The explicit removal request supplies kind/hash authority for this one
/// deterministic sibling. A retry must inspect it even when canonical is absent.
/// No directory scan or legacy random-quarantine cleanup is authorized here.
pub(super) fn bound_remove_verified<F>(
    control: &crate::control::ControlHandle,
    parent: &File,
    leaf: &str,
    verify: F,
) -> Result<bool>
where
    F: Fn(&File, &str) -> Result<()>,
{
    use crate::durability::DurabilityPoint;
    use crate::store_dir::{Publication, StoreDirectory};

    let directory = StoreDirectory::from_retained_with_control(
        parent.try_clone().kio_io(Path::new(leaf))?,
        PathBuf::from("CAS removal parent"),
        control.clone(),
    )?;
    let quarantine = format!(".kio-cas-remove-{leaf}");
    let canonical_exists = directory.contains_entry(Path::new(leaf))?;
    let quarantine_exists = directory.contains_entry(Path::new(&quarantine))?;
    if canonical_exists && quarantine_exists {
        return Err(bound_stage_corrupt(
            "CAS removal canonical and quarantine both exist",
        ));
    }
    if !canonical_exists && !quarantine_exists {
        // Also close an interrupted post-unlink sync window on an absent retry.
        directory.sync()?;
        return Ok(false);
    }
    if canonical_exists {
        verify(parent, leaf)?;
        control.checkpoint(DurabilityPoint::CasRemoveReady)?;
        // Single-link, no-replace atomic rename: never a second plaintext link.
        directory.rename_regular_between_raw(
            Path::new(leaf),
            &directory,
            Path::new(&quarantine),
            Publication::CreateOnly,
        )?;
    }
    directory.sync()?;
    control.checkpoint(DurabilityPoint::CasRemoveQuarantined)?;
    // Retained-parent verifiers reject links, unstable identities, and bytes
    // not attributable to the explicitly authorized semantic CAS key.
    verify(parent, &quarantine)?;
    if directory.contains_entry(Path::new(leaf))? {
        return Err(bound_stage_corrupt("CAS removal canonical reappeared"));
    }
    bound_remove(parent, &quarantine)?;
    directory.sync()?;
    control.checkpoint(DurabilityPoint::CasRemoveDeleted)?;
    Ok(true)
}

pub(super) enum CasRemovalKind {
    Raw,
    Chunk,
    Embedding,
    Content(ContentObjectKind),
}

/// Ambient callers retain the validated parent and use exactly the same
/// namespace-safe removal and semantic recovery as bound callers.
pub(super) fn remove_verified_cas_path(
    control: &crate::control::ControlHandle,
    kio: &Path,
    path: &Path,
    kind: CasRemovalKind,
    hash: &str,
) -> Result<bool> {
    {
        let parent_path = path
            .parent()
            .ok_or_else(|| KioError::io("CAS path has no parent", path.display().to_string()))?;
        let leaf = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                KioError::io("CAS path has no UTF-8 leaf", path.display().to_string())
            })?;
        // The ambient prefix outside .kio keeps its existing OS path semantics
        // (including relative paths and macOS /var aliases). Retain .kio with
        // no-follow, then open every controlled ancestor through that handle.
        let mut options = OpenOptions::new();
        options.read(true);
        cas_semantics::configure_removal_root(&mut options);
        let mut parent = options.open(kio).kio_io(kio)?;
        ensure_bound_directory(&parent, kio)?;
        let relative_parent = parent_path
            .strip_prefix(kio)
            .map_err(|_| KioError::invalid_usage("CAS removal parent escapes .kio"))?;
        for component in relative_parent.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(KioError::invalid_usage("invalid CAS removal ancestor"));
            };
            let name = name
                .to_str()
                .ok_or_else(|| KioError::invalid_usage("invalid CAS removal ancestor"))?;
            parent = bound_open_dir(&parent, name)?;
        }
        bound_remove_verified(control, &parent, leaf, |parent, candidate| match kind {
            CasRemovalKind::Raw => bound_read_verified(
                &CasReadAccess::Unmetered,
                parent,
                candidate,
                ObjectKind::Raw,
                hash,
                false,
            )
            .map(|_| ()),
            CasRemovalKind::Chunk => {
                bound_read_chunk_accounted(&CasReadAccess::Unmetered, parent, candidate, hash)
                    .map(|_| ())
                    .map_err(|error| error.error)
            }
            CasRemovalKind::Embedding => {
                bound_read_embedding(&CasReadAccess::Unmetered, parent, candidate, hash).map(|_| ())
            }
            CasRemovalKind::Content(kind) => {
                bound_verify_content(&CasReadAccess::Unmetered, parent, candidate, kind, hash)
                    .map(|_| ())
            }
        })
    }
}
