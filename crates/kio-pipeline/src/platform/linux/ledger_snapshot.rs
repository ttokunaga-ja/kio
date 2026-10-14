pub(crate) use super::unix_ledger_snapshot::{
    Identity, PrivateStorage, filesystem_root, identity, same_identity, validate_file_links,
    validate_test_barrier_parent, verify_directory_binding, verify_private_storage,
    verify_regular_binding,
};
use crate::ledger::snapshot::{AttemptError, LedgerSnapshotError};
use cap_primitives::fs as cap_fs;
use std::{
    fs,
    path::{Component, Path, PathBuf},
};
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
                out.push(n);
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
pub(crate) fn normalize_snapshot_entry_path(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> Result<PathBuf, LedgerSnapshotError> {
    let inherited_path = inherited_ledger_root(path, control)?.is_some();
    if inherited_path {
        Ok(path.to_path_buf())
    } else {
        normalized_absolute(path)
    }
}
/// Resolve the sole inherited-descriptor spelling through the shared core
/// capability boundary.  The resolver validates the raw lexical form before
/// duplicating the descriptor, so this module never treats `/dev/fd` as an
/// ambient filesystem path.
fn inherited_ledger_root(
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> Result<
    Option<(kio_core::store_dir::StoreDirectory, Vec<std::ffi::OsString>)>,
    LedgerSnapshotError,
> {
    kio_core::private_fs::resolve_inherited_private_root_with_control(path, control).map_err(
        |error| {
            LedgerSnapshotError::UnsafeIntegrity(format!(
                "resolve inherited ledger directory capability: {error}"
            ))
        },
    )
}
/// Bind an inherited directory capability and traverse only the resolver's
/// validated suffix with no-follow operations.
fn bound_inherited_parent(
    path: &Path,
    control: &kio_core::control::ControlHandle,
    #[cfg(test)] hooks: Option<&crate::ledger::snapshot::SnapshotTestHooks>,
) -> Result<Option<(fs::File, String, PathBuf)>, LedgerSnapshotError> {
    let Some((root, suffix)) = inherited_ledger_root(path, control)? else {
        return Ok(None);
    };
    bound_inherited_parent_from_root(
        path,
        root,
        &suffix,
        #[cfg(test)]
        hooks,
    )
    .map(Some)
}
/// Traverse a validated ledger suffix from an already-retained inherited root.
fn bound_inherited_parent_from_root(
    path: &Path,
    root: kio_core::store_dir::StoreDirectory,
    suffix: &[std::ffi::OsString],
    #[cfg(test)] hooks: Option<&crate::ledger::snapshot::SnapshotTestHooks>,
) -> Result<(fs::File, String, PathBuf), LedgerSnapshotError> {
    let (leaf, parent_suffix) = suffix.split_last().ok_or_else(|| {
        LedgerSnapshotError::UnsafeIntegrity("inherited ledger path has no file name".into())
    })?;
    let leaf = leaf
        .to_str()
        .ok_or_else(|| LedgerSnapshotError::UnsafeIntegrity("ledger file name is invalid".into()))?
        .to_owned();
    let parent_path = path
        .parent()
        .ok_or_else(|| LedgerSnapshotError::UnsafeIntegrity("ledger has no parent".into()))?
        .to_owned();
    let mut dir = root.root_handle().try_clone().map_err(|error| {
        LedgerSnapshotError::UnstableBusy(format!("clone inherited ledger root: {error}"))
    })?;

    for name in parent_suffix {
        let label = name.to_string_lossy();
        let before =
            cap_fs::stat(&dir, Path::new(name), cap_fs::FollowSymlinks::No).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    #[cfg(test)]
                    run_missing_parent_hook(hooks);
                    LedgerSnapshotError::Missing
                } else {
                    LedgerSnapshotError::UnstableBusy(format!(
                        "stat inherited ledger parent: {error}"
                    ))
                }
            })?;
        validate_dir(&before, &label)?;
        let child = cap_fs::open_dir_nofollow(&dir, Path::new(name)).map_err(|error| {
            LedgerSnapshotError::UnsafeIntegrity(format!(
                "open inherited ledger parent nofollow: {error}"
            ))
        })?;
        let opened = cap_fs::Metadata::from_file(&child).map_err(|error| {
            LedgerSnapshotError::UnstableBusy(format!("inspect inherited ledger parent: {error}"))
        })?;
        let after =
            cap_fs::stat(&dir, Path::new(name), cap_fs::FollowSymlinks::No).map_err(|error| {
                LedgerSnapshotError::UnsafeIntegrity(format!(
                    "re-stat inherited ledger parent: {error}"
                ))
            })?;
        validate_dir(&opened, &label)?;
        validate_dir(&after, &label)?;
        if !same_identity(&before, &opened) || !same_identity(&before, &after) {
            return Err(LedgerSnapshotError::UnsafeIntegrity(
                "inherited ledger parent changed while opening".into(),
            ));
        }
        dir = child;
    }
    Ok((dir, leaf, parent_path))
}
/// Confirm an inherited-path absence against one retained descriptor root,
/// then prove that the raw descriptor still names that root just before
/// accepting `Missing`. Re-resolving `/dev/fd/N` for both absence walks would
/// otherwise join observations from two unrelated authority roots.
fn stable_inherited_parent_binding(
    path: &Path,
    control: &kio_core::control::ControlHandle,
    #[cfg(test)] hooks: Option<&crate::ledger::snapshot::SnapshotTestHooks>,
) -> Result<Option<(fs::File, String, PathBuf)>, AttemptError> {
    let (root, suffix) = inherited_ledger_root(path, control)
        .map_err(parent_binding_error)?
        .ok_or_else(|| AttemptError::Unsafe("inherited ledger root disappeared".into()))?;
    match bound_inherited_parent_from_root(
        path,
        root.clone(),
        &suffix,
        #[cfg(test)]
        hooks,
    ) {
        Ok(binding) => Ok(Some(binding)),
        Err(LedgerSnapshotError::Missing) => {
            match bound_inherited_parent_from_root(
                path,
                root.clone(),
                &suffix,
                #[cfg(test)]
                hooks,
            ) {
                Ok(_) => Err(AttemptError::Unstable(
                    "ledger parent appeared while confirming its absence".into(),
                )),
                Err(LedgerSnapshotError::Missing) => {
                    let (fresh_root, fresh_suffix) = inherited_ledger_root(path, control)
                        .map_err(parent_binding_error)?
                        .ok_or_else(|| {
                            AttemptError::Unstable(
                                "inherited ledger root disappeared while confirming absence".into(),
                            )
                        })?;
                    if fresh_suffix != suffix {
                        return Err(AttemptError::Unsafe(
                            "inherited ledger suffix changed while confirming absence".into(),
                        ));
                    }
                    require_same_inherited_root(&root, &fresh_root, "while confirming absence")?;
                    match bound_inherited_parent_from_root(
                        path,
                        fresh_root,
                        &suffix,
                        #[cfg(test)]
                        hooks,
                    ) {
                        Err(LedgerSnapshotError::Missing) => {
                            let (post_walk_root, post_walk_suffix) =
                                inherited_ledger_root(path, control)
                                    .map_err(parent_binding_error)?
                                    .ok_or_else(|| {
                                        AttemptError::Unstable(
                                        "inherited ledger root disappeared after final absence walk"
                                            .into(),
                                    )
                                    })?;
                            if post_walk_suffix != suffix {
                                return Err(AttemptError::Unsafe(
                                    "inherited ledger suffix changed after final absence walk"
                                        .into(),
                                ));
                            }
                            require_same_inherited_root(
                                &root,
                                &post_walk_root,
                                "after final absence walk",
                            )?;
                            Ok(None)
                        }
                        Ok(_) => Err(AttemptError::Unstable(
                            "ledger parent appeared while accepting inherited absence".into(),
                        )),
                        Err(error) => Err(parent_binding_error(error)),
                    }
                }
                Err(error) => Err(parent_binding_error(error)),
            }
        }
        Err(error) => Err(parent_binding_error(error)),
    }
}
fn require_same_inherited_root(
    held: &kio_core::store_dir::StoreDirectory,
    fresh: &kio_core::store_dir::StoreDirectory,
    phase: &str,
) -> Result<(), AttemptError> {
    let held_metadata =
        cap_fs::Metadata::from_file(held.root_handle().as_ref()).map_err(|error| {
            AttemptError::Unstable(format!("inspect retained inherited ledger root: {error}"))
        })?;
    let fresh_metadata =
        cap_fs::Metadata::from_file(fresh.root_handle().as_ref()).map_err(|error| {
            AttemptError::Unstable(format!("inspect fresh inherited ledger root: {error}"))
        })?;
    if !same_identity(&held_metadata, &fresh_metadata) {
        return Err(AttemptError::Unsafe(format!(
            "inherited ledger root identity changed {phase}"
        )));
    }
    Ok(())
}
pub(crate) fn bound_parent(
    path: &Path,
    control: &kio_core::control::ControlHandle,
    #[cfg(test)] hooks: Option<&crate::ledger::snapshot::SnapshotTestHooks>,
) -> Result<(fs::File, String, PathBuf), LedgerSnapshotError> {
    if let Some(binding) = bound_inherited_parent(
        path,
        control,
        #[cfg(test)]
        hooks,
    )? {
        return Ok(binding);
    }
    crate::ledger::snapshot::bound_ambient_parent(
        path,
        control,
        #[cfg(test)]
        hooks,
    )
}
pub(crate) fn stable_parent_binding(
    path: &Path,
    control: &kio_core::control::ControlHandle,
    #[cfg(test)] hooks: Option<&crate::ledger::snapshot::SnapshotTestHooks>,
) -> Result<Option<(fs::File, String, PathBuf)>, AttemptError> {
    if inherited_ledger_root(path, control)
        .map_err(parent_binding_error)?
        .is_some()
    {
        return stable_inherited_parent_binding(
            path,
            control,
            #[cfg(test)]
            hooks,
        );
    }
    crate::ledger::snapshot::stable_ambient_parent_binding(
        path,
        control,
        #[cfg(test)]
        hooks,
    )
}
#[cfg(test)]
use crate::ledger::snapshot::run_missing_parent_hook;
use crate::ledger::snapshot::{parent_binding_error, validate_dir};
