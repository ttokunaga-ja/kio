//! Private, minimal process environments for native acceptance children.
//!
//! Acceptance children must not inherit credentials, debug controls, or a
//! caller-selected executable search path.  Windows still requires a small
//! OS runtime surface after `env_clear`, so that surface is made explicit here
//! rather than being reconstructed independently by each acceptance lane.

use std::{env, ffi::OsString, path::PathBuf, process::Command};

use crate::acceptance::AcceptanceError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IsolatedChildEnvironment {
    home: PathBuf,
    config: PathBuf,
    data: PathBuf,
    cache: PathBuf,
    tmp: PathBuf,
}

impl IsolatedChildEnvironment {
    pub(crate) fn new(
        home: impl Into<PathBuf>,
        config: impl Into<PathBuf>,
        data: impl Into<PathBuf>,
        cache: impl Into<PathBuf>,
        tmp: impl Into<PathBuf>,
    ) -> Self {
        Self {
            home: home.into(),
            config: config.into(),
            data: data.into(),
            cache: cache.into(),
            tmp: tmp.into(),
        }
    }

    pub(crate) fn apply(&self, command: &mut Command) -> Result<(), AcceptanceError> {
        self.apply_with_system_root(command, env::var_os("SystemRoot"))
    }

    fn apply_with_system_root(
        &self,
        command: &mut Command,
        system_root: Option<OsString>,
    ) -> Result<(), AcceptanceError> {
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("TMPDIR", &self.tmp);

        #[cfg(windows)]
        {
            let system_root = system_root.ok_or_else(|| {
                AcceptanceError::Invalid(
                    "Windows acceptance child requires a host SystemRoot".into(),
                )
            })?;
            command
                .env("APPDATA", &self.config)
                .env("LOCALAPPDATA", &self.data)
                .env("USERPROFILE", &self.home)
                .env("TEMP", &self.tmp)
                .env("TMP", &self.tmp)
                .env("SystemRoot", system_root);
        }

        #[cfg(not(windows))]
        let _ = system_root;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, process::Command};

    use super::IsolatedChildEnvironment;

