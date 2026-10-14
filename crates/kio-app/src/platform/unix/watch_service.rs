use crate::response::watch::WatchServiceRunnerResponse;
use crate::watch_service::invalid;
use kio_core::Result;
use std::path::PathBuf;

pub(crate) fn run_watch_service_runner(
    _manifest_path: PathBuf,
    _control: &kio_core::control::ControlHandle,
) -> Result<WatchServiceRunnerResponse> {
    Err(invalid(
        "watch service runner is reserved for the Windows Task Scheduler",
    ))
}
