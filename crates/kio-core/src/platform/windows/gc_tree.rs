use super::gc_io::{directory, matches_stable};
use crate::cas::CasReadAccess;
use crate::error::Result;
use crate::gc::{FileObservation, corrupt, is_io_not_found, observe_cas_regular};
use std::path::Path;
#[allow(clippy::too_many_arguments)]
pub(crate) fn retire_candidate_tree(
    record_stages: &crate::disk_records::RecordStageRoot,
    tree_hash: &str,
    expected_size: u64,
    raw: &str,
    canonical_parent: &std::fs::File,
    archive_parent: &std::fs::File,
    quarantine: &str,
    _captured_name: &str,
    validate_observation: impl Fn(&FileObservation) -> Result<()>,
    require_marker: impl Fn() -> Result<()>,
    before_quarantine: impl Fn() -> Result<()>,
    quarantine_checkpoint: impl Fn() -> Result<()>,
    before_erase: impl Fn() -> Result<()>,
    retirement_checkpoint: impl Fn() -> Result<()>,
    control: &crate::control::ControlHandle,
) -> Result<bool> {
    let source = directory(canonical_parent, control)?;
    let archive = directory(archive_parent, control)?;
    let (victim, needs_move) =
        match crate::platform::open_gc_mutation(&source, Path::new(raw), expected_size.max(64)) {
            Ok(file) => (file, true),
            Err(error) if is_io_not_found(&error) => {
                match crate::platform::open_gc_mutation(
                    &archive,
                    Path::new(quarantine),
                    expected_size.max(64),
                ) {
                    Ok(file) => (file, false),
                    // Disposition already removed every name before interruption.
                    Err(error) if is_io_not_found(&error) => return Ok(false),
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
    let (_, before) = observe_tree_handle(record_stages, &victim, expected_size.max(64))?;
    validate_observation(&before)?;
    if needs_move {
        // This checks any conflicting archive, including unsafe entries;
        // the no-replace move is the final collision authority.
        match observe_cas_regular(
            &CasReadAccess::Unmetered,
            archive_parent,
            quarantine,
            expected_size.max(64),
        ) {
            Ok(_) => return Err(corrupt("GC tree exists at canonical and archive paths")),
            Err(error) if is_io_not_found(&error) => {}
            Err(error) => return Err(error),
        }
        require_marker()?;
        before_quarantine()?;

        crate::platform::move_verified_regular_handle_to_raw(
            &source,
            &victim,
            &archive,
            Path::new(quarantine),
        )?;
    }
    let (_, observed) = observe_tree_handle(record_stages, &victim, expected_size.max(64))?;
    if !matches_stable(&observed, &before) || observed.digest != tree_hash {
        return Err(corrupt("tree changed during GC quarantine"));
    }
    quarantine_checkpoint()?;
    before_erase()?;
    let (_, final_observation) =
        observe_tree_handle(record_stages, &victim, expected_size.max(64))?;
    if !matches_stable(&final_observation, &before) {
        return Err(corrupt("tree changed immediately before GC erase"));
    }
    retirement_checkpoint()?;
    crate::platform::retire_gc_handle(&archive, &victim)?;
    Ok(true)
}

fn observe_tree_handle(
    root: &crate::disk_records::RecordStageRoot,
    file: &std::fs::File,
    max: u64,
) -> Result<(crate::dag::TreeObject, FileObservation)> {
    use crate::gc::observed_read::stream_handle;

    let (tree, observation) = stream_handle(Some(root), &CasReadAccess::Unmetered, file, max)?;
    Ok((
        tree.ok_or_else(|| corrupt("invalid tree object"))?,
        observation,
    ))
}
