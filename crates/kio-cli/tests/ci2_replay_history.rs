#[cfg(target_os = "linux")]
#[test]
fn replay_history_accepts_real_local_readiness_diagnostics() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let temporary = tempfile::tempdir().unwrap();
    let corpus = temporary.path().join("corpus");
    kio_eval::generator::generate_corpus(&corpus, false).unwrap();
    // The replay boundary requires a single-link executable, as in the hosted job.
    let executable = temporary.path().join("kio-replay");
    std::fs::copy(env!("CARGO_BIN_EXE_kio"), &executable).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Cargo's debug CLI contains symbols that exceed the replay binary bound.
    // Strip only the disposable copy; the release CLI used by CI is already bounded.
    let strip = std::process::Command::new("strip")
        .arg("--strip-debug")
        .arg(&executable)
        .status()
        .unwrap();
    assert!(strip.success());
    let metadata = std::fs::metadata(&executable).unwrap();
    assert!(metadata.len() <= 128 * 1024 * 1024);
    assert_eq!(metadata.nlink(), 1);
    let summary = kio_eval::replay::replay_history(&corpus, &executable).unwrap();
    assert!(summary.commits > 0);
}
