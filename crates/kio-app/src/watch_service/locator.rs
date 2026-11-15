//! Registration storage remains anchored while watcher environment changes.
use super::*;

pub(super) fn installation_locator(
    id: &str,
    environment: &ServiceEnvironment,
    native: impl FnOnce(&str) -> Result<Option<PathBuf>>,
) -> Result<Option<PathBuf>> {
    let registered = native(id)?
        .map(|path| {
            if path.exists() {
                Ok(path)
            } else {
                read_locator(&path.with_extension("locator.json"))
                    .map(|saved| saved.unwrap_or(path))
            }
        })
        .transpose()?;
    let current = manifest_locator(id, environment)?;
    let saved = read_locator(&current.with_extension("locator.json"))?;
    let local = saved
        .filter(|path| path.exists())
        .or_else(|| current.exists().then_some(current));
    if let Some(registered) = registered {
        if let Some(local) = local
            && !same_location(&registered, &local)
        {
            return Err(service_error(
                "registered and local service manifests have different anchors",
            ));
        }
        return Ok(Some(registered));
    }
    // An approved update publishes this locator before unregistering anything.
    // It retains the same lock/intent when the OS registration is temporarily
    // absent; it is not a second copy of the canonical manifest.
    Ok(local)
}

fn same_location(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

pub(super) fn read_locator(path: &Path) -> Result<Option<PathBuf>> {
    let Some(parent) = path.parent() else {
        return Err(service_error("invalid service locator"));
    };
    if !parent.exists() {
        return Ok(None);
    }
    let directory = kio_core::private_fs::verify_private_creation_parent(parent)?;
    let leaf = Path::new(
        path.file_name()
            .ok_or_else(|| service_error("invalid service locator"))?,
    );
    if !directory.contains_entry(leaf)? {
        return Ok(None);
    }
    let name = leaf
        .to_str()
        .ok_or_else(|| service_error("non-UTF-8 manifest locator"))?;
    let bytes = kio_core::private_fs::read_private_file_at(&directory, name, MAX_MANIFEST_BYTES)?;
    let target: PathBuf = serde_json::from_slice(&bytes).map_err(service_error)?;
    validate_argument_path(&target)?;
    if target == path {
        return Err(service_error("recursive service locator"));
    }
    Ok(Some(target))
}

pub(super) fn load_installation(
    path: &Path,
    control: &ControlHandle,
) -> Result<(StoreDirectory, PathBuf, ServiceManifest)> {
    validate_argument_path(path)?;
    let parent = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| service_error("invalid manifest locator"))?;
    let store = kio_core::private_fs::verify_private_creation_parent_with_control(parent, control)?;
    let leaf = path
        .strip_prefix(parent)
        .map_err(service_error)?
        .to_path_buf();
    let directory = StoreDirectory::from_retained_with_control(
        store.open_directory(Path::new(MANIFEST_DIRECTORY))?,
        parent.join(MANIFEST_DIRECTORY),
        control.clone(),
    )?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| service_error("invalid manifest name"))?;
    let bytes = kio_core::private_fs::read_private_file_at(&directory, name, MAX_MANIFEST_BYTES)?;
    let manifest: ServiceManifest = serde_json::from_slice(&bytes).map_err(service_error)?;
    validate_manifest(&manifest)?;
    if leaf != manifest_leaf(&manifest.id)?
        || path.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new(MANIFEST_DIRECTORY))
    {
        return Err(service_error(
            "manifest locator does not match its private registration anchor",
        ));
    }
    if native_manifest_anchor(&manifest)?.is_some_and(|anchor| !same_location(path, &anchor)) {
        return Err(service_error(
            "native task cache and manifest anchor differ",
        ));
    }
    Ok((store, leaf, manifest))
}

