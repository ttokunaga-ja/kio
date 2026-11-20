use super::watch_runner::{run_native, run_native_output};
use crate::response::watch::NativeServiceState;
use crate::watch_service::{
    ServiceEnvironment, ServiceManifest, invalid, service_environment_pairs, service_error,
};
use kio_core::Result;
use kio_process::BoundedProcessOutput;
use std::path::{Path, PathBuf};

pub(crate) fn native_registration_absent(manifest: &ServiceManifest) -> Result<bool> {
    let output = run_native_output(
        systemctl_path()?,
        [
            "--user".into(),
            "show".into(),
            "--property=LoadState".into(),
            manifest.id.clone(),
        ],
    )?;
    if output
        .stdout
        .lines()
        .any(|line| line == "LoadState=not-found")
    {
        return Ok(true);
    }
    if !output.status.success() {
        return Err(service_error(
            "systemd could not inspect native watch registration",
        ));
    }
    Ok(false)
}

fn complete_source(manifest: &ServiceManifest) -> Result<String> {
    let output = run_native_output(
        systemctl_path()?,
        [
            "--user".into(),
            "cat".into(),
            "--no-pager".into(),
            manifest.id.clone(),
        ],
    )?;
    if !output.status.success() {
        return Err(service_error(
            "systemd could not read the complete service definition",
        ));
    }
    Ok(output.stdout)
}

pub(crate) fn native_loaded_definition_matches(manifest: &ServiceManifest) -> Result<bool> {
    loaded_definition_matches_with_query(manifest, |query| {
        if query == "cat" {
            return complete_source(manifest);
        }
        let output = run_native_output(
            systemctl_path()?,
            [
                "--user".into(),
                "show".into(),
                "--property=NeedDaemonReload".into(),
                "--value".into(),
                manifest.id.clone(),
            ],
        )?;
        if !output.status.success() {
            return Err(service_error(
                "systemd could not verify its loaded definition",
            ));
        }
        Ok(output.stdout)
    })
}

fn loaded_definition_matches_with_query(
    manifest: &ServiceManifest,
    mut query: impl FnMut(&str) -> Result<String>,
) -> Result<bool> {
    if !super::watch_systemd_unit::unit_definition_matches(
        &query("cat")?,
        &manifest.native_path,
        &manifest.definition,
    ) {
        return Ok(false);
    }
    // Matching backing files do not authorize commands in a stale manager.
    Ok(query("dirty")?.trim() == "no")
}

pub(crate) fn native_prepare_replacement(manifest: &ServiceManifest) -> Result<()> {
    if native_loaded_definition_matches(manifest)? {
        return Ok(());
    }
    // Called only for a durable, approved replacement whose private target
    // file has already been published. Do not execute any cached stop action.
    if !super::watch_systemd_unit::unit_definition_matches(
        &complete_source(manifest)?,
        &manifest.native_path,
        &manifest.definition,
    ) || native_state(manifest)?.active
    {
        return Err(service_error(
            "foreign or active service during replacement publication recovery",
        ));
    }
    native_after_remove()?;
    if !native_loaded_definition_matches(manifest)? {
        return Err(service_error("service changed during replacement reload"));
    }
    Ok(())
}

pub(crate) fn native_manifest_anchor(_manifest: &ServiceManifest) -> Result<Option<PathBuf>> {
    Ok(None)
}

pub(crate) fn native_manifest_locator(id: &str) -> Result<Option<PathBuf>> {
    let output = run_native_output(
        systemctl_path()?,
        [
            "--user".into(),
            "show".into(),
            "--property=LoadState,Environment".into(),
            id.into(),
        ],
    )?;
    if output
        .stdout
        .lines()
        .any(|line| line == "LoadState=not-found")
    {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(service_error(
            "systemd could not locate its registered manifest",
        ));
    }
    let environment = output
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("Environment="))
        .ok_or_else(|| service_error("systemd did not expose the service environment"))?;
    let words = systemd_words(environment)?;
    let values = words
        .iter()
        .filter_map(|word| word.strip_prefix("XDG_DATA_HOME="))
        .collect::<Vec<_>>();
    if values.len() != 1 {
        return Err(service_error("ambiguous registered service data directory"));
    }
    let path = Path::new(values[0])
        .join("kio/watch-services")
        .join(format!("{id}.json"));
    crate::watch_service::validate_argument_path(&path)?;
    Ok(Some(path))
}

