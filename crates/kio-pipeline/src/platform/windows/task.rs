use crate::{IoResultExt, PipelineError, Result};
use std::path::Path;

pub(crate) fn retain_task_root(
    root: &Path,
    _listed: &std::fs::Metadata,
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> Result<kio_core::store_dir::StoreDirectory> {
    let expected = kio_core::platform::real_directory_identity(root).pipeline_io(root)?;
    let canonical = root.canonicalize().pipeline_io(root)?;
    let retained =
        kio_core::store_dir::StoreDirectory::open_with_control(&canonical, control.clone())
            .map_err(|error| {
                PipelineError::corrupt(path.display().to_string(), error.to_string())
            })?;

    let identity_matches = expected.is_some()
        && kio_core::platform::directory_handle_identity(&retained.root_handle()) == expected;

    if !identity_matches {
        return Err(PipelineError::corrupt(
            path.display().to_string(),
            "task store root changed while opening",
        ));
    }
    Ok(retained)
}
