pub(crate) use super::unix_ledger_snapshot::{
    Identity, PrivateStorage, filesystem_root, identity, same_identity, validate_file_links,
    validate_test_barrier_parent, verify_directory_binding, verify_private_storage,
    verify_regular_binding,
};
use crate::ledger::snapshot::LedgerSnapshotError;
use std::path::{Component, Path, PathBuf};
pub(crate) fn normalized_absolute(path: &Path) -> Result<PathBuf, LedgerSnapshotError> {
    if !path.is_absolute() {
        return Err(LedgerSnapshotError::UnsafeIntegrity(format!(
            "ledger path must be absolute: {}",
            path.display()
        )));
    }

    let mut out = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::RootDir => {}
            Component::Normal(n) => {
                if out == Path::new("/") && n == "var" {
                    out.push("private");
                    out.push("var");
                } else {
                    out.push(n);
                }
            }
            _ => {
                return Err(LedgerSnapshotError::UnsafeIntegrity(format!(
                    "ledger path is not normalized: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(out)
}
pub(crate) fn normalize_snapshot_entry_path(path: &Path) -> Result<PathBuf, LedgerSnapshotError> {
    normalized_absolute(path)
}
pub(crate) use crate::ledger::snapshot::{
    bound_ambient_parent as bound_parent, stable_ambient_parent_binding as stable_parent_binding,
};
