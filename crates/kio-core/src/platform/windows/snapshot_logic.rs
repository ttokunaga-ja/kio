use crate::Result;
use crate::gc::{SnapshotAutoState, corrupt};
use crate::scope::parse_utc_seconds;

pub(crate) fn validate_snapshot_state_transition(
    old: &SnapshotAutoState,
    new: &SnapshotAutoState,
) -> Result<()> {
    let old_attempt = parse_utc_seconds(&old.last_successful_eligible_attempt_at)
        .ok_or_else(|| corrupt("invalid old snapshot attempt"))?;
    let old_idle = parse_utc_seconds(&old.idle_observed_since)
        .ok_or_else(|| corrupt("invalid old snapshot idle time"))?;
    let new_attempt = parse_utc_seconds(&new.last_successful_eligible_attempt_at)
        .ok_or_else(|| corrupt("invalid new snapshot attempt"))?;
    let new_idle = parse_utc_seconds(&new.idle_observed_since)
        .ok_or_else(|| corrupt("invalid new snapshot idle time"))?;
    let valid = if old.working_set_digest == new.working_set_digest {
        new_idle == old_idle
            && (new_attempt == old_attempt || new_attempt >= old_attempt.max(old_idle))
    } else {
        new_idle >= old_attempt.max(old_idle)
            && (new_attempt == old_attempt || new_attempt == new_idle)
    };
    if !valid {
        return Err(corrupt("invalid snapshot auto checkpoint transition"));
    }
    Ok(())
}
