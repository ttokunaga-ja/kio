use crate::platform::watch_argv;
use crate::response::watch::NativeServiceState;
use crate::watch_service::{
    ServiceEnvironment, ServiceManifest, invalid, run_native, run_native_output,
    service_environment_pairs, service_error,
};
use kio_core::Result;
use std::path::{Path, PathBuf};

pub(crate) fn native_registration_absent(manifest: &ServiceManifest) -> Result<bool> {
    let output = run_native_output(
        "/bin/launchctl",
        [
            "print".into(),
            format!("gui/{}/{}", unsafe { libc::geteuid() }, manifest.id),
        ],
    )?;
    if output.status.success() {
        return Ok(false);
    }
    if output.status.code() == Some(113)
        && (output.stderr.contains("Could not find service")
            || output.stderr.contains("Bad request"))
    {
        return Ok(true);
    }
    Err(service_error(
        "launchctl could not inspect native watch registration",
    ))
}

pub(crate) fn native_prepare_replacement(_manifest: &ServiceManifest) -> Result<()> {
    Ok(())
}

pub(crate) fn native_manifest_anchor(_manifest: &ServiceManifest) -> Result<Option<PathBuf>> {
    Ok(None)
}

pub(crate) fn native_manifest_locator(id: &str) -> Result<Option<PathBuf>> {
    let output = run_native_output(
        "/bin/launchctl",
        [
            "print".into(),
            format!("gui/{}/{}", unsafe { libc::geteuid() }, id),
        ],
    )?;
    let data = if output.status.success() {
        let values = output
            .stdout
            .lines()
            .filter_map(|line| line.trim().strip_prefix("XDG_DATA_HOME => "))
            .collect::<Vec<_>>();
        if values.len() != 1 {
            return Err(service_error("ambiguous registered service data directory"));
        }
        Some(PathBuf::from(values[0]))
    } else if output.status.code() == Some(113) {
        unloaded_agent_data_home(id)?
    } else {
        return Err(service_error(
            "launchd could not locate its registered manifest",
        ));
    };
    let Some(data) = data else {
        return Ok(None);
    };
    let path = data.join("kio/watch-services").join(format!("{id}.json"));
    crate::watch_service::validate_argument_path(&path)?;
    Ok(Some(path))
}

fn account_home() -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut capacity = 1024;
    loop {
        let mut buffer = Vec::new();
        buffer.try_reserve_exact(capacity).map_err(|_| {
            kio_core::error::ResourceLimit {
                resource: kio_core::error::Resource::Memory,
                stage: "service_account_lookup".into(),
                limit: None,
                observed: None,
                resource_tier: None,
            }
            .into_error("cannot allocate account lookup buffer")
        })?;
        buffer.resize(capacity, 0_u8);
        let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found = std::ptr::null_mut();
        let result = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if result == libc::ERANGE {
            capacity = capacity
                .checked_mul(2)
                .ok_or_else(|| service_error("account lookup overflow"))?;
            continue;
        }
        if result != 0 || found.is_null() {
            return Err(service_error("cannot resolve login account home"));
        }
        let record = unsafe { record.assume_init() };
        if record.pw_dir.is_null() {
            return Err(service_error("login account home unavailable"));
        }
        let value = unsafe { std::ffi::CStr::from_ptr(record.pw_dir) };
        return Ok(PathBuf::from(std::ffi::OsStr::from_bytes(value.to_bytes())));
    }
}

fn unloaded_agent_data_home(id: &str) -> Result<Option<PathBuf>> {
    // A stopped LaunchAgent is absent from launchctl but its login registration
    // remains in the account's LaunchAgents directory. Read that owned plist.
    let mut homes = vec![account_home()?];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if !homes.contains(&home) {
            homes.push(home);
        }
    }
    unloaded_agent_data_home_with(&homes, id, |bytes| {
        let mut command = std::process::Command::new("/usr/bin/plutil");
        command
            .args([
                "-extract",
                "EnvironmentVariables.XDG_DATA_HOME",
                "raw",
                "-o",
                "-",
                "-",
            ])
            .env_clear()
            .env("LC_ALL", "C");
        let output = kio_process::run_bounded_command(
            &mut command,
            kio_process::BoundedProcessOptions {
                timeout: std::time::Duration::from_secs(10),
                max_stdout_bytes: 32 * 1024,
                max_stderr_bytes: 4096,
            },
            Some(kio_process::BoundedStdin::new(bytes, 64 * 1024)),
        )
        .map_err(service_error)?;
        if !output.status.success() {
            return Err(service_error(
                "cannot read registered LaunchAgent environment",
            ));
        }
        let data = PathBuf::from(output.stdout.trim_end_matches('\n'));
        crate::watch_service::validate_argument_path(&data)?;
        Ok(data)
    })
}

fn unloaded_agent_data_home_with(
    homes: &[PathBuf],
    id: &str,
    mut decode: impl FnMut(Vec<u8>) -> Result<PathBuf>,
) -> Result<Option<PathBuf>> {
    let mut found = Vec::new();
    for home in homes {
        let parent = home.join("Library/LaunchAgents");
        if !parent.exists() {
            continue;
        }
        let directory = kio_core::private_fs::verify_private_creation_parent(&parent)?;
        let name = format!("{id}.plist");
        if !directory.contains_entry(Path::new(&name))? {
            continue;
        }
        let bytes = kio_core::private_fs::read_private_file_at(&directory, &name, 64 * 1024)?;
        let data = decode(bytes)?;
        if !found.contains(&data) {
            found.push(data);
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(service_error(
            "ambiguous LaunchAgent registration locations",
        )),
    }
}

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
    if native_registration_absent(manifest)? {
        return Ok(());
    }
    if !native_loaded_definition_matches(manifest)? {
        return Err(service_error("launchd registration changed before stop"));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stopped_agent_discovers_its_saved_data_home_without_launchctl_registration() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let directory = home.join("Library/LaunchAgents");
        std::fs::create_dir_all(&directory).unwrap();
        let store = kio_core::store_dir::StoreDirectory::open(&directory).unwrap();
        let id = "io.kio.watch.fixture";
        let leaf = format!("{id}.plist");
        let bytes = b"owned plist with old XDG_DATA_HOME";
        store
            .write_atomic(
                Path::new(&leaf),
                bytes,
                kio_core::store_dir::Publication::CreateOnly,
            )
            .unwrap();
        let data = home.join("old-data");
        let found = unloaded_agent_data_home_with(std::slice::from_ref(&home), id, |observed| {
            assert_eq!(observed, bytes);
            Ok(data.clone())
        })
        .unwrap();
        assert_eq!(found, Some(data));
        assert_eq!(std::fs::read(directory.join(leaf)).unwrap(), bytes);
    }
}
