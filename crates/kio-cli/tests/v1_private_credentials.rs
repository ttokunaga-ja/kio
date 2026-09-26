//! Credential settings must never become diagnostic or log payloads.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

fn command(root: &Path, home: &Path, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    command.env_clear();
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command
        .current_dir(root)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("TMPDIR", home.join("tmp"))
        .env("TEMP", home.join("tmp"))
        .env("TMP", home.join("tmp"))
        .args(args);
    command
}

#[test]
fn malformed_plain_credential_stays_out_of_terminal_and_error_log() {
    const CANARY: &str = "CANARY_SYNTHETIC_CREDENTIAL_3812";
    for json_mode in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let home = temp.path().join("home");
        fs::create_dir(&root).unwrap();
        fs::create_dir_all(home.join("tmp")).unwrap();
        command(&root, &home, &["init"]).assert().success();
        let config = home.join("config/kio");
        fs::create_dir_all(&config).unwrap();
        let tools = config.join("tools.toml");
        fs::write(&tools, format!("[markdown]\nauth = \"plain:{CANARY}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(home.join("config"), fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&tools, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut run = command(&root, &home, &["status"]);
        if json_mode {
            run.arg("--json");
        }
        let output = run.output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.stdout.is_empty());
        assert!(stderr.contains("tools.toml contains invalid TOML at byte"));
        assert!(
            !stderr.contains(CANARY),
            "terminal disclosed credential source"
        );
        if json_mode {
            let error: Value = serde_json::from_str(&stderr).unwrap();
            assert_eq!(error["error_code"], "KIO-E-CONFIG-SCHEMA-001");
        }
        let log = fs::read_to_string(home.join("data/kio/logs/errors.jsonl")).unwrap();
        assert!(log.contains("KIO-E-CONFIG-SCHEMA-001"));
        assert!(
            !log.contains(CANARY),
            "error log disclosed credential source"
        );
    }
}
