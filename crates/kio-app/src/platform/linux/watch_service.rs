use crate::response::watch::NativeServiceState;
use crate::watch_service::{
    ServiceEnvironment, ServiceManifest, invalid, run_native, run_native_output,
    service_environment_pairs, service_error,
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
        .lines()
        .any(|line| line == format!("FragmentPath={}", manifest.native_path.display()))
        && execution_matches(
            &output.stdout,
            &manifest.binary,
            &manifest.root,
            manifest.reconcile_interval_seconds,
        )
        && environment_matches(&output.stdout, &environment))
}

fn execution_matches(output: &str, binary: &Path, root: &Path, interval: u64) -> bool {
    let Some(exec) = output.lines().find(|line| line.starts_with("ExecStart=")) else {
        return false;
    };
    let expected = format!(
        "{} watch --root {} run --reconcile-interval-seconds {interval}",
        binary.display(),
        root.display()
    );
    exec.matches("argv[]=").count() == 1
        && exec.contains(&format!("path={} ;", binary.display()))
        && exec
            .split_once("argv[]=")
            .and_then(|(_, argv)| argv.strip_prefix(&expected))
            .is_some_and(|suffix| suffix.starts_with(" ;"))
        && interval_matches(output, interval)
}

fn environment_matches(output: &str, expected: &[(&str, String)]) -> bool {
    let Some(environment) = output
        .lines()
        .find_map(|line| line.strip_prefix("Environment="))
    else {
        return false;
    };
    // systemctl quotes strv entries containing whitespace and escapes literal
    // quotes/backslashes. Compare whole entries, never path prefixes.
    let mut entries = Vec::new();
    let mut entry = String::new();
    let mut quoted = false;
    let mut chars = environment.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\"' => quoted = !quoted,
            '\\' => match chars.next() {
                Some(ch @ ('\\' | '\"')) => entry.push(ch),
                _ => return false,
            },
            ch if ch.is_whitespace() && !quoted => {
                if !entry.is_empty() {
                    entries.push(std::mem::take(&mut entry));
                }
            }
            ch => entry.push(ch),
        }
    }
    if quoted {
        return false;
    }
    if !entry.is_empty() {
        entries.push(entry);
    }
    entries.len() == expected.len()
        && expected
            .iter()
            .all(|(name, value)| entries.contains(&format!("{name}={value}")))
}

fn interval_matches(output: &str, interval: u64) -> bool {
    let Some(exec) = output.lines().find(|line| line.starts_with("ExecStart=")) else {
        return false;
    };
    let expected = interval.to_string();
    let mut words = exec.split_whitespace();
    let mut matched = false;
    while let Some(word) = words.next() {
        if word == "--reconcile-interval-seconds" {
            if matched || words.next() != Some(expected.as_str()) {
                return false;
            }
            matched = true;
        }
    }
    matched
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
    fn loaded_execution_and_environment_reject_foreign_prefixes_and_extra_values() {
        let valid = "ExecStart={ path=/bin/kio ; argv[]=/bin/kio watch --root /root run --reconcile-interval-seconds 300 ; ignore_errors=no ; }\nEnvironment=HOME=/home/user XDG_CACHE_HOME=\"/home/user/cache space\"";
        let expected = [
            ("HOME", "/home/user".into()),
            ("XDG_CACHE_HOME", "/home/user/cache space".into()),
        ];
        assert!(execution_matches(
            valid,
            Path::new("/bin/kio"),
            Path::new("/root"),
            300
        ));
        assert!(environment_matches(valid, &expected));
        for foreign in [
            valid.replace("/bin/kio", "/bin/kio-foreign"),
            valid.replace("--root /root", "--root /root-foreign"),
            valid.replace("300 ;", "300 foreign ;"),
        ] {
            assert!(!execution_matches(
                &foreign,
                Path::new("/bin/kio"),
                Path::new("/root"),
                300
            ));
        }
        for foreign in [
            valid.replace("HOME=/home/user", "HOME=/home/user-foreign"),
            format!("{valid} EVIL=1"),
        ] {
            assert!(!environment_matches(&foreign, &expected));
        }
    }

    #[test]
    fn loaded_interval_requires_the_exact_argument() {
        assert!(interval_matches(
            "ExecStart={ argv[]=/bin/kio watch --root /root run --reconcile-interval-seconds 300 ; }",
            300
        ));
        assert!(!interval_matches(
            "ExecStart={ argv[]=/bin/kio watch --root /root run --reconcile-interval-seconds 3000 ; }",
            300
        ));
        assert!(!interval_matches(
            "ExecStart=foreign\nEnvironment=--reconcile-interval-seconds 300",
            300
        ));
    }
}
