use crate::{IoResultExt, PipelineError, Result};
use std::path::Path;

pub(crate) fn retain_task_root(
    root: &Path,
    listed: &std::fs::Metadata,
    path: &Path,
    control: &kio_core::control::ControlHandle,
) -> Result<kio_core::store_dir::StoreDirectory> {
    let canonical = root.canonicalize().pipeline_io(root)?;
    let retained =
        kio_core::store_dir::StoreDirectory::open_with_control(&canonical, control.clone())
            .map_err(|error| {
                PipelineError::corrupt(path.display().to_string(), error.to_string())
            })?;

    let identity_matches = {
        use std::os::unix::fs::MetadataExt;
        let opened = retained.root_handle().metadata().pipeline_io(root)?;
        listed.dev() == opened.dev() && listed.ino() == opened.ino()
    };

    if !identity_matches {
        return Err(PipelineError::corrupt(
            path.display().to_string(),
            "task store root changed while opening",
        ));
    }
    Ok(retained)
}
