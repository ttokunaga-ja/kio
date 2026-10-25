use super::gc_snapshot_io::{directory, observe, open_snapshot_source};
use crate::error::Result;
use crate::gc::exchange::{self, OperationKind};
use crate::gc::{
    FileObservation, MAX_METADATA, SNAPSHOT_AUTO_STATE_LEAF, SnapshotExchangeAuthority, corrupt,
    ensure_child_dir, has_pending_snapshot_state_exchange, is_snapshot_auto_state_temporary_name,
    open_optional_dir, parse_snapshot_auto_state, read_regular_observed,
    snapshot_auto_authority_changed, snapshot_auto_state_changed,
};
use crate::platform::{
    ExchangeExpected, gc_retire_snapshot_state_temporary as retire_snapshot_state_temporary,
};
use std::fs::File;
use std::path::Path;
fn validate_handles(source: Option<&File>, target: &File) -> Result<()> {
    let (new, _) = observe(target)?;
    if let Some(source) = source {
        crate::platform::gc_snapshot_logic::validate_snapshot_state_transition(
            &observe(source)?.0,
            &new,
        )?;
    }
    Ok(())
}
pub(crate) fn replace_snapshot_state<D, R>(
    kio: &File,
    control: &crate::control::ControlHandle,
    temporary: &str,
    bytes: &[u8],
    expected: &FileObservation,
    temporary_observation: &FileObservation,
    derive: D,
) -> Result<()>
where
    D: FnOnce() -> Result<(SnapshotExchangeAuthority, R)>,
    R: Fn() -> Result<SnapshotExchangeAuthority>,
{
    if !is_snapshot_auto_state_temporary_name(temporary) {
        return Err(corrupt("invalid snapshot state temporary"));
    }
    let (binding, recheck) = derive()?;
    let gc = ensure_child_dir(kio, "gc")?;
    let internal = ensure_child_dir(&gc, "internal")?;
    let owner = directory(
        &ensure_child_dir(&internal, "snapshot-state-exchange")?,
        control,
    )?;
    let parent = directory(kio, control)?;
    let target_state = parse_snapshot_auto_state(bytes)?;
    let authority = {
        let source = match open_snapshot_source(kio, expected, control).and_then(|source| {
            if observe(&source)?.1 != *expected {
                return Err(snapshot_auto_state_changed());
            }
            Ok(source)
        }) {
            Ok(source) => source,
            Err(error) => {
                // Only this pre-journal CAS conflict may retire our prepared
                // target. Later failures leave journal-owned recovery state.
                // Unknown or pending owner state never authorizes cleanup.
                if error.error_code() == "KIO-E-SNAPSHOT-STATE-CHANGED-001"
                    && !exchange::inspect_pending(&owner)?
                {
                    retire_snapshot_state_temporary(
                        kio,
                        temporary,
                        bytes,
                        temporary_observation,
                        control,
                    )?;
                }
                return Err(error);
            }
        };
        let target =
            crate::platform::open_gc_mutation(&parent, Path::new(temporary), MAX_METADATA)?;
        if observe(&target)?.0 != target_state {
            return Err(snapshot_auto_state_changed());
        }
        ExchangeExpected {
            source: crate::platform::observe_gc_exchange(&source, MAX_METADATA)?,
            target: crate::platform::observe_gc_exchange(&target, MAX_METADATA)?,
        }
    };
    let pending = crate::platform::begin_gc_exchange(
        &owner,
        &parent,
        SNAPSHOT_AUTO_STATE_LEAF,
        &parent,
        temporary,
        OperationKind::SnapshotState,
        &authority,
        MAX_METADATA,
        &binding.binding,
        |source, target| {
            if observe(source)?.1 != *expected || observe(target)?.0 != target_state {
                return Err(snapshot_auto_state_changed());
            }
            if recheck()? != binding {
                return Err(snapshot_auto_authority_changed());
            }
            validate_handles(Some(source), target)
        },
    )?;
    pending.complete(&authority, |_| Ok(()))?;
    if recheck()? != binding {
        return Err(snapshot_auto_authority_changed());
    }
    Ok(())
}
pub(crate) fn recover_snapshot_state(
    kio: &File,
    control: &crate::control::ControlHandle,
    derive: impl Fn() -> Result<SnapshotExchangeAuthority>,
    admit: impl Fn() -> Result<()>,
) -> Result<()> {
    if !has_pending_snapshot_state_exchange(kio, control)? {
        return Ok(());
    }
    let binding = derive()?;
    let gc = open_optional_dir(kio, "gc")?
        .ok_or_else(|| corrupt("missing snapshot exchange GC parent"))?;
    let internal = open_optional_dir(&gc, "internal")?
        .ok_or_else(|| corrupt("missing snapshot exchange internal parent"))?;
    let owner_file = open_optional_dir(&internal, "snapshot-state-exchange")?
        .ok_or_else(|| corrupt("missing snapshot exchange owner"))?;
    let owner = directory(&owner_file, control)?;
    let parent = directory(kio, control)?;
    crate::platform::validate_pending_gc_exchange_authority(&owner, &binding.binding)?;
    // Atomic-only residue never explains an absent checkpoint: it represents
    // either unpublished intent boot or cleanup after exact source retirement.
    let stable_public = if !owner.contains_entry(Path::new("intent.json"))? {
        let (bytes, observation) =
            read_regular_observed(kio, SNAPSHOT_AUTO_STATE_LEAF, MAX_METADATA)?;
        parse_snapshot_auto_state(&bytes)?;
        Some(observation)
    } else {
        None
    };
    if let Some(pending) = crate::platform::open_pending_gc_exchange_under_lock(
        &owner,
        &parent,
        &parent,
        OperationKind::SnapshotState,
        MAX_METADATA,
        &binding.binding,
    )? {
        validate_handles(pending.source(), pending.target())?;
        if derive()? != binding {
            return Err(snapshot_auto_authority_changed());
        }
        let expected = ExchangeExpected {
            source: pending.source_record().clone(),
            target: pending.target_record().clone(),
        };
        pending.complete(&expected, |_| Ok(()))?;
    } else if let Some(expected) = stable_public {
        if read_regular_observed(kio, SNAPSHOT_AUTO_STATE_LEAF, MAX_METADATA)?.1 != expected {
            return Err(snapshot_auto_state_changed());
        }
    } else {
        return Err(corrupt(
            "snapshot exchange disappeared without terminal authority",
        ));
    }
    if derive()? != binding {
        return Err(snapshot_auto_authority_changed());
    }
    admit()?;
    Ok(())
}
