mod support;

use support::canonical_tempdir;

use std::fs;
use std::process::Command;

use kio_core::scope::Repository;
use serde_json::Value;
use tempfile::TempDir;

fn command(dir: &TempDir, args: &[&str]) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("kio"));
    command
        .current_dir(dir.path())
        .envs(support::home_env(dir.path()))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .arg("--json")
        .args(args);
    command
}

fn ok(dir: &TempDir, args: &[&str]) -> Value {
    let output = command(dir, args).output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn failure(dir: &TempDir, args: &[&str], code: i32) -> Value {
    let output = command(dir, args).output().unwrap();
    assert_eq!(output.status.code(), Some(code), "{args:?}");
    serde_json::from_slice(&output.stderr).unwrap()
}

fn index(dir: &TempDir) -> String {
    ok(dir, &["index", "--offline"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn write_new_working_fixture(dir: &TempDir, leaf: &str, bytes: &[u8]) {
    #[cfg(windows)]
    {
        use kio_core::store_dir::{Publication, StoreDirectory};
        let target = StoreDirectory::open(dir.path()).unwrap();
        let owner = StoreDirectory::open(&dir.path().join(".kio")).unwrap();
        target
            .write_atomic_with_owner(
                &owner,
                std::path::Path::new(leaf),
                bytes,
                Publication::CreateOnly,
            )
            .unwrap();
    }
    #[cfg(not(windows))]
    fs::write(dir.path().join(leaf), bytes).unwrap();
}

fn fixture() -> (TempDir, String, String) {
    let dir = canonical_tempdir();
    ok(&dir, &["init"]);
    write_new_working_fixture(&dir, "a.md", b"old unique orchid phrase");
    write_new_working_fixture(&dir, "b.md", b"old b");
    let source = index(&dir);
    fs::write(dir.path().join("a.md"), b"new unique tulip phrase").unwrap();
    write_new_working_fixture(&dir, "c.md", b"keep c");
    let head = index(&dir);
    (dir, source, head)
}

#[test]
fn managed_restore_selected_path_is_a_linear_child_and_preserves_unselected_files() {
    let (dir, source, head) = fixture();
    fs::write(dir.path().join("c.md"), b"local dirty c").unwrap();
    let output = ok(&dir, &["restore", &source, "--path", "a.md"]);
    let restored = output["apply"]["commit_hash"].as_str().unwrap().to_owned();
    assert_eq!(
        fs::read(dir.path().join("a.md")).unwrap(),
        b"old unique orchid phrase"
    );
    assert_eq!(fs::read(dir.path().join("c.md")).unwrap(), b"local dirty c");
    assert_eq!(output["source_commit"], source);
    assert_eq!(output["expected_head"], head);
    assert_eq!(output["projection_status"], "ready");
    assert_eq!(output["progress"]["phase"], "published");
    assert_eq!(output["progress"]["files_completed"], 1);
    assert_eq!(output["progress"]["bytes_completed"], 24);

    let search = ok(&dir, &["search", "orchid", "--mode", "text"]);
    assert!(
        search["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| result["evidence_pointer"]["path_at_commit"] == "a.md")
    );
    let dirty_search = ok(&dir, &["search", "localdirty", "--mode", "text"]);
    assert!(dirty_search["results"].as_array().unwrap().is_empty());

    let repo = Repository::open(dir.path()).unwrap();
    let commit = repo.read_commit(&restored).unwrap();
    assert_eq!(commit.parent.as_deref(), Some(head.as_str()));
    assert_eq!(commit.restore_provenance.unwrap().source_commit, source);
}

#[test]
fn managed_restore_preview_and_expected_head_conflict_do_not_mutate() {
    let (dir, source, head) = fixture();
    let before = fs::read(dir.path().join("a.md")).unwrap();
    let preview = ok(&dir, &["restore", &source, "--preview"]);
    assert_eq!(preview["status"], "preview");
    assert_eq!(preview["progress"]["phase"], "planned");
    assert_eq!(preview["progress"]["files_total"], 1);
    assert_eq!(preview["progress"]["bytes_total"], 24);
    assert_eq!(preview["expected_head"], head);
    assert_eq!(fs::read(dir.path().join("a.md")).unwrap(), before);
    assert_eq!(
        fs::read_to_string(dir.path().join(".kio/HEAD"))
            .unwrap()
            .trim(),
        head
    );

    let error = failure(&dir, &["restore", &source, "--expected-head", &source], 3);
    assert_eq!(error["error_code"], "KIO-E-MANAGED-RESTORE-CONFLICT-001");
    assert_eq!(fs::read(dir.path().join("a.md")).unwrap(), before);
}

#[test]
fn managed_restore_default_is_policy_filtered_and_explicit_denial_fails() {
    let (dir, source, _) = fixture();
    fs::write(dir.path().join(".kioignore"), b"b.md\n").unwrap();
    let preview = ok(&dir, &["restore", &source, "--preview"]);
    let paths = preview["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"a.md"));
    assert!(!paths.contains(&"b.md"));

    let error = failure(&dir, &["restore", &source, "--path", "b.md"], 2);
    assert_eq!(error["error_code"], "KIO-E-MANAGED-RESTORE-POLICY-001");
}

#[cfg(debug_assertions)]
fn restore_stage_path(dir: &TempDir) -> std::path::PathBuf {
    let journal: Value =
        serde_json::from_slice(&fs::read(dir.path().join(".kio/managed-restore-v1.json")).unwrap())
            .unwrap();
    dir.path()
        .join(".kio")
        .join(journal["stages"][0]["leaf"].as_str().unwrap())
}

#[cfg(debug_assertions)]
struct StoppedRestore(std::process::Child);
#[cfg(debug_assertions)]
impl Drop for StoppedRestore {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(debug_assertions)]
fn paused(dir: &TempDir, args: &[&str], point: &str) -> (StoppedRestore, std::path::PathBuf) {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let ready = dir.path().join(format!(".test-{point}.ready"));
    let stderr = dir.path().join(format!(".test-{point}.stderr"));
    let mut child = StoppedRestore(
        command(dir, args)
            .env("KIO_TEST_DURABILITY_POINT", point)
            .env("KIO_TEST_DURABILITY_READY", &ready)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    let expected = format!("point={point}\npid={}\n", child.0.id());
    let deadline = Instant::now() + Duration::from_secs(30);
    while fs::read_to_string(&ready).ok().as_deref() != Some(&expected) {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "{point}: {}",
            fs::read_to_string(&stderr).unwrap()
        );
        assert!(Instant::now() < deadline, "checkpoint timeout: {point}");
        std::thread::sleep(Duration::from_millis(10));
    }
    (child, ready)
}

#[cfg(debug_assertions)]
fn kill_at(dir: &TempDir, args: &[&str], point: &str) {
    let (mut child, _) = paused(dir, args, point);
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
}

#[cfg(debug_assertions)]
#[test]
fn st06_process_crashes_at_each_apply_and_publication_phase_recover() {
    for point in [
        "restore_journal",
        "restore_removing_old",
        "atomic_remove_ready",
        "atomic_remove_quarantined",
        "atomic_remove_deleted",
        "restore_old_removed",
        "restore_publishing_new",
        "restore_stage_ready",
        "restore_file_published",
        "restore_applied",
        "restore_working_bytes",
        "publication_journal",
        "publication_manifest",
        "publication_head",
        "restore_head",
    ]
    .into_iter()
    .chain(if cfg!(unix) {
        Some("stage_captured")
    } else {
        None
    }) {
        let (dir, source, head) = fixture();
        kill_at(&dir, &["restore", &source, "--path", "a.md"], point);
        let recovered = ok(&dir, &["repair", "recover-restore"]);
        assert_eq!(recovered["recovered"], true, "{point}: {recovered}");
        let repo = Repository::open(dir.path()).unwrap();
        let published = matches!(
            point,
            "publication_head" | "publication_manifest" | "restore_head"
        );
        let current = repo.head_commit_hash().unwrap().unwrap();
        if published {
            assert_ne!(current, head, "{point}");
            assert_eq!(repo.read_commit(&current).unwrap().parent, Some(head));
        } else {
            assert_eq!(current, head, "{point}");
        }
        let expected: &[u8] = if published {
            b"old unique orchid phrase"
        } else {
            b"new unique tulip phrase"
        };
        assert_eq!(
            fs::read(dir.path().join("a.md")).unwrap(),
            expected,
            "{point}"
        );
        assert!(!dir.path().join(".kio/managed-restore-v1.json").exists());
        assert!(
            !fs::read_dir(dir.path().join(".kio"))
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("managed-restore-stage-"))
        );
    }
}

#[cfg(debug_assertions)]
#[test]
fn st06_process_crashes_during_rollback_recover_idempotently() {
    for point in [
        "restore_rollback_removing_new",
        "restore_rollback_new_removed",
        "restore_restoring_old",
        "restore_stage_ready",
        "restore_file_published",
        "atomic_remove_ready",
        "atomic_remove_quarantined",
        "atomic_remove_deleted",
    ]
    .into_iter()
    .chain(if cfg!(unix) {
        Some("stage_captured")
    } else {
        None
    }) {
        let (dir, source, head) = fixture();
        kill_at(
            &dir,
            &["restore", &source, "--path", "a.md"],
            "restore_applied",
        );
        kill_at(&dir, &["repair", "recover-restore"], point);
        let recovered = ok(&dir, &["repair", "recover-restore"]);
        assert_eq!(recovered["recovered"], true, "{point}");
        assert_eq!(
            fs::read(dir.path().join("a.md")).unwrap(),
            b"new unique tulip phrase",
            "{point}"
        );
        assert_eq!(
            Repository::open(dir.path())
                .unwrap()
                .head_commit_hash()
                .unwrap()
                .as_deref(),
            Some(head.as_str())
        );
    }
}

#[cfg(debug_assertions)]
#[test]
fn st06_projection_failure_preserves_the_published_head_and_reports_unavailable() {
    use std::io::Read;
    let (dir, source, head) = fixture();
    let (mut child, ready) = paused(
        &dir,
        &["restore", &source, "--path", "a.md"],
        "restore_head",
    );
    let published = fs::read_to_string(dir.path().join(".kio/HEAD"))
        .unwrap()
        .trim()
        .to_owned();
    assert_ne!(published, head);
    // Break only a reproducible projection after authoritative publication.
    let db = dir.path().join(".kio/index/sqlite.db");
    fs::rename(&db, dir.path().join(".test-original.sqlite")).unwrap();
    fs::create_dir(&db).unwrap();
    fs::write(ready.with_extension("release"), b"continue").unwrap();
    let mut stdout = String::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    assert_eq!(child.0.wait().unwrap().code(), Some(3));
    let output: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(output["status"], "applied_projection_failed");
    assert_eq!(output["projection_status"], "failed");
    assert_eq!(output["progress"]["phase"], "published");
    assert_eq!(
        fs::read_to_string(dir.path().join(".kio/HEAD"))
            .unwrap()
            .trim(),
        published
    );
    assert_eq!(
        fs::read(dir.path().join("a.md")).unwrap(),
        b"old unique orchid phrase"
    );
}

#[cfg(debug_assertions)]
#[test]
fn st06_recovery_can_crash_after_carrying_the_lower_removal_proof() {
    let (dir, source, head) = fixture();
    kill_at(
        &dir,
        &["restore", &source, "--path", "a.md"],
        "atomic_remove_quarantined",
    );
    kill_at(&dir, &["repair", "recover-restore"], "restore_old_removed");
    assert_eq!(ok(&dir, &["repair", "recover-restore"])["recovered"], true);
    assert_eq!(
        fs::read(dir.path().join("a.md")).unwrap(),
        b"new unique tulip phrase"
    );
    assert_eq!(
        Repository::open(dir.path())
            .unwrap()
            .head_commit_hash()
            .unwrap()
            .as_deref(),
        Some(head.as_str())
    );
}

#[cfg(debug_assertions)]
#[test]
fn st06_complete_stage_digest_is_checked_before_publication() {
    let (dir, source, head) = fixture();
    let (mut child, ready) = paused(
        &dir,
        &["restore", &source, "--path", "a.md"],
        "restore_stage_ready",
    );
    fs::write(restore_stage_path(&dir), b"tampered stage contents!!").unwrap();
    fs::write(ready.with_extension("release"), b"continue").unwrap();
    assert_eq!(child.0.wait().unwrap().code(), Some(3));
    assert!(!dir.path().join("a.md").exists());
    assert_eq!(ok(&dir, &["repair", "recover-restore"])["recovered"], true);
    assert_eq!(
        fs::read(dir.path().join("a.md")).unwrap(),
        b"new unique tulip phrase"
    );
    assert_eq!(
        Repository::open(dir.path())
            .unwrap()
            .head_commit_hash()
            .unwrap()
            .as_deref(),
        Some(head.as_str())
    );
}

#[cfg(debug_assertions)]
#[test]
fn review_p1_completed_removal_before_parent_phase_recovers() {
    for rollback in [false, true] {
        let (dir, source, head) = fixture();
        if rollback {
            kill_at(
                &dir,
                &["restore", &source, "--path", "a.md"],
                "restore_applied",
            );
            kill_at(
                &dir,
                &["repair", "recover-restore"],
                "restore_removal_completed",
            );
        } else {
            kill_at(
                &dir,
                &["restore", &source, "--path", "a.md"],
                "restore_removal_completed",
            );
        }
        assert_eq!(ok(&dir, &["repair", "recover-restore"])["recovered"], true);
        assert_eq!(
            fs::read(dir.path().join("a.md")).unwrap(),
            b"new unique tulip phrase"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(".kio/HEAD"))
                .unwrap()
                .trim(),
            head
        );
    }
}

#[cfg(debug_assertions)]
#[test]
fn review_p1_stage_created_before_identity_record_recovers() {
    for point in [
        "restore_stage_created",
        "restore_stage_discarded",
        "stage_file_created",
    ] {
        for rollback in [false, true] {
            let (dir, source, head) = fixture();
            if rollback {
                kill_at(
                    &dir,
                    &["restore", &source, "--path", "a.md"],
                    "restore_applied",
                );
                kill_at(&dir, &["repair", "recover-restore"], point);
            } else {
                kill_at(&dir, &["restore", &source, "--path", "a.md"], point);
            }
            let journal: Value = serde_json::from_slice(
                &fs::read(dir.path().join(".kio/managed-restore-v1.json")).unwrap(),
            )
            .unwrap();
            let reserved = dir
                .path()
                .join(".kio")
                .join(journal["stage_creation"]["leaf"].as_str().unwrap());
            assert_eq!(ok(&dir, &["repair", "recover-restore"])["recovered"], true);
            assert_eq!(
                fs::read(dir.path().join("a.md")).unwrap(),
                b"new unique tulip phrase"
            );
            assert_eq!(
                fs::read_to_string(dir.path().join(".kio/HEAD"))
                    .unwrap()
                    .trim(),
                head
            );
            if point == "stage_file_created" {
                // No saved nonce on this entry: preserve it as unidentified,
                // while retrying with a new reservation and a different name.
                assert!(reserved.exists());
                assert_eq!(fs::metadata(reserved).unwrap().len(), 0);
            } else {
                assert!(!reserved.exists());
            }
        }
    }
}

#[cfg(debug_assertions)]
#[test]
fn review_p1_ready_intent_does_not_undo_external_deletion() {
    let (dir, source, head) = fixture();
    kill_at(
        &dir,
        &["restore", &source, "--path", "a.md"],
        "atomic_remove_ready",
    );
    fs::remove_file(dir.path().join("a.md")).unwrap();
    let output = command(&dir, &["repair", "recover-restore"])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "external deletion must remain ambiguous"
    );
    assert!(!dir.path().join("a.md").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join(".kio/HEAD"))
            .unwrap()
            .trim(),
        head
    );
    assert!(dir.path().join(".kio/managed-restore-v1.json").exists());
}

#[cfg(debug_assertions)]
#[test]
fn review_p1_resumed_rollback_removal_checks_nonce_with_simulated_id_reuse() {
    use kio_core::store_dir::{Publication, StageWriter, StoreDirectory};
    let (dir, source, head) = fixture();
    kill_at(
        &dir,
        &["restore", &source, "--path", "a.md"],
        "restore_applied",
    );
    kill_at(&dir, &["repair", "recover-restore"], "atomic_remove_ready");
    fs::remove_file(dir.path().join("a.md")).unwrap();
    let root = StoreDirectory::open(dir.path()).unwrap();
    let owner = StoreDirectory::open(&dir.path().join(".kio")).unwrap();
    let mut foreign = StageWriter::create(&owner, std::path::Path::new("foreign-stage")).unwrap();
    foreign.write_chunk(b"old unique orchid phrase").unwrap();
    let physical = serde_json::to_value(foreign.identity()).unwrap()["physical"].clone();
    foreign
        .publish(&root, std::path::Path::new("a.md"), Publication::CreateOnly)
        .unwrap();
    // Model a filesystem reusing the saved physical ID, without altering any
    // nonce or bytes. The replacement's independently generated nonce differs.
    let intent_path = dir.path().join(".kio/.kio-atomic/remove.json");
    let mut intent: Value = serde_json::from_slice(&fs::read(&intent_path).unwrap()).unwrap();
    intent["source_id"] = physical.clone();
    if let Some(identity) = intent.get_mut("stage_identity").filter(|v| v.is_object()) {
        identity["physical"] = physical;
    }
    fs::write(&intent_path, serde_jcs::to_vec(&intent).unwrap()).unwrap();
    let output = command(&dir, &["repair", "recover-restore"])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "reused physical ID must not authorize deletion"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("nonce"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(dir.path().join("a.md")).unwrap(),
        b"old unique orchid phrase"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(".kio/HEAD"))
            .unwrap()
            .trim(),
        head
    );
}

#[cfg(debug_assertions)]
#[test]
fn review_p1_retry_stage_disposal_preserves_the_durable_gap_proof() {
    for origin in ["restore_publishing_new", "restore_restoring_old"] {
        for point in ["restore_stage_discarded", "restore_stage_created"] {
            let (dir, source, head) = fixture();
            if origin == "restore_restoring_old" {
                kill_at(
                    &dir,
                    &["restore", &source, "--path", "a.md"],
                    "restore_applied",
                );
                kill_at(&dir, &["repair", "recover-restore"], origin);
            } else {
                kill_at(&dir, &["restore", &source, "--path", "a.md"], origin);
            }
            kill_at(&dir, &["repair", "recover-restore"], point);
            assert_eq!(ok(&dir, &["repair", "recover-restore"])["recovered"], true);
            assert_eq!(
                fs::read(dir.path().join("a.md")).unwrap(),
                b"new unique tulip phrase"
            );
            assert_eq!(
                fs::read_to_string(dir.path().join(".kio/HEAD"))
                    .unwrap()
                    .trim(),
                head
            );
        }
    }
}
