//! GC exchange journal protocol, including read-only foreign pending inspection.
use crate::store_dir::{AtomicWorkspaceState, StoreDirectory};
use crate::{ExitCode, KioError, Result};
use std::path::Path;

pub(crate) const INTENT: &str = "intent.json";
pub(crate) const BACKUP: &str = "backup";

pub(crate) fn failure(message: impl Into<String>) -> KioError {
    KioError::new(
        "KIO-E-GC-EXCHANGE-001",
        message,
        serde_json::json!({}),
        ExitCode::PermanentFailure,
    )
}

/// Inspect an existing exchange owner without recovering or creating anything.
/// Foreign-platform pending journals are deliberately reported as pending.
pub fn inspect_pending(owner: &StoreDirectory) -> Result<bool> {
    let mut pending = false;
    let handle = owner.root_handle();
    let entries = cap_primitives::fs::read_dir(&handle, Path::new("."))
        .map_err(|e| failure(e.to_string()))?;
    for (index, entry) in entries.enumerate() {
        if index >= 3 {
            return Err(failure("GC exchange owner exceeds its entry bound"));
        }
        let name = entry.map_err(|e| failure(e.to_string()))?.file_name();
        match name.to_str() {
            Some(INTENT | BACKUP) => {
                // Reopen only through the retained owner, rejecting links/reparse points.
                owner.open_regular_read(Path::new(&name), u64::MAX)?;
                pending = true;
            }
            Some(crate::store_dir::ATOMIC_WORKSPACE_DIR) => {
                owner.open_directory(Path::new(&name))?;
            }
            _ => return Err(failure("unexpected GC exchange owner entry")),
        }
    }
    Ok(pending || owner.inspect_atomic()? == AtomicWorkspaceState::Pending)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Marker,
    Index,
    SnapshotState,
}

pub fn validate_leaves(kind: OperationKind, public: &str, prepared: &str) -> Result<()> {
    let (expected, prefix) = match kind {
        OperationKind::Marker => ("in_progress", ".gc-retired-marker-"),
        OperationKind::Index => ("sqlite.db", ".gc-index-"),
        OperationKind::SnapshotState => ("snapshot-auto.json", ".snapshot-auto-state-"),
    };
    if public != expected
        || !prepared.starts_with(prefix)
        || prepared.len() <= prefix.len()
        || prepared.len() > 128
        || !prepared
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(failure("invalid kind-specific exchange leaves"));
    }
    if kind == OperationKind::SnapshotState
        && !super::is_snapshot_auto_state_temporary_name(prepared)
    {
        return Err(failure("invalid snapshot state prepared suffix"));
    }
    if kind == OperationKind::Marker {
        let suffix = &prepared[prefix.len()..];
        let Some((pid, nanos)) = suffix.split_once('-') else {
            return Err(failure("invalid marker prepared suffix"));
        };
        if pid.is_empty()
            || nanos.is_empty()
            || !pid.bytes().all(|b| b.is_ascii_digit())
            || !nanos.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(failure("invalid marker prepared suffix"));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Initial,
    Captured,
    Published,
    Complete,
    Retired,
}
pub fn placement(
    public: Option<bool>,
    prepared: Option<bool>,
    backup: Option<bool>,
) -> Result<Placement> {
    // false = old source, true = replacement target.
    match (public, prepared, backup) {
        (Some(false), Some(true), None) => Ok(Placement::Initial),
        (None, Some(true), Some(false)) => Ok(Placement::Captured),
        (Some(true), None, Some(false)) => Ok(Placement::Published),
        (Some(true), Some(false), None) => Ok(Placement::Complete),
        (Some(true), None, None) => Ok(Placement::Retired),
        _ => Err(failure("GC exchange has an inadmissible file placement")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copied_pending_and_unknown_residue_are_never_treated_as_clean() {
        let temp = tempfile::tempdir().unwrap();
        let owner = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
        assert!(!inspect_pending(&owner).unwrap());
        std::fs::write(temp.path().join(INTENT), b"foreign Windows intent").unwrap();
        assert!(inspect_pending(&owner).unwrap());
        assert_eq!(
            std::fs::read(temp.path().join(INTENT)).unwrap(),
            b"foreign Windows intent"
        );
        std::fs::write(temp.path().join("foreign"), b"preserve").unwrap();
        assert!(inspect_pending(&owner).is_err());
        assert_eq!(
            std::fs::read(temp.path().join("foreign")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn only_five_of_twenty_seven_placements_are_admissible() {
        let mut accepted = 0;
        for public in [None, Some(false), Some(true)] {
            for prepared in [None, Some(false), Some(true)] {
                for backup in [None, Some(false), Some(true)] {
                    accepted += usize::from(placement(public, prepared, backup).is_ok());
                }
            }
        }
        assert_eq!(accepted, 5);
    }
    #[test]
    fn kind_specific_leaves_reject_aliases() {
        assert!(
            validate_leaves(
                OperationKind::Marker,
                "in_progress",
                ".gc-retired-marker-12-12345"
            )
            .is_ok()
        );
        assert!(validate_leaves(OperationKind::Index, "sqlite.db", ".gc-index-rotation").is_ok());
        for leaf in [
            "../x",
            ".gc-index-x:y",
            ".gc-index-",
            ".gc-index-x/../y",
            ".gc-index-ä",
        ] {
            assert!(validate_leaves(OperationKind::Index, "sqlite.db", leaf).is_err());
        }
        assert!(
            validate_leaves(
                OperationKind::Marker,
                "sqlite.db",
                ".gc-retired-marker-12-34"
            )
            .is_err()
        );
    }
}
