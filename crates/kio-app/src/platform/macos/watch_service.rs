use crate::platform::watch_argv;
use crate::response::watch::NativeServiceState;
use crate::watch_service::{
    ServiceEnvironment, ServiceManifest, invalid, run_native, run_native_output,
    service_environment_pairs, service_error,
};
use kio_core::Result;
use std::path::{Path, PathBuf};

pub(crate) fn native_loaded_definition_matches(manifest: &ServiceManifest) -> Result<bool> {
    let output = run_native_output(
        "/bin/launchctl",
        [
            "print".into(),
            format!("gui/{}/{}", unsafe { libc::geteuid() }, manifest.id),
        ],
    )?;
    // A stopped service is deliberately booted out, leaving only our verified
    // plist. A successful query, however, must bind the loaded service to the
    // exact executable and root before it can be controlled or deleted.
    if !output.status.success() {
        // `launchctl print` uses exit 113 for a genuinely unloaded service.
        // Other failures are an ambiguous manager error and must fail closed.
        if output.status.code() == Some(113)
            && (output.stderr.contains("Could not find service")
                || output.stderr.contains("Bad request"))
        {
            return Ok(true);
        }
        return Err(service_error(
            "launchctl could not inspect native watch registration",
        ));
    }
    let expected = watch_argv(manifest)?;
    let path = manifest
        .native_path
        .to_str()
        .ok_or_else(|| service_error("non-UTF-8 native definition path"))?;
    let binary = manifest
        .binary
        .to_str()
        .ok_or_else(|| service_error("non-UTF-8 watch binary path"))?;
    let lines = output.stdout.lines().map(str::trim).collect::<Vec<_>>();
    let exact_path = lines.iter().any(|line| *line == format!("path = {path}"));
    let exact_program = lines
        .iter()
        .any(|line| *line == format!("program = {binary}"));
    let Some(start) = lines.iter().position(|line| *line == "arguments = {") else {
        return Ok(false);
    };
    let Some(end) = lines[start + 1..].iter().position(|line| *line == "}") else {
        return Ok(false);
    };
    let observed = lines[start + 1..start + 1 + end]
        .iter()
        .map(|line| {
            // launchctl's verified output prints one bare argv value per line.
            // Any assignment, escaping, or blank line is ambiguous and rejected.
            (!line.is_empty() && !line.contains(" = ")).then(|| (*line).to_owned())
        })
        .collect::<Option<Vec<_>>>();
    let environment = service_environment_pairs(&manifest.environment)?;
    let environment_matches = environment.iter().all(|(name, value)| {
        lines
            .iter()
            .any(|line| *line == format!("{name} => {value}"))
    });
    Ok(exact_path
        && exact_program
        && environment_matches
        && observed.as_deref() == Some(expected.as_slice()))
}

pub(crate) fn native_definition_path(
    id: &str,
    environment: &ServiceEnvironment,
) -> Result<PathBuf> {
    Ok(environment
        .home
        .join("Library/LaunchAgents")
        .join(format!("{id}.plist")))
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
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array>{}</array><key>EnvironmentVariables</key><dict>{}</dict><key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>\n",
        xml_escape(id)?,
        args.iter()
            .map(|arg| Ok(format!("<string>{}</string>", xml_escape(arg)?)))
            .collect::<Result<Vec<_>>>()?
            .join(""),
        service_environment_pairs(environment)?
            .iter()
            .map(|(name, value)| Ok(format!(
                "<key>{}</key><string>{}</string>",
                xml_escape(name)?,
                xml_escape(value)?
            )))
            .collect::<Result<Vec<_>>>()?
            .join("")
    ))
}

pub(crate) fn native_enable(manifest: &ServiceManifest) -> Result<()> {
    // The per-user LaunchAgent directory is loaded at login. Installing only
    // writes the verified plist, so it cannot start this session's watcher.
    let _ = manifest;
    Ok(())
}

pub(crate) fn native_start(manifest: &ServiceManifest) -> Result<()> {
    let target = format!("gui/{}/{}", unsafe { libc::geteuid() }, manifest.id);
    let printed = run_native_output("/bin/launchctl", ["print".into(), target.clone()])?;
    if !printed.status.success() {
        run_native(
            "/bin/launchctl",
            [
                "bootstrap".into(),
                format!("gui/{}", unsafe { libc::geteuid() }),
                manifest.native_path.display().to_string(),
            ],
        )?;
    }
    run_native("/bin/launchctl", ["kickstart".into(), "-k".into(), target]).map(|_| ())
}

pub(crate) fn native_stop(manifest: &ServiceManifest) -> Result<()> {
    run_native(
        "/bin/launchctl",
        [
            "bootout".into(),
            format!("gui/{}", unsafe { libc::geteuid() }),
            manifest.native_path.display().to_string(),
        ],
    )
    .map(|_| ())
}

pub(crate) fn native_uninstall(manifest: &ServiceManifest) -> Result<()> {
    let target = format!("gui/{}/{}", unsafe { libc::geteuid() }, manifest.id);
    let printed = run_native_output("/bin/launchctl", ["print".into(), target])?;
    if printed.status.success() {
        native_stop(manifest)
    } else if printed.status.code() == Some(113)
        && (printed.stderr.contains("Could not find service")
            || printed.stderr.contains("Bad request"))
    {
        Ok(())
    } else {
        Err(service_error(
            "launchctl could not inspect service before uninstall",
        ))
    }
}

pub(crate) fn native_after_remove() -> Result<()> {
    Ok(())
}

pub(crate) fn native_state(manifest: &ServiceManifest) -> Result<NativeServiceState> {
    let active = run_native_output(
        "/bin/launchctl",
        [
            "print".into(),
            format!("gui/{}/{}", unsafe { libc::geteuid() }, manifest.id),
        ],
    )?
    .status
    .success();
    Ok(NativeServiceState {
        registered: true,
        active,
    })
}

fn xml_escape(value: &str) -> Result<String> {
    if value
        .chars()
        .any(|c| c == '\0' || c == '\r' || c == '\n' || (c.is_control() && c != '\t'))
    {
        return Err(invalid(
            "native scheduler value contains a control character",
        ));
    }
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\"', "&quot;")
        .replace('\'', "&apos;"))
}
