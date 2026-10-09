use crate::watch_service::{ServiceManifest, service_error};
use kio_core::Result;

pub(crate) fn watch_argv(manifest: &ServiceManifest) -> Result<Vec<String>> {
    Ok(vec![
        manifest
            .binary
            .to_str()
            .ok_or_else(|| service_error("non-UTF-8 watch binary path"))?
            .to_owned(),
        "watch".into(),
        "--root".into(),
        manifest
            .root
            .to_str()
            .ok_or_else(|| service_error("non-UTF-8 watch root path"))?
            .to_owned(),
        "run".into(),
        "--reconcile-interval-seconds".into(),
        manifest.reconcile_interval_seconds.to_string(),
    ])
}