fn systemd_words(value: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\"' => quoted = !quoted,
            '\\' => match chars.next() {
                Some(ch @ ('\\' | '\"')) => word.push(ch),
                _ => return Err(service_error("ambiguous systemd environment escape")),
            },
            ch if ch.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            ch => word.push(ch),
        }
    }
    if quoted {
        return Err(service_error("unclosed systemd environment quote"));
    }
    if !word.is_empty() {
        words.push(word);
    }
    Ok(words)
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
    if !native_loaded_definition_matches(manifest)? {
        return Err(service_error(
            "refusing to enable a foreign composed service definition",
        ));
    }
    run_systemctl(["enable".into(), manifest.id.clone()]).map(|_| ())
}

pub(crate) fn native_start(manifest: &ServiceManifest) -> Result<()> {
    native_enable(manifest)?;
    run_systemctl(["start".into(), manifest.id.clone()]).map(|_| ())
}

pub(crate) fn native_stop(manifest: &ServiceManifest) -> Result<()> {
    if native_registration_absent(manifest)? {
        return Ok(());
    }
    if !native_loaded_definition_matches(manifest)? {
        return Err(service_error("systemd registration changed before stop"));
    }
    // After an interrupted unlink the manager may still retain the old unit.
    // There is no file left to disable, but the loaded instance must be stopped
    // before daemon-reload can complete removal.
    if manifest.native_path.is_file() {
        run_systemctl(["disable".into(), manifest.id.clone()])?;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn review2_drop_in_exec_stop_is_foreign_before_any_lifecycle_command() {
        let value = serde_json::json!({
            "version":2,"id":"io.kio.watch.test","scope_id":"scope","root":"/root","binary":"/bin/kio",
            "reconcile_interval_seconds":300,"native_path":"/config/systemd/user/watch.service","definition":"[Service]\nExecStart=/bin/kio\n",
            "environment":{"home":"/home/user","userprofile":"/home/user","xdg_config_home":"/config","xdg_data_home":"/data","xdg_cache_home":"/cache"},"phase":"installed"
        });
        let manifest: ServiceManifest = serde_json::from_value(value).unwrap();
        let show = "FragmentPath=/config/systemd/user/watch.service\nExecStart={ path=/bin/kio ; argv[]=/bin/kio watch --root /root run --reconcile-interval-seconds 300 ; }\nEnvironment=HOME=/home/user USERPROFILE=/home/user XDG_CONFIG_HOME=/config XDG_DATA_HOME=/data XDG_CACHE_HOME=/cache";
        let cat = format!(
            "# {}\n{}\n# /config/systemd/user/watch.service.d/foreign.conf\n[Service]\nExecStop=/bin/foreign\n",
            manifest.native_path.display(),
            manifest.definition
        );
        let own = format!(
            "# {}\n{}",
            manifest.native_path.display(),
            manifest.definition
        );
        assert!(
            loaded_definition_matches_with_query(&manifest, |query| Ok(if query == "dirty" {
                "no".into()
            } else {
                own.clone()
            }))
            .unwrap()
        );
        assert!(
            !loaded_definition_matches_with_query(&manifest, |query| Ok(if query == "dirty" {
                "yes".into()
            } else {
                own.clone()
            }))
            .unwrap()
        );
        assert!(
            !loaded_definition_matches_with_query(&manifest, |query| Ok(if query == "cat" {
                cat.clone()
            } else {
                show.into()
            }))
            .unwrap()
        );
    }
}
