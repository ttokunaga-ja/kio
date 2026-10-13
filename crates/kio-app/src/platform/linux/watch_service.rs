use crate::response::watch::NativeServiceState;
use crate::watch_service::{
    ServiceEnvironment, ServiceManifest, invalid, run_native, run_native_output,
    service_environment_pairs, service_error,
};
use kio_core::Result;
use kio_process::BoundedProcessOutput;
use std::path::{Path, PathBuf};

pub(crate) fn native_loaded_definition_matches(manifest: &ServiceManifest) -> Result<bool> {
    let output = run_native_output(
        systemctl_path()?,
        [
            "--user".into(),
            "show".into(),
            "--property=FragmentPath,ExecStart,Environment".into(),
            manifest.id.clone(),
        ],
    )?;
    if !output.status.success() {
        return Err(service_error(
            "systemd could not inspect native watch registration",
        ));
    }
    let environment = service_environment_pairs(&manifest.environment)?;
    Ok(output
        .stdout
        .contains(manifest.native_path.to_string_lossy().as_ref())
        && output
            .stdout
            .contains(manifest.binary.to_string_lossy().as_ref())
        && output
            .stdout
            .contains(manifest.root.to_string_lossy().as_ref())
        && environment
            .iter()
            .all(|(name, value)| output.stdout.contains(&format!("{name}={value}"))))
}

pub(crate) fn native_definition_path(
    id: &str,
    environment: &ServiceEnvironment,
) -> Result<PathBuf> {
    Ok(environment
        .xdg_config_home
        .join("systemd/user")
        .join(format!("{id}.service")))
}

pub(crate) fn render_definition(
    id: &str,
    binary: &Path,
    root: &Path,
    interval: u64,
    environment: &ServiceEnvironment,
    _manifest_path: &Path,
) -> Result<String> {
    let args = [
        binary.as_os_str().to_string_lossy().into_owned(),
        "watch".into(),
        "--root".into(),
        root.as_os_str().to_string_lossy().into_owned(),
        "run".into(),
        "--reconcile-interval-seconds".into(),
        interval.to_string(),
    ];
    Ok(format!(
        "[Unit]\nDescription=Kio offline watch {}\n\n[Service]\nType=simple\nEnvironment={}\nExecStart={}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        systemd_escape(id)?,
        service_environment_pairs(environment)?
            .iter()
            .map(|(name, value)| systemd_escape(&format!("{name}={value}")))
            .collect::<Result<Vec<_>>>()?
            .join(" "),
        args.iter()
            .map(|arg| systemd_escape(arg))
            .collect::<Result<Vec<_>>>()?
            .join(" ")
    ))
}

pub(crate) fn native_enable(manifest: &ServiceManifest) -> Result<()> {
    run_systemctl(["daemon-reload".into()])?;
    run_systemctl(["enable".into(), manifest.id.clone()]).map(|_| ())
}

pub(crate) fn native_start(manifest: &ServiceManifest) -> Result<()> {
    native_enable(manifest)?;
    run_systemctl(["start".into(), manifest.id.clone()]).map(|_| ())
}

pub(crate) fn native_stop(manifest: &ServiceManifest) -> Result<()> {
    run_systemctl(["disable".into(), manifest.id.clone()])?;
    run_systemctl(["stop".into(), manifest.id.clone()]).map(|_| ())
}

pub(crate) fn native_uninstall(manifest: &ServiceManifest) -> Result<()> {
    native_stop(manifest)
}

pub(crate) fn native_after_remove() -> Result<()> {
    run_systemctl(["daemon-reload".into()]).map(|_| ())
}

pub(crate) fn native_state(manifest: &ServiceManifest) -> Result<NativeServiceState> {
    let enabled = run_native_output(
        systemctl_path()?,
        ["--user".into(), "is-enabled".into(), manifest.id.clone()],
    )?
    .status
    .success();
    let active = run_native_output(
        systemctl_path()?,
        ["--user".into(), "is-active".into(), manifest.id.clone()],
    )?
    .status
    .success();
    Ok(NativeServiceState {
        registered: enabled,
        active,
    })
}

fn systemctl_path() -> Result<&'static str> {
    if Path::new("/usr/bin/systemctl").is_file() {
        Ok("/usr/bin/systemctl")
    } else if Path::new("/bin/systemctl").is_file() {
        Ok("/bin/systemctl")
    } else {
        Err(service_error("systemctl is unavailable"))
    }
}

fn run_systemctl(args: impl IntoIterator<Item = String>) -> Result<BoundedProcessOutput> {
    run_native(
        systemctl_path()?,
        std::iter::once("--user".to_owned()).chain(args),
    )
}

fn systemd_escape(value: &str) -> Result<String> {
    if value.chars().any(|c| c.is_control()) {
        return Err(invalid(
            "native scheduler value contains a control character",
        ));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('\"', "\\\"")
    ))
}
