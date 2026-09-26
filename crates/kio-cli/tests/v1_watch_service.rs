//! CLI surface contracts for native watch-service lifecycle commands.
//!
//! These tests intentionally inspect help only: lifecycle installation is never
//! exercised against the developer's real native scheduler in CI.

use assert_cmd::Command;

#[test]
fn watch_service_help_exposes_lifecycle_without_starting_a_native_service() {
    let output = Command::cargo_bin("kio")
        .unwrap()
        .args(["watch", "service", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for command in ["install", "start", "stop", "status", "uninstall"] {
        assert!(
            help.contains(command),
            "missing lifecycle command {command}: {help}"
        );
    }
}

#[test]
fn watch_service_install_help_exposes_bounded_interval() {
    let output = Command::cargo_bin("kio")
        .unwrap()
        .args(["watch", "service", "install", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--reconcile-interval-seconds"));
}
