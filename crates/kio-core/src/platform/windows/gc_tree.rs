use super::gc_io::{directory, matches_stable, observe};
use crate::cas::{MAX_TREE_OBJECT_BYTES, hash_bytes};
use crate::error::Result;
use crate::gc::{corrupt, is_io_not_found, read_regular_observed};
use std::path::Path;
#[allow(clippy::too_many_arguments)]
pub(crate) fn retire_candidate_tree(
    tree_hash: &str,
    raw: &str,
    canonical_parent: &std::fs::File,
    archive_parent: &std::fs::File,
    quarantine: &str,
    _captured_name: &str,
    validate_bytes: impl Fn(&[u8]) -> Result<()>,
    require_marker: impl Fn() -> Result<()>,
    before_quarantine: impl Fn() -> Result<()>,
    quarantine_checkpoint: impl Fn() -> Result<()>,
    before_erase: impl Fn() -> Result<()>,
    retirement_checkpoint: impl Fn() -> Result<()>,
    control: &crate::control::ControlHandle,
) -> Result<bool> {
    let source = directory(canonical_parent, control)?;
    let archive = directory(archive_parent, control)?;
    let (mut victim, needs_move) =
        match crate::platform::open_gc_mutation(&source, Path::new(raw), MAX_TREE_OBJECT_BYTES) {
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
    validate_bytes(&bytes)?;
    if needs_move {
        // This checks any conflicting archive, including unsafe entries;
        // the no-replace move is the final collision authority.
        match read_regular_observed(archive_parent, quarantine, MAX_TREE_OBJECT_BYTES) {
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
    let (moved, observed) = observe(&mut victim, MAX_TREE_OBJECT_BYTES)?;
    if moved != bytes || !matches_stable(&observed, &before) || hash_bytes(&moved) != tree_hash {
        return Err(corrupt("tree changed during GC quarantine"));
    }
    quarantine_checkpoint()?;
    before_erase()?;
    let (final_bytes, final_observation) = observe(&mut victim, MAX_TREE_OBJECT_BYTES)?;
    if final_bytes != bytes || !matches_stable(&final_observation, &before) {
        return Err(corrupt("tree changed immediately before GC erase"));
    }
    retirement_checkpoint()?;
    crate::platform::retire_gc_handle(&archive, &victim)?;
    Ok(true)
}
