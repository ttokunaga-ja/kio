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

fn loaded_agent_data_home_with(
    printed: &str,
    id: &str,
    read: impl FnOnce(&Path) -> Result<PathBuf>,
) -> Result<PathBuf> {
    let values = printed
        .lines()
        .filter_map(|line| line.trim().strip_prefix("path = "))
        .collect::<Vec<_>>();
    if values.len() != 1 {
        return Err(service_error("ambiguous loaded LaunchAgent source"));
    }
    let path = Path::new(values[0]);
    crate::watch_service::validate_argument_path(path)?;
    if path.file_name() != Some(std::ffi::OsStr::new(&format!("{id}.plist"))) {
        return Err(service_error(
            "loaded LaunchAgent source has a foreign identity",
        ));
    }
    read(path)
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

fn unloaded_agent_data_home_with(
    accounts: &[PathBuf],
    id: &str,
    decode: impl FnMut(Vec<u8>) -> Result<PathBuf>,
) -> Result<Option<PathBuf>> {
    let paths = accounts
        .iter()
        .map(|home| agent_definition_path(id, home))
        .collect::<Result<Vec<_>>>()?;
    registered_agent_data_home_with(&paths, id, decode)
}

fn registered_agent_data_home_with(
    paths: &[PathBuf],
    id: &str,
    mut decode: impl FnMut(Vec<u8>) -> Result<PathBuf>,
) -> Result<Option<PathBuf>> {
    let mut found = None;
    for path in paths {
        let parent = path
            .parent()
            .ok_or_else(|| service_error("invalid LaunchAgent source"))?;
        if !parent.exists() {
            continue;
        }
        let directory = kio_core::private_fs::verify_private_creation_parent(parent)?;
        let name = format!("{id}.plist");
        if path.file_name() != Some(std::ffi::OsStr::new(&name)) {
            return Err(service_error("foreign LaunchAgent source identity"));
        }
        if !directory.contains_entry(Path::new(&name))? {
            continue;
        }
        if found.is_some() {
            return Err(service_error("duplicate LaunchAgent registrations"));
        }
        let bytes = kio_core::private_fs::read_private_file_at(&directory, &name, 64 * 1024)?;
        found = Some(decode(bytes)?);
    }
    Ok(found)
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
    _environment: &ServiceEnvironment,
) -> Result<PathBuf> {
    agent_definition_path(id, &account_home()?)
}

fn agent_definition_path(id: &str, account: &Path) -> Result<PathBuf> {
    crate::watch_service::manifest_leaf(id)?;
    let path = account
        .join("Library/LaunchAgents")
        .join(format!("{id}.plist"));
    crate::watch_service::validate_argument_path(&path)?;
    Ok(path)
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
    fn environment(base: &Path, name: &str) -> ServiceEnvironment {
        ServiceEnvironment {
            home: base.join(format!("{name}-home")),
            userprofile: base.join(format!("{name}-profile")),
            xdg_config_home: base.join(format!("{name}-config")),
            xdg_data_home: base.join(format!("{name}-data")),
            xdg_cache_home: base.join(format!("{name}-cache")),
        }
    }

    #[test]
    fn stopped_agent_home_and_data_update_discovers_registration_and_cannot_duplicate_id() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let account = base.join("os-account");
        let old = environment(&base, "old");
        let new = environment(&base, "new");
        let id = "io.kio.watch.fixture";
        let registered = agent_definition_path(id, &account).unwrap();
        let definition = render_definition(
            id,
            &base.join("kio"),
            &base.join("root"),
            300,
            &old,
            &old.xdg_data_home
                .join("kio/watch-services")
                .join(format!("{id}.json")),
        )
        .unwrap();
        std::fs::create_dir_all(registered.parent().unwrap()).unwrap();
        let store =
            kio_core::store_dir::StoreDirectory::open(registered.parent().unwrap()).unwrap();
        store
            .write_atomic(
                Path::new(registered.file_name().unwrap()),
                definition.as_bytes(),
                kio_core::store_dir::Publication::CreateOnly,
            )
            .unwrap();
        // bootout has removed the loaded service. Only the OS login plist
        // remains; neither old captured HOME nor data home is an input.
        let found = unloaded_agent_data_home_with(std::slice::from_ref(&account), id, |bytes| {
            assert_eq!(bytes, definition.as_bytes());
            Ok(old.xdg_data_home.clone())
        })
        .unwrap();
        assert_eq!(
            found,
            Some(old.xdg_data_home.clone()),
            "stopped registration must survive simultaneous HOME and XDG_DATA_HOME changes"
        );
        assert_eq!(
            agent_definition_path(id, &account).unwrap(),
            registered,
            "one service ID must keep one OS login registration"
        );
        let updated = render_definition(
            id,
            &base.join("kio"),
            &base.join("root"),
            600,
            &new,
            &old.xdg_data_home
                .join("kio/watch-services")
                .join(format!("{id}.json")),
        )
        .unwrap();
        // A second installation into the canonical OS slot cannot overwrite
        // this ID; replacement must use the existing registration.
        assert!(
            store
                .write_atomic(
                    Path::new(registered.file_name().unwrap()),
                    updated.as_bytes(),
                    kio_core::store_dir::Publication::CreateOnly
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&registered).unwrap(), definition.as_bytes());
        assert!(!new.home.join("Library/LaunchAgents").exists());
    }

    #[test]
    fn loaded_agent_uses_the_registered_plist_path_instead_of_environment_values() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let id = "io.kio.watch.fixture";
        let registered = base
            .join("old-home/Library/LaunchAgents")
            .join(format!("{id}.plist"));
        let data = base.join("private-original");
        let printed = format!(
            "gui/501/{id} = {{\n path = {}\n XDG_DATA_HOME => /misleading-child-environment\n}}",
            registered.display()
        );
        let found = loaded_agent_data_home_with(&printed, id, |path| {
            assert_eq!(path, registered);
            Ok(data.clone())
        })
        .unwrap();
        assert_eq!(found, data);
        assert!(
            loaded_agent_data_home_with("path = /tmp/foreign.plist", id, |_| panic!(
                "foreign path"
            ))
            .is_err()
        );
    }

    #[test]
    fn duplicate_agent_paths_are_rejected_even_when_their_data_home_is_identical() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let homes = [base.join("account"), base.join("other")];
        let id = "io.kio.watch.fixture";
        for home in &homes {
            let directory = home.join("Library/LaunchAgents");
            std::fs::create_dir_all(&directory).unwrap();
            let store = kio_core::store_dir::StoreDirectory::open(&directory).unwrap();
            store
                .write_atomic(
                    Path::new(&format!("{id}.plist")),
                    b"duplicate",
                    kio_core::store_dir::Publication::CreateOnly,
                )
                .unwrap();
        }
        let paths = homes
            .iter()
            .map(|home| agent_definition_path(id, home).unwrap())
            .collect::<Vec<_>>();
        assert!(
            registered_agent_data_home_with(&paths, id, |_| Ok(base.join("same-data"))).is_err(),
            "same data home does not make duplicate OS registrations safe"
        );
        for path in paths {
            assert_eq!(std::fs::read(path).unwrap(), b"duplicate");
        }
    }

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
