use crate::watch_service::{ServiceManifest, invalid, manifest_locator, service_error};
use kio_core::Result;
use std::path::Path;

pub(crate) fn validate_runner_manifest_locator(
    manifest_path: &Path,
    manifest: &ServiceManifest,
) -> Result<()> {
    if manifest_path != manifest_locator(&manifest.id, &manifest.environment)? {
        return Err(service_error(
            "watch service runner manifest locator does not bind its environment and identifier",
        ));
    }
    Ok(())
}

pub(crate) fn runner_argv(manifest_path: &Path) -> Result<Vec<String>> {
    let manifest_path = manifest_path
        .to_str()
        .ok_or_else(|| invalid("watch service manifest path is not valid UTF-8"))?;
    Ok(vec![
        "watch".into(),
        "service".into(),
        "runner".into(),
        "--manifest".into(),
        manifest_path.to_owned(),
    ])
}
