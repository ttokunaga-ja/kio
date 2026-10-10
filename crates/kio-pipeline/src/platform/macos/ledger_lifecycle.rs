pub(crate) use super::unix_ledger_lifecycle::*;
use crate::Result;
use crate::ledger::lifecycle_fs::contract;
use kio_core::store_dir::StoreDirectory;
use std::path::{Component, Path, PathBuf};
pub(crate) fn try_inherited_parent(
    _path: &Path,
    _allow_create: bool,
    _control: kio_core::control::ControlHandle,
) -> Result<Option<StoreDirectory>> {
    Ok(None)
}
pub(crate) fn sqlite_path_from_retained_parent(
    _parent: &StoreDirectory,
    _leaf: &Path,
) -> Result<PathBuf> {
    Err(contract(
        "KIO-E-LEDGER-PRIVATE-001",
        "inherited ledger roots are unsupported on this platform",
    ))
}

pub(crate) fn normalized_absolute_db_path(path: &Path) -> Result<PathBuf> {
    {
        if !path.is_absolute() {
            return Err(contract(
                "KIO-E-LEDGER-PATH-001",
                "ledger path must be absolute",
            ));
        }
        let mut normalized = PathBuf::from("/");
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    if normalized == Path::new("/") && name == "var" {
                        normalized.push("private");
                        normalized.push("var");
                    } else {
                        normalized.push(name);
                    }
                }
                _ => {
                    return Err(contract(
                        "KIO-E-LEDGER-PATH-001",
                        "ledger path is not normalized",
                    ));
                }
            }
        }
        Ok(normalized)
    }
}