    fn environment(command: &Command) -> BTreeMap<OsString, Option<OsString>> {
        command
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(OsString::from)))
            .collect()
    }

    #[test]
    fn isolated_environment_excludes_ambient_secrets_and_path() {
        let paths = IsolatedChildEnvironment::new(
            PathBuf::from("/private/home"),
            PathBuf::from("/private/config"),
            PathBuf::from("/private/data"),
            PathBuf::from("/private/cache"),
            PathBuf::from("/private/tmp"),
        );
        let mut command = Command::new("kio");
        command
            .env("MISTRAL_API_KEY", "ambient-secret")
            .env("GEMINI_API_KEY", "ambient-secret")
            .env("KIO_TEST_DURABILITY_POINT", "ambient-control")
            .env("PATH", "/ambient/path");

        assert!(
            paths
                .apply_with_system_root(&mut command, Some(OsString::from(r"C:\Windows")))
                .is_ok()
        );
        let environment = environment(&command);
        for forbidden in [
            "MISTRAL_API_KEY",
            "GEMINI_API_KEY",
            "KIO_TEST_DURABILITY_POINT",
            "PATH",
        ] {
            assert!(!environment.contains_key(&OsString::from(forbidden)));
        }
        assert_eq!(
            environment.get(&OsString::from("HOME")),
            Some(&Some(OsString::from("/private/home")))
        );
        assert_eq!(
            environment.get(&OsString::from("XDG_CONFIG_HOME")),
            Some(&Some(OsString::from("/private/config")))
        );
        assert_eq!(
            environment.get(&OsString::from("TMPDIR")),
            Some(&Some(OsString::from("/private/tmp")))
        );
    }

    #[cfg(unix)]
    #[test]
    fn actual_child_receives_only_the_private_environment() {
        let paths = IsolatedChildEnvironment::new(
            "/private/home",
            "/private/config",
            "/private/data",
            "/private/cache",
            "/private/tmp",
        );
        let mut command = Command::new("/usr/bin/env");
        command
            .env("GITHUB_ENV", "ambient-github-env")
            .env("GITHUB_OUTPUT", "ambient-github-output")
            .env("MISTRAL_API_KEY", "ambient-provider-key")
            .env("GEMINI_API_KEY", "ambient-provider-key")
            .env("KIO_TEST_DURABILITY_POINT", "ambient-test-control")
            .env("PATH", "/ambient/path");
        assert!(
            paths
                .apply_with_system_root(&mut command, Some(OsString::from("ignored-on-unix")))
                .is_ok()
        );

        let output = command.output().expect("private child must start");
        assert!(output.status.success());
        let environment = String::from_utf8_lossy(&output.stdout);
        for forbidden in [
            "GITHUB_ENV=",
            "GITHUB_OUTPUT=",
            "MISTRAL_API_KEY=",
            "GEMINI_API_KEY=",
            "KIO_TEST_DURABILITY_POINT=",
            "PATH=",
        ] {
            assert!(
                !environment.contains(forbidden),
                "child inherited {forbidden}"
            );
        }
        assert!(environment.contains("HOME=/private/home\n"));
        assert!(environment.contains("XDG_DATA_HOME=/private/data\n"));
    }

    #[cfg(windows)]
    #[test]
    fn actual_windows_child_receives_only_the_private_environment() {
        let paths = IsolatedChildEnvironment::new(
            r"C:\private\home",
            r"C:\private\config",
            r"C:\private\data",
            r"C:\private\cache",
            r"C:\private\tmp",
        );
        let mut command = Command::new("cmd.exe");
        command
            .args(["/C", "set"])
            .env("GITHUB_ENV", "ambient-github-env")
            .env("GITHUB_OUTPUT", "ambient-github-output")
            .env("MISTRAL_API_KEY", "ambient-provider-key")
            .env("GEMINI_API_KEY", "ambient-provider-key")
            .env("KIO_TEST_DURABILITY_POINT", "ambient-test-control")
            .env("PATH", r"C:\ambient\path");
        assert!(
            paths
                .apply_with_system_root(&mut command, Some(OsString::from(r"C:\Windows")))
                .is_ok()
        );

        let output = command.output().expect("private child must start");
        assert!(output.status.success());
        let environment = String::from_utf8_lossy(&output.stdout);
        for forbidden in [
            "GITHUB_ENV=",
            "GITHUB_OUTPUT=",
            "MISTRAL_API_KEY=",
            "GEMINI_API_KEY=",
            "KIO_TEST_DURABILITY_POINT=",
            "PATH=",
        ] {
            assert!(
                !environment.contains(forbidden),
                "child inherited {forbidden}"
            );
        }
        assert!(environment.contains(r"USERPROFILE=C:\private\home"));
        assert!(environment.contains(r"TEMP=C:\private\tmp"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_environment_requires_and_preserves_only_system_root_runtime() {
        let paths = IsolatedChildEnvironment::new(
            PathBuf::from(r"C:\private\home"),
            PathBuf::from(r"C:\private\config"),
            PathBuf::from(r"C:\private\data"),
            PathBuf::from(r"C:\private\cache"),
            PathBuf::from(r"C:\private\tmp"),
        );
        let mut missing = Command::new("kio.exe");
        assert!(paths.apply_with_system_root(&mut missing, None).is_err());

        let mut command = Command::new("kio.exe");
        assert!(
            paths
                .apply_with_system_root(&mut command, Some(OsString::from(r"C:\Windows")))
                .is_ok()
        );
        let environment = environment(&command);
        assert_eq!(
            environment.get(&OsString::from("SystemRoot")),
            Some(&Some(OsString::from(r"C:\Windows")))
        );
        assert_eq!(
            environment.get(&OsString::from("APPDATA")),
            Some(&Some(OsString::from(r"C:\private\config")))
        );
        assert_eq!(
            environment.get(&OsString::from("TEMP")),
            Some(&Some(OsString::from(r"C:\private\tmp")))
        );
    }
}
