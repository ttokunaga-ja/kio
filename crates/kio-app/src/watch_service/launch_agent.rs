use crate::watch_service::{
    ServiceEnvironment, invalid, service_environment_pairs, service_error, validate_argument_path,
};
use kio_core::Result;
use std::path::{Path, PathBuf};

pub(crate) fn loaded_agent_data_home_with(
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
    validate_argument_path(path)?;
    if path.file_name() != Some(std::ffi::OsStr::new(&format!("{id}.plist"))) {
        return Err(service_error(
            "loaded LaunchAgent source has a foreign identity",
        ));
    }
    read(path)
}

pub(crate) fn unloaded_agent_data_home_with(
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

pub(crate) fn registered_agent_data_home_with(
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

pub(crate) fn agent_definition_path(id: &str, account: &Path) -> Result<PathBuf> {
    crate::watch_service::manifest_leaf(id)?;
    let path = account
        .join("Library/LaunchAgents")
        .join(format!("{id}.plist"));
    validate_argument_path(&path)?;
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
