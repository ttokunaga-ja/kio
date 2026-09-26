#![cfg(target_os = "linux")]

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Child, Command},
    time::Duration,
};

use kio_process::{
    BoundedProcessOptions,
    confinement::{RenderResourceLimits, RenderSandbox},
};

struct FixtureProcess(Child);
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn renderer_procfs_does_not_expose_host_processes() {
    // This dedicated host fixture has no ambient credentials. It must be
    // invisible through the renderer's procfs even though it has the same UID.
    let host = FixtureProcess(
        Command::new("/bin/sleep")
            .arg("30")
            .env_clear()
            .env("KIO_PRIVATE_FIXTURE", "host-only")
            .spawn()
            .unwrap(),
    );
    let scratch = tempfile::tempdir().unwrap();
    let runtime = ["/bin", "/usr/bin", "/lib", "/lib64", "/usr/lib"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.exists());
    let sandbox = RenderSandbox::new(
        Path::new("/bin/sh"),
        scratch.path(),
        runtime,
        RenderResourceLimits::default(),
    )
    .expect("Linux acceptance requires bubblewrap");
    let output = sandbox
        .run(
            [
                OsString::from("-c"),
                OsString::from(
                    "test -r /proc/self/status || exit 31; \
             if cat \"/proc/$1/environ\" >/dev/null 2>&1; then exit 32; fi; \
             if test -r \"/proc/$1/root/etc/passwd\"; then exit 33; fi; printf isolated",
                ),
                OsString::from("fixture"),
                OsString::from(host.0.id().to_string()),
            ],
            &[(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            BoundedProcessOptions {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 4096,
            },
        )
        .expect("Linux acceptance requires working namespace confinement");
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(output.stdout, "isolated");
}

#[test]
fn selecting_a_renderer_file_does_not_admit_its_siblings() {
    use std::{fs, os::unix::fs::PermissionsExt};
    let scratch = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let renderer = home.path().join("soffice");
    fs::write(home.path().join("private.txt"), b"private sibling").unwrap();
    fs::write(
        &renderer,
        b"#!/bin/sh\nif cat \"$KIO_SIBLING\" 2>/dev/null; then exit 31; fi\nprintf confined\n",
    )
    .unwrap();
    fs::set_permissions(&renderer, fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = ["/bin", "/usr/bin", "/lib", "/lib64", "/usr/lib"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.exists());
    let sandbox = RenderSandbox::new(
        &renderer,
        scratch.path(),
        runtime,
        RenderResourceLimits::default(),
    )
    .unwrap();
    let output = sandbox
        .run(
            std::iter::empty::<OsString>(),
            &[
                (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
                (
                    OsString::from("KIO_SIBLING"),
                    home.path().join("private.txt").into_os_string(),
                ),
            ],
            BoundedProcessOptions {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
        )
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(output.stdout, "confined");
}

#[test]
fn runtime_symlink_is_mounted_at_its_requested_alias() {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    let scratch = tempfile::tempdir().unwrap();
    let trusted = tempfile::tempdir().unwrap();
    let renderer = trusted.path().join("renderer");
    fs::write(trusted.path().join("runtime.txt"), b"alias-visible").unwrap();
    fs::write(&renderer, b"#!/bin/sh\ncat \"$KIO_ALIAS/runtime.txt\"\n").unwrap();
    fs::set_permissions(&renderer, fs::Permissions::from_mode(0o700)).unwrap();
    let alias_dir = tempfile::tempdir_in("/tmp").unwrap();
    let alias = alias_dir.path().join("runtime-alias");
    symlink(trusted.path(), &alias).unwrap();
    let runtime = ["/bin", "/usr/bin", "/lib", "/lib64", "/usr/lib"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .chain(std::iter::once(alias.clone()));
    let sandbox = RenderSandbox::new(
        &renderer,
        scratch.path(),
        runtime,
        RenderResourceLimits::default(),
    )
    .unwrap();
    let output = sandbox
        .run(
            std::iter::empty::<OsString>(),
            &[(OsString::from("KIO_ALIAS"), alias.into_os_string())],
            BoundedProcessOptions {
                timeout: Duration::from_secs(10),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
        )
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr);
    assert_eq!(output.stdout, "alias-visible");
}