pub(super) fn ensure_installation_locator(
    old: &ServiceManifest,
    manifest: &ServiceManifest,
    store: &StoreDirectory,
    leaf: &Path,
) -> Result<()> {
    if old.environment.xdg_data_home == manifest.environment.xdg_data_home {
        return Ok(());
    }
    let control = store.control();
    let anchor = store.path().join(leaf);
    let current = manifest_locator(&manifest.id, &manifest.environment)?;
    if same_location(&anchor, &current) {
        return Ok(());
    }
    if fs::symlink_metadata(&current).is_ok() {
        return Err(service_error(
            "another manifest occupies the new data directory",
        ));
    }
    let pointer = current.with_extension("locator.json");
    let bytes = serde_json::to_vec(&anchor).map_err(service_error)?;
    let mut location = NativeDefinitionLocation::admit(&pointer, control)?;
    match location.inspect(&bytes)? {
        NativeRegistration::Absent => location.publish(&bytes),
        NativeRegistration::Owned => Ok(()),
        NativeRegistration::Foreign => {
            let saved = read_locator(&pointer)?
                .ok_or_else(|| service_error("service locator disappeared"))?;
            let old_bytes = serde_json::to_vec(&saved).map_err(service_error)?;
            let absent = || matches!(fs::symlink_metadata(&saved), Err(error) if error.kind() == std::io::ErrorKind::NotFound);
            if saved.file_name() != anchor.file_name()
                || saved.parent().and_then(Path::file_name)
                    != Some(std::ffi::OsStr::new(MANIFEST_DIRECTORY))
                || !absent()
            {
                return Err(service_error("foreign service manifest locator"));
            }
            // A retired registration can leave its own directory index behind.
            // Reuse it only if the exact owner-private bytes still point to an
            // absent canonical manifest of this service. Never replace a live
            // or unidentified registration's index.
            location.directory.write_atomic_checked(
                &location.leaf,
                &bytes,
                Publication::Replace,
                |_, _| {
                    if !absent() || location.inspect(&old_bytes)? != NativeRegistration::Owned {
                        return Err(service_error("service locator changed during replacement"));
                    }
                    Ok(())
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_canonical_file_wins_over_a_retired_directory_index() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let mut environment = super::super::tests::manifest().environment;
        environment.xdg_data_home = base.clone();
        let id = "io.kio.watch.abc";
        let current = manifest_locator(id, &environment).unwrap();
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        let directory = StoreDirectory::open(current.parent().unwrap()).unwrap();
        directory
            .write_atomic(
                Path::new(current.file_name().unwrap()),
                b"canonical",
                Publication::CreateOnly,
            )
            .unwrap();
        let retired = base.join("retired/kio/watch-services/io.kio.watch.abc.json");
        directory
            .write_atomic(
                Path::new(current.with_extension("locator.json").file_name().unwrap()),
                &serde_json::to_vec(&retired).unwrap(),
                Publication::CreateOnly,
            )
            .unwrap();
        assert_eq!(
            installation_locator(id, &environment, |_| Ok(Some(current.clone()))).unwrap(),
            Some(current)
        );
    }

    #[test]
    fn approved_data_update_reuses_only_an_owned_locator_to_an_absent_registration() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let mut old = super::super::tests::manifest();
        old.environment.xdg_data_home = base.join("old-data");
        let mut new = old.clone();
        new.environment.xdg_data_home = base.join("new-data");
        let anchor_store = StoreDirectory::open(&base).unwrap();
        let leaf = manifest_leaf(&new.id).unwrap();
        anchor_store
            .create_directory_all(Path::new(MANIFEST_DIRECTORY))
            .unwrap();
        anchor_store
            .write_atomic(&leaf, b"canonical", Publication::CreateOnly)
            .unwrap();
        let pointer = manifest_locator(&new.id, &new.environment)
            .unwrap()
            .with_extension("locator.json");
        let retired = base.join("retired/kio").join(&leaf);
        let mut location =
            NativeDefinitionLocation::admit(&pointer, &ControlHandle::default()).unwrap();
        location
            .publish(&serde_json::to_vec(&retired).unwrap())
            .unwrap();
        ensure_installation_locator(&old, &new, &anchor_store, &leaf).unwrap();
        assert_eq!(read_locator(&pointer).unwrap(), Some(base.join(&leaf)));
        fs::create_dir_all(retired.parent().unwrap()).unwrap();
        fs::write(&retired, b"unidentified live record").unwrap();
        let original = serde_json::to_vec(&retired).unwrap();
        location
            .directory
            .write_atomic(&location.leaf, &original, Publication::Replace)
            .unwrap();
        assert!(ensure_installation_locator(&old, &new, &anchor_store, &leaf).is_err());
        assert_eq!(
            location
                .directory
                .read_optional(&location.leaf, MAX_MANIFEST_BYTES)
                .unwrap()
                .unwrap(),
            original
        );
    }
}
