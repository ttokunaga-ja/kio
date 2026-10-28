use std::{fs, path::Path, process::Command};

fn run(scope: &Path, device: &Path, args: &[&str]) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_kio"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "SystemRoot",
            std::env::var_os("SystemRoot").unwrap_or_default(),
        )
        .env("HOME", device.join("home"))
        .env("USERPROFILE", device.join("home"))
        .env("APPDATA", device.join("config"))
        .env("LOCALAPPDATA", device.join("data"))
        .env("XDG_CONFIG_HOME", device.join("config"))
        .env("XDG_DATA_HOME", device.join("data"))
        .env("XDG_CACHE_HOME", device.join("cache"))
        .env("XDG_STATE_HOME", device.join("state"))
        .env("XDG_RUNTIME_DIR", device.join("runtime"))
        .env("TMPDIR", device.join("tmp"))
        .env("TMP", device.join("tmp"))
        .env("TEMP", device.join("tmp"))
        .current_dir(scope)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "args={args:?}; {output:?}");
    output
}

#[test]
fn real_cli_reports_exact_local_readiness_separately() {
    let temporary = tempfile::tempdir().unwrap();
    let scope = temporary.path().join("scope");
    let device = temporary.path().join("device");
    fs::create_dir(&scope).unwrap();
    for name in ["home", "config", "data", "cache", "state", "runtime", "tmp"] {
        fs::create_dir_all(device.join(name)).unwrap();
    }
    fs::write(
        scope.join("document.md"),
        "# Readiness\n\nOffline text evidence.\n",
    )
    .unwrap();

    let init = run(&scope, &device, &["--json", "init", "."]);
    assert_eq!(
        init.stderr,
        b"Adapter readiness: not evaluated; reason: this operation does not select a network Adapter\n"
    );
    let index = run(&scope, &device, &["--json", "index", "--offline", "--yes"]);
    assert_eq!(
        index.stderr,
        b"Adapter HTTPS readiness: not evaluated; reason: no managed CA was acquired for this operation\n"
    );
}
