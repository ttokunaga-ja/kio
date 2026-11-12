//! docs/04 section 5.4: durable local execution state is operational, not knowledge.
#![cfg(debug_assertions)]
mod support;
use assert_cmd::Command;
use kio_core::test_control::TestEnvGuard;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

fn state_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            assert!(!meta.file_type().is_symlink());
            if meta.is_dir() {
                pending.push(path);
            } else {
                assert!(meta.is_file() && meta.len() <= 2 * 1024 * 1024);
                files.insert(
                    path.strip_prefix(root).unwrap().into(),
                    fs::read(path).unwrap(),
                );
            }
        }
        assert!(
            files.len() + pending.len() <= 512,
            "bounded actual local state"
        );
    }
    files
}

#[test]
fn app01_default_home_local_execution_state_is_never_discovered_as_knowledge() {
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let root = support::canonical_tempdir();
    let scope = support::create_private_child_dir(root.path(), "knowledge-home");
    let external = support::create_private_child_dir(root.path(), "external");
    for name in ["data", "config", "cache", "runtime", "tmp"] {
        support::create_private_child_dir(&external, name);
    }
    let _home = TestEnvGuard::set("HOME", &scope);
    let _profile = TestEnvGuard::set("USERPROFILE", &scope);
    let _data = TestEnvGuard::set("XDG_DATA_HOME", external.join("data"));
    let _config = TestEnvGuard::set("XDG_CONFIG_HOME", external.join("config"));
    let _cache = TestEnvGuard::set("XDG_CACHE_HOME", external.join("cache"));
    let _runtime = TestEnvGuard::set("XDG_RUNTIME_DIR", external.join("runtime"));
    let _state = TestEnvGuard::remove("XDG_STATE_HOME");
    fs::write(
        scope.join("note.md"),
        "# Actual local knowledge\n\nfallback-state positive control needle\n",
    )
    .unwrap();
    // Keep setup from enrolling either the new state or its ancestor controls.
    // Remove this fixture-only rule before proving the precise exclusion and
    // the ordinary .local sibling positive controls below.
    const SETUP_RULE: &[u8] = b".local/\n";
    fs::write(scope.join(".kioignore"), SETUP_RULE).unwrap();
    let run = |args: &[&str]| -> Value {
        let mut cmd = Command::cargo_bin("kio").unwrap();
        cmd.env_clear().current_dir(&scope);
        if let Some(value) = std::env::var_os("SystemRoot") {
            cmd.env("SystemRoot", value);
        }
        cmd.env("HOME", &scope)
            .env("USERPROFILE", &scope)
            .env("XDG_DATA_HOME", external.join("data"))
            .env("XDG_CONFIG_HOME", external.join("config"))
            .env("XDG_CACHE_HOME", external.join("cache"))
            .env("XDG_RUNTIME_DIR", external.join("runtime"))
            .env("TMPDIR", external.join("tmp"))
            .env("TMP", external.join("tmp"))
            .env("TEMP", external.join("tmp"))
            .env("KIO_EVAL_DETERMINISTIC_EMBED", "scale-v3")
            .env("KIO_TEST_MARKDOWNIZE_ADAPTER", "deterministic")
            .env("KIO_TEST_QUERY_EMBED_TRACE", external.join("query.trace"))
            .env("KIO_FIXED_NOW", "2026-09-08T00:00:00Z")
            .args(args)
            .arg("--json");
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    run(&["init", "--independent", "--yes"]);
    run(&["index", "--offline"]);
    let source = support::immutable_sqlite(scope.join(".kio/index/sqlite.db"));
    let count: i64 = source
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE target_type='chunk'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(count > 0, "actual free positive document embedding");
    drop(source);
    let query = run(&[
        "search",
        "fallback-state positive control needle",
        "--mode",
        "vector",
        "--offline",
        "--scope",
        scope.to_str().unwrap(),
    ]);
    assert_eq!(query["resolved_mode"], "vector");
    assert_eq!(
        fs::read_to_string(external.join("query.trace"))
            .unwrap()
            .lines()
            .count(),
        1,
        "actual free query dispatch"
    );
    let operational = scope.join(".local/state/kio/local-execution");
    assert!(
        operational.is_dir(),
        "unset STATE must use actual HOME fallback"
    );
    let before = state_bytes(&operational);
    assert!(
        before
            .keys()
            .any(|path| path.file_name() == Some(std::ffi::OsStr::new("record.json"))),
        "actual durable local record"
    );
    assert_eq!(fs::read(scope.join(".kioignore")).unwrap(), SETUP_RULE);
    fs::remove_file(scope.join(".kioignore")).unwrap();
    let ordinary = support::create_private_child_dir(&scope.join(".local"), "unowned-knowledge");
    fs::write(
        ordinary.join("ordinary.md"),
        "ordinary unowned data must remain discoverable\n",
    )
    .unwrap();
    let sibling =
        support::create_private_child_dir(operational.parent().unwrap(), "local-execution-unowned");
    fs::write(
        sibling.join("ordinary.md"),
        "same-parent prefix sibling is ordinary unowned knowledge\n",
    )
    .unwrap();
    let binding = kio_core::management::ManagementBinding::bind(&scope).unwrap();
    let mut paged = Vec::new();
    let mut continuation = None;
    for _ in 0..8 {
        let slice =
            kio_pipeline::scan::discover_managed_child_scopes_page(&binding, continuation.as_ref())
                .unwrap();
        paged.extend(slice.plan.candidates);
        continuation = slice.continuation;
        if continuation.is_none() {
            break;
        }
    }
    assert!(
        continuation.is_none(),
        "bounded discovery fixture must finish"
    );
    let one_shot = kio_pipeline::scan::discover_managed_child_scopes(&binding).unwrap();
    for (kind, candidates) in [("paged", &paged), ("one-shot", &one_shot.candidates)] {
        assert!(
            candidates
                .iter()
                .any(|c| c.path == ".local/unowned-knowledge" && c.status == "planned"),
            "{kind}: unowned .local data remains eligible: {candidates:?}"
        );
        assert!(
            candidates
                .iter()
                .any(|c| c.path == ".local/state/kio/local-execution-unowned"
                    && c.status == "planned"),
            "{kind}: do not exclude the whole state/kio parent or lexical prefix siblings: {candidates:?}"
        );
        assert!(
            !candidates.iter().any(|c| c.status == "planned"
                && (c.path == ".local/state/kio/local-execution"
                    || c.path.starts_with(".local/state/kio/local-execution/"))),
            "{kind}: operational state must never be enrolled or scanned as knowledge: {candidates:?}"
        );
    }
    assert_eq!(
        state_bytes(&operational),
        before,
        "discovery is read-only for actual execution state"
    );
    assert!(
        !ordinary.join(".kio").exists(),
        "read-only discovery does not enroll ordinary child"
    );
}
