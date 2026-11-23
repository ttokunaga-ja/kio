//! Complete systemd source comparison, shared with the scheduler fixtures.
use std::path::{Path, PathBuf};

use kio_core::Result;
use serde::Deserialize;

use crate::watch_service::{service_error, validate_argument_path};

pub(crate) fn persistent_unit_path(unit_path_json: &str, name: &str) -> Result<PathBuf> {
    #[derive(Deserialize)]
    struct UnitPath {
        #[serde(rename = "type")]
        signature: String,
        data: Vec<PathBuf>,
    }

    let paths: UnitPath = serde_json::from_str(unit_path_json).map_err(service_error)?;
    if paths.signature != "as" {
        return Err(service_error("systemd UnitPath is not a string array"));
    }
    // The manager exposes both systemd/user.control and systemd/user under
    // its persistent config directory. The analogous runtime pair shares
    // its directory with systemd/transient and cannot persist across logins.
    let mut candidates = paths
        .data
        .iter()
        .filter(|path| path.is_absolute() && path.ends_with("systemd/user.control"))
        .filter(|path| !paths.data.contains(&path.with_file_name("transient")))
        .map(|path| path.with_file_name("user"))
        .filter(|path| paths.data.contains(path));
    let directory = candidates
        .next()
        .filter(|_| candidates.next().is_none())
        .ok_or_else(|| {
            service_error("systemd did not expose a unique persistent user unit directory")
        })?;
    let path = directory.join(name);
    validate_argument_path(&path)?;
    Ok(path)
}

pub(crate) fn unit_definition_matches(observed: &str, path: &Path, definition: &str) -> bool {
    let expected = format!("# {}\n{}", path.display(), definition);
    observed.trim_end_matches('\n') == expected.trim_end_matches('\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_manager_config_directory_with_spaces_and_ignores_runtime() {
        let json = r#"{"type":"as","data":["/home/Runner Account/.config/systemd/user.control",
            "/run/user/1001/systemd/user.control","/run/user/1001/systemd/transient",
            "/home/Runner Account/.config/systemd/user","/etc/systemd/user",
            "/run/user/1001/systemd/user","/usr/lib/systemd/user"]}"#;
        assert_eq!(
            persistent_unit_path(json, "io.kio.watch.example.service").unwrap(),
            Path::new("/home/Runner Account/.config/systemd/user/io.kio.watch.example.service")
        );
    }

    #[test]
    fn rejects_runtime_only_and_ambiguous_manager_paths() {
        let runtime = r#"{"type":"as","data":["/run/user/1001/systemd/user.control",
            "/run/user/1001/systemd/transient","/run/user/1001/systemd/user"]}"#;
        assert!(persistent_unit_path(runtime, "example.service").is_err());
        let ambiguous = r#"{"type":"as","data":["/a/systemd/user.control",
            "/a/systemd/user","/b/systemd/user.control","/b/systemd/user"]}"#;
        assert!(persistent_unit_path(ambiguous, "example.service").is_err());
    }

    #[test]
    fn matching_manager_fragment_with_a_drop_in_is_still_foreign() {
        let path = Path::new("/home/runner/.config/systemd/user/example.service");
        let definition = "[Service]\nExecStart=/bin/kio\n";
        let source = format!("# {}\n{definition}", path.display());
        assert!(unit_definition_matches(&source, path, definition));
        let composed = format!(
            "{source}\n# {}.d/override.conf\n[Service]\nExecStop=/bin/other\n",
            path.display()
        );
        assert!(!unit_definition_matches(&composed, path, definition));
    }
}
