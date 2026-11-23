pub(crate) use super::watch_launch_agent::render_definition;
use super::watch_launch_agent::{
    agent_definition_path, loaded_agent_data_home_with, registered_agent_data_home_with,
    unloaded_agent_data_home_with,
};
use super::watch_runner::{run_native, run_native_output};
use crate::platform::watch_argv;
use crate::response::watch::NativeServiceState;
use crate::watch_service::{
    ServiceEnvironment, ServiceManifest, service_environment_pairs, service_error,
};
use kio_core::Result;
use kio_process::BoundedProcessOutput;
use std::path::PathBuf;

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
    if service_absent(&output) {
        return Ok(true);
    }
    Err(service_error(
        "launchctl could not inspect native watch registration",
    ))
}

fn service_absent(output: &BoundedProcessOutput) -> bool {
    output.status.code() == Some(113) && output.stderr.contains("Could not find service")
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
        Some(loaded_agent_data_home_with(&output.stdout, id, |path| {
            // Inspect both the loaded registration's source and the actual OS
            // login directory. A second plist is a conflict, even if it points
            // to the same manifest.
            let canonical = agent_definition_path(id, &account_home()?)?;
            let paths = if path == canonical {
                vec![canonical]
            } else {
                vec![path.to_path_buf(), canonical]
            };
            registered_agent_data_home_with(&paths, id, decode_agent_data_home)?
                .ok_or_else(|| service_error("loaded LaunchAgent source disappeared"))
        })?)
    } else if service_absent(&output) {
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
                page_progress: None,
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
    // launchd loads this account directory at login. Captured watcher HOME is
    // child configuration and must never select another registration directory.
    unloaded_agent_data_home_with(&[account_home()?], id, decode_agent_data_home)
}

fn decode_agent_data_home(bytes: Vec<u8>) -> Result<PathBuf> {
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
        if service_absent(&output) {
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

pub(crate) const NATIVE_DEFINITION_EXTENSION: &str = "plist";

pub(crate) fn native_definition_path(
    id: &str,
    _environment: &ServiceEnvironment,
) -> Result<PathBuf> {
    agent_definition_path(id, &account_home()?)
}

pub(crate) fn native_enable(manifest: &ServiceManifest) -> Result<()> {
    // The per-user LaunchAgent directory is loaded at login. Installing only
    // writes the verified plist, so it cannot start this session's watcher.
    let _ = manifest;
    Ok(())
}

pub(crate) fn native_start(manifest: &ServiceManifest) -> Result<()> {
    let domain = format!("gui/{}", unsafe { libc::geteuid() });
    let target = format!("{domain}/{}", manifest.id);
    let printed = run_native_output("/bin/launchctl", ["print".into(), target.clone()])
        .map_err(|error| service_error(format!("launchctl print: {}", error.message())))?;
    start_launch_agent(&printed, domain, target, &manifest.native_path, |args| {
        let verb = &args[0];
        run_native("/bin/launchctl", args.iter().cloned())
            .map(|_| ())
            .map_err(|error| service_error(format!("launchctl {verb}: {}", error.message())))
    })
}

fn start_launch_agent(
    printed: &BoundedProcessOutput,
    domain: String,
    target: String,
    plist: &std::path::Path,
    run: impl FnOnce(Vec<String>) -> Result<()>,
) -> Result<()> {
    if printed.status.success() {
        run(vec!["kickstart".into(), target])
    } else if service_absent(printed) {
        // RunAtLoad starts the watcher on bootstrap. Forcing a restart here
        // kills that instance and waits for launchd's restart throttle.
        run(vec![
            "bootstrap".into(),
            domain,
            plist.display().to_string(),
        ])
    } else {
        Err(service_error(
            "launchctl print could not identify the LaunchAgent before start",
        ))
    }
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
    } else if service_absent(&printed) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::process::ExitStatusExt, path::Path, process::ExitStatus};

    #[test]
    fn start_bootstraps_an_unloaded_agent_once_and_never_forces_a_restart() {
        for (status, stderr, expected) in [
            (
                113,
                "Bad request.\nCould not find service",
                Some(vec!["bootstrap", "gui/501", "/tmp/probe.plist"]),
            ),
            (0, "", Some(vec!["kickstart", "gui/501/io.kio.watch.probe"])),
            (113, "Bad request.", None),
        ] {
            let printed = BoundedProcessOutput {
                status: ExitStatus::from_raw(status << 8),
                stdout: String::new(),
                stderr: stderr.into(),
                duration: std::time::Duration::ZERO,
            };
            let mut called = None;
            let result = start_launch_agent(
                &printed,
                "gui/501".into(),
                "gui/501/io.kio.watch.probe".into(),
                Path::new("/tmp/probe.plist"),
                |args| {
                    called = Some(args);
                    Ok(())
                },
            );
            assert_eq!(result.is_ok(), expected.is_some());
            assert_eq!(
                called,
                expected.map(|args| args.into_iter().map(String::from).collect())
            );
        }
    }
}
