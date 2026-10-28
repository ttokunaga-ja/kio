//! Exercise the eval consumer with stdout from Cargo's actual CLI binary.
mod support;

use std::{fs, path::Path, process::Command};

fn run(root: &Path, device: &Path, args: &[&str]) -> Vec<u8> {
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
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "args={args:?}; {output:?}");
    output.stdout
}

#[test]
fn real_offline_index_stdout_obeys_eval_canonical_contract() {
    for initialize_ledger in [false, true] {
        let temporary = support::canonical_tempdir();
        let root = temporary.path().join("scope");
        let device = temporary.path().join("device");
        fs::create_dir(&root).unwrap();
        for name in ["home", "config", "data", "cache", "state", "runtime", "tmp"] {
            fs::create_dir_all(device.join(name)).unwrap();
        }
        fs::write(
            root.join("document.md"),
            "# Contract fixture\n\nOffline text evidence.\n",
        )
        .unwrap();
        run(&root, &device, &["--json", "init", "."]);
        if initialize_ledger {
            run(&root, &device, &["--json", "ledger", "init"]);
        }
        let bytes = run(&root, &device, &["--json", "index", "--offline", "--yes"]);
        kio_eval::scale_prepare::validate_offline_repair_index_output(&bytes, 1, 0).unwrap_or_else(
            |error| {
                panic!(
                    "ledger={initialize_ledger}; {error}; stdout={}",
                    String::from_utf8_lossy(&bytes)
                )
            },
        );
    }
}
