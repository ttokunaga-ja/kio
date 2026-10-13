use crate::management::{binding_for_repo, open_existing_managed_root};
use crate::platform::watch_service::native_windows_task_definition_matches;
use crate::platform::windows::wait_supervised_child;
use crate::platform::{validate_runner_manifest_locator, watch_argv};
use crate::response::watch::WatchServiceRunnerResponse;
use crate::watch_service::{
    MAX_MANIFEST_BYTES, NativeRegistration, NativeScheduler, Scheduler, ServiceManifest, invalid,
    service_environment_pairs, service_error, validate_manifest,
};
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::management::{ManagementAuthority, read_record};
use kio_core::store_dir::StoreDirectory;
use std::env;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Start the Windows Task Scheduler child only after re-binding its private
/// manifest to this executable, the native task, and the managed root.
///
/// The scheduler does not provide a trustworthy inherited environment.  This
/// runner therefore sets the captured device paths on the child `Command`
/// rather than mutating this process environment.
pub(crate) fn run_watch_service_runner(
    manifest_path: PathBuf,
    control: &ControlHandle,
) -> Result<WatchServiceRunnerResponse> {
    let manifest = load_runner_manifest(&manifest_path, control)?;
    let current_binary = env::current_exe()
        .map_err(service_error)?
        .canonicalize()
        .map_err(service_error)?;
    let current_binary = kio_core::private_fs::verify_trusted_executable(&current_binary)?;
    let manifest_binary = kio_core::private_fs::verify_trusted_executable(&manifest.binary)?;
    if current_binary != manifest_binary {
        return Err(service_error(
            "watch service runner executable does not match its manifest",
        ));
    }
    let root = manifest.root.canonicalize().map_err(service_error)?;
    if root != manifest.root {
        return Err(service_error(
            "watch service root no longer matches its canonical manifest path",
        ));
    }
    let repo = open_existing_managed_root(&root, control)?;
    let binding = binding_for_repo(&repo)?;
    let record = read_record(&binding)?;
    if record.authority != ManagementAuthority::Root || record.scope_id != manifest.scope_id {
        return Err(service_error(
            "watch service manifest no longer matches the managed root authority",
        ));
    }
    let scheduler = NativeScheduler(control.clone());
    if scheduler.inspect(&manifest)? != NativeRegistration::Owned
        || !native_runner_registration_matches(&manifest)?
    {
        return Err(service_error(
            "watch service runner refuses an absent or foreign native registration",
        ));
    }
    let mut child = watcher_child_command(&manifest)?;
    let status = wait_supervised_child(&mut child).map_err(service_error)?;
    if !status.success() {
        return Err(service_error(format!(
            "watch service child exited unsuccessfully: {status}"
        )));
    }
    Ok(WatchServiceRunnerResponse {
        status: "watcher_exited".to_owned(),
        service_id: manifest.id,
    })
}

fn load_runner_manifest(manifest_path: &Path, control: &ControlHandle) -> Result<ServiceManifest> {
    if !manifest_path.is_absolute() {
        return Err(invalid("watch service manifest path must be absolute"));
    }
    // This first read proves every ancestor and the leaf are owner-private
    // without trusting scheduler-provided environment variables.
    let private_bytes = kio_core::private_fs::read_private_file_with_control(
        manifest_path,
        MAX_MANIFEST_BYTES,
        control,
    )?;
    let manifest: ServiceManifest =
        serde_json::from_slice(&private_bytes).map_err(service_error)?;
    validate_manifest(&manifest)?;
    validate_runner_manifest_locator(manifest_path, &manifest)?;
    let parent = manifest_path
        .parent()
        .ok_or_else(|| service_error("watch service manifest has no parent"))?;
    let leaf = manifest_path
        .file_name()
        .ok_or_else(|| service_error("watch service manifest has no file name"))?;
    // Retain the verified parent before opening the leaf a second time.  The
    // byte comparison catches a replacement between the private-file audit
    // and capability-relative read.
    let store = StoreDirectory::open_with_control(parent, control.clone())?;
    let file = store.open_regular_read(Path::new(leaf), MAX_MANIFEST_BYTES)?;
    store.ensure_owner_private(Path::new(leaf))?;
    let mut retained_bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES.saturating_add(1))
        .read_to_end(&mut retained_bytes)
        .map_err(service_error)?;
    if retained_bytes.len() > MAX_MANIFEST_BYTES as usize {
        return Err(service_error(
            "watch service manifest exceeds its byte limit",
        ));
    }
    if retained_bytes != private_bytes {
        return Err(service_error(
            "watch service manifest changed while the runner was binding it",
        ));
    }
    Ok(manifest)
}

fn watcher_child_command(manifest: &ServiceManifest) -> Result<Command> {
    let mut command = Command::new(&manifest.binary);
    command.args(watch_argv(manifest)?.into_iter().skip(1));
    for (name, value) in service_environment_pairs(&manifest.environment)? {
        command.env(name, value);
    }
    Ok(command)
}

/// Runner startup has a stricter condition than lifecycle recovery: a missing
/// task can be safely uninstalled, but it cannot authorize a manually invoked
/// hidden runner to start a watcher.
fn native_runner_registration_matches(manifest: &ServiceManifest) -> Result<bool> {
    native_windows_task_definition_matches(manifest, false)
}
