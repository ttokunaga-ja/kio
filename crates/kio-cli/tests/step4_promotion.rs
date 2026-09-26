use std::fs;

use assert_cmd::Command;
use base64::Engine as _;
use kio_adapter::tool_lock::tool_lock_hash;
use kio_core::scope::Repository;
use kio_pipeline::markdownize::load_validated_normalized_instance;
use kio_pipeline::prepare::UnitType;
use kio_pipeline::task::{
    TaskOutputRef, TaskStatus, TaskStore, TaskType, validate_task_output_ref,
};
use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::TempDir;

const ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
    "KIO_TEST_PROMOTION_FAULT",
];

fn kio(dir: &TempDir, args: &[&str], online_mock: Option<&str>) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in ENV_DENYLIST {
        command.env_remove(name);
    }
    command
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .args(args)
        .arg("--json");
    if let Some(mock) = online_mock {
        command.env("KIO_TEST_MISTRAL_OCR", mock);
    }
    command
}

fn json_success(dir: &TempDir, args: &[&str], online_mock: Option<&str>) -> Value {
    let bytes = kio(dir, args, online_mock)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&bytes).unwrap()
}

fn init(dir: &TempDir) {
    json_success(dir, &["init"], None);
    json_success(dir, &["ledger", "init"], None);
    json_success(
        dir,
        &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
        Some("mock"),
    );
}

fn head(dir: &TempDir) -> String {
    fs::read_to_string(dir.path().join(".kio/HEAD"))
        .unwrap()
        .trim()
        .to_owned()
}

fn fake_pdf(pages: &[&str]) -> String {
    let kids = (0..pages.len())
        .map(|index| format!("{} 0 R", index + 2))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = format!(
        "%PDF-1.4\n1 0 obj << /Type /Pages /Kids [{kids}] /Count {} >> endobj\n",
        pages.len()
    );
    for (index, page) in pages.iter().enumerate() {
        out.push_str(&format!(
            "{} 0 obj << /Type /Page /Parent 1 0 R >> stream\nBT ({page}) Tj ET\nendstream endobj\n",
            index + 2
        ));
    }
    out.push_str("%%EOF\n");
    out
}

/// A full, deterministically-ordered snapshot of `cost-ledger.sqlite`'s two
/// tables, for byte-for-byte-style before/after equality checks (the retired
/// JSONL charge ledger's `fs::read` comparison this replaces relied on the
/// file being append-only — comparing every row of both tables here is the
/// direct SQLite-era equivalent of "nothing new was charged or reserved").
fn cost_ledger(dir: &TempDir) -> Vec<String> {
    let conn = Connection::open(dir.path().join(".test-data/kio/cost-ledger.sqlite")).unwrap();
    let mut rows = Vec::new();
    let mut stmt = conn
        .prepare(
            "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq, \
             batch_job_id, usd, estimated, outcome, month FROM cost_ledger \
             ORDER BY scope_id, adapter_kind, input_hash, tool_profile_hash, submission_seq",
        )
        .unwrap();
    let mapped = stmt
        .query_map([], |row| {
            Ok(format!(
                "cost_ledger:{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, f64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
            ))
        })
        .unwrap();
    for row in mapped {
        rows.push(row.unwrap());
    }
    drop(stmt);
    let mut stmt = conn
        .prepare(
            "SELECT scope_id, adapter_kind, input_hash, tool_profile_hash, state, \
             request_kind, estimated_usd, error FROM batch_requests \
             ORDER BY scope_id, adapter_kind, input_hash, tool_profile_hash",
        )
        .unwrap();
    let mapped = stmt
        .query_map([], |row| {
            Ok(format!(
                "batch_requests:{}|{}|{}|{}|{}|{}|{}|{}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, f64>(6)?,
                row.get::<_, Option<String>>(7)?.unwrap_or_default(),
            ))
        })
        .unwrap();
    for row in mapped {
        rows.push(row.unwrap());
    }
    rows
}

fn promotion_state_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join(".kio/promotion-state.json")
}

fn sqlite_head_rows(dir: &TempDir, commit_hash: &str) -> i64 {
    let conn = Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM tree_entries WHERE commit_hash = ?1",
        params![commit_hash],
        |row| row.get(0),
    )
    .unwrap()
}

fn assert_live_tool_lock_matches_head(dir: &TempDir) {
    let current = head(dir);
    let repo = Repository::open(dir.path()).unwrap();
    let commit = repo.read_commit(&current).unwrap();
    let tool_lock: Value =
        serde_json::from_slice(&fs::read(dir.path().join(".kio/tool-lock.json")).unwrap()).unwrap();
    assert_eq!(commit.tool_lock_hash, tool_lock_hash(&tool_lock).unwrap());
}

fn faulted_batch(dir: &TempDir, phase: &str) -> Value {
    let output = kio(dir, &["batch", "resume"], Some("mock"))
        .env("KIO_TEST_PROMOTION_FAULT", phase)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let response: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(response["error_code"], "KIO-E-PROMOTION-FAULT-001");
    assert_eq!(response["context"]["phase"], phase);
    response
}

#[test]
fn ct4_promotion_done_batch_updates_provenance_search_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    init(&dir);
    fs::write(
        dir.path().join("a.pdf"),
        fake_pdf(&["promotion fixture alpha"]),
    )
    .unwrap();
    fs::write(
        dir.path().join("b.pdf"),
        fake_pdf(&["promotion fixture beta"]),
    )
    .unwrap();
    json_success(&dir, &["index", "--yes"], None);
    let baseline_head = head(&dir);

    json_success(&dir, &["batch", "resume"], Some("mock"));
    let promoted_head = head(&dir);
    assert_ne!(promoted_head, baseline_head, "one batch must advance HEAD");

    let repo = Repository::open(dir.path()).unwrap();
    let commit = repo.read_commit(&promoted_head).unwrap();
    assert_eq!(
        commit.parent,
        Some(baseline_head),
        "the two accepted files must promote in one batch commit"
    );
    let tree = repo.read_tree(&commit.tree).unwrap();
    let mut promoted_profiles = tree
        .entries
        .iter()
        .filter_map(|entry| {
            entry
                .normalize
                .as_ref()
                .map(|value| value.tool_profile_hash.clone())
        })
        .collect::<Vec<_>>();
    promoted_profiles.sort();
    promoted_profiles.dedup();
    assert_eq!(
        promoted_profiles.len(),
        1,
        "the mock batch uses one immutable profile"
    );
    let profile_hash = &promoted_profiles[0];
    assert!(tree.entries.iter().all(|entry| {
        entry
            .normalize
            .as_ref()
            .is_some_and(|normalize| normalize.tool_profile_hash == *profile_hash)
    }));

    let tool_lock: Value =
        serde_json::from_slice(&fs::read(dir.path().join(".kio/tool-lock.json")).unwrap()).unwrap();
    assert_eq!(tool_lock["markdown"]["profile_hash"], profile_hash.as_str());
    assert_eq!(tool_lock["markdown"]["kind"], "online_api");
    assert!(tool_lock["markdown"].get("url").is_none());
    assert!(tool_lock["markdown"].get("auth").is_none());
    assert_eq!(commit.tool_lock_hash, tool_lock_hash(&tool_lock).unwrap());

    let conn = Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunks WHERE tool_profile_hash = ?1",
            params![profile_hash],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        count >= 2,
        "both files must be rebuilt under the promoted profile"
    );
    // Windows does not allow the later atomic index swap to replace sqlite.db
    // while this read connection is still open. Keep the assertion scoped to
    // the database observation it needs, just as a real search process would
    // close its handle before a subsequent index process replaces the database.
    drop(conn);
    let search = json_success(
        &dir,
        &["search", "mock ocr", "--scope", ".", "--mode", "text"],
        None,
    );
    assert!(search["results"].as_array().unwrap().len() >= 2);
    let bbox_search = json_success(
        &dir,
        &["search", "1000", "--scope", ".", "--mode", "text"],
        None,
    );
    assert!(
        bbox_search["results"].as_array().unwrap().len() >= 2,
        "promoted bbox transcriptions must be searchable"
    );

    let charged = cost_ledger(&dir);
    json_success(&dir, &["batch", "resume"], Some("mock"));
    assert_eq!(
        head(&dir),
        promoted_head,
        "repeated resume must not recommit"
    );
    assert_eq!(
        cost_ledger(&dir),
        charged,
        "repeated resume must not recharge"
    );

    // Ordinary index reconciles the Done online instances before its one snapshot;
    // it must not demote to baseline or create a second promotion commit.
    json_success(&dir, &["index", "--yes"], Some("mock"));
    assert_eq!(head(&dir), promoted_head);
    assert_eq!(cost_ledger(&dir), charged);
}

#[test]
fn ct4_bbox_006_ocr_from_scratch_promotes_scanned_pdf_and_image() {
    let dir = tempfile::tempdir().unwrap();
    init(&dir);
    // Neither input has locally extractable text. The provider response must become
    // the first canonical Prepared unit set (page:1 and image:0 respectively).
    fs::write(
        dir.path().join("scan.pdf"),
        b"%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\n%%EOF\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("diagram.png"),
        base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGP4DwQACfsD/fteaysAAAAASUVORK5CYII=")
            .unwrap(),
    )
    .unwrap();

    json_success(&dir, &["index", "--yes"], None);
    let baseline_head = head(&dir);
    json_success(&dir, &["batch", "resume"], Some("mock"));
    let promoted_head = head(&dir);
    assert_ne!(
        promoted_head, baseline_head,
        "OCR-from-scratch completion must promote a new HEAD"
    );

    let repo = Repository::open(dir.path()).unwrap();
    let store = TaskStore::new(repo.kio_dir());
    let tasks = store
        .all()
        .unwrap()
        .into_iter()
        .filter(|task| task.task_type == TaskType::Markdownize)
        .collect::<Vec<_>>();
    assert_eq!(tasks.len(), 2, "one durable task per input, without churn");
    for task in &tasks {
        assert_eq!(task.status, TaskStatus::Done);
        let TaskOutputRef::NormalizedInstance {
            raw_hash,
            tool_profile_hash,
            r#gen,
            ..
        } = validate_task_output_ref(repo.kio_dir(), task).unwrap()
        else {
            panic!("Done OCR task must retain a typed normalized output_ref");
        };
        let instance = load_validated_normalized_instance(
            repo.kio_dir(),
            &raw_hash,
            &tool_profile_hash,
            r#gen,
        )
        .unwrap();
        assert_eq!(instance.manifest.units.len(), 1);
        assert_eq!(instance.units.len(), 1);
        let expected_key = if task.input_path.ends_with(".pdf") {
            "page:1"
        } else {
            "image:0"
        };
        let expected_type = if task.input_path.ends_with(".pdf") {
            UnitType::Page
        } else {
            UnitType::Image
        };
        assert_eq!(instance.manifest.units[0].unit_key, expected_key);
        assert_eq!(instance.manifest.units[0].unit_type, expected_type);
        assert_eq!(instance.manifest.units[0].prepared_hash, raw_hash);
        assert_eq!(instance.units[0].unit_key, expected_key);
        assert_eq!(instance.units[0].unit_type, expected_type);
    }

    let search = json_success(
        &dir,
        &["search", "mock ocr", "--scope", ".", "--mode", "text"],
        None,
    );
    assert!(search["results"].as_array().unwrap().len() >= 2);
    let bbox_search = json_success(
        &dir,
        &["search", "1000", "--scope", ".", "--mode", "text"],
        None,
    );
    assert!(
        bbox_search["results"].as_array().unwrap().len() >= 2,
        "bbox annotations discovered from both inputs must be searchable"
    );

    let charged = cost_ledger(&dir);
    json_success(&dir, &["batch", "resume"], Some("mock"));
    json_success(&dir, &["index", "--yes"], Some("mock"));
    assert_eq!(head(&dir), promoted_head, "idle retries must not recommit");
    assert_eq!(cost_ledger(&dir), charged, "idle retries must not recharge");
    assert_eq!(
        TaskStore::new(repo.kio_dir())
            .all()
            .unwrap()
            .into_iter()
            .filter(|task| task.task_type == TaskType::Markdownize)
            .count(),
        2,
        "idle retries must not append duplicate tasks"
    );
}

#[test]
fn ct4_promotion_respects_bbox_disabled_profile_identity() {
    let dir = tempfile::tempdir().unwrap();
    init(&dir);
    let config_path = dir.path().join(".kio/config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[markdownize]\nbbox_annotation = false\n");
    fs::write(config_path, config).unwrap();
    // A grant binds the immutable processing profile, including bbox policy.
    // Profile drift must be explicitly re-approved before the mock OCR send.
    json_success(
        &dir,
        &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
        Some("mock"),
    );
    fs::write(
        dir.path().join("no-bbox.pdf"),
        fake_pdf(&["promotion without bbox"]),
    )
    .unwrap();
    json_success(&dir, &["index", "--yes"], None);
    let baseline_head = head(&dir);

    json_success(&dir, &["batch", "resume"], Some("mock"));
    assert_ne!(
        head(&dir),
        baseline_head,
        "a valid bbox-disabled immutable profile must promote"
    );
    let search = json_success(
        &dir,
        &["search", "mock ocr", "--scope", ".", "--mode", "text"],
        None,
    );
    assert_eq!(search["results"].as_array().unwrap().len(), 1);
}

#[test]
fn ct4_idempotent_mixed_profile_resume_uses_current_bbox_policy() {
    let dir = tempfile::tempdir().unwrap();
    init(&dir);
    fs::write(dir.path().join("a.pdf"), fake_pdf(&["bbox true first"])).unwrap();
    json_success(&dir, &["index", "--yes"], None);
    json_success(&dir, &["batch", "resume"], Some("mock"));
    let tool_lock_path = dir.path().join(".kio/tool-lock.json");
    let true_policy_tool_lock = fs::read(&tool_lock_path).unwrap();

    let config_path = dir.path().join(".kio/config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[markdownize]\nbbox_annotation = false\n");
    fs::write(&config_path, config).unwrap();
    json_success(
        &dir,
        &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
        Some("mock"),
    );
    fs::write(dir.path().join("b.pdf"), fake_pdf(&["bbox false second"])).unwrap();
    json_success(&dir, &["index", "--yes"], None);

    // The config toggle also enqueues a false-policy replacement for a.pdf. Keep
    // that task non-live so this pass produces a deliberately mixed HEAD: the
    // accepted true-policy a.pdf plus the accepted false-policy b.pdf.
    let repo = Repository::open(dir.path()).unwrap();
    let store = TaskStore::new(repo.kio_dir());
    store
        .update_matching(|task| {
            if task.input_path == "a.pdf" && task.status == TaskStatus::Pending {
                task.status = TaskStatus::Failed;
                task.fallback_reason = Some("invalid_input".to_owned());
                true
            } else {
                false
            }
        })
        .unwrap();
    json_success(&dir, &["batch", "resume"], Some("mock"));

    let mixed_head = head(&dir);
    let config = fs::read_to_string(&config_path)
        .unwrap()
        .replace("bbox_annotation = false", "bbox_annotation = true");
    fs::write(&config_path, config).unwrap();
    json_success(
        &dir,
        &["adapter", "approve", "mistral_ocr_markdownize", "--yes"],
        Some("mock"),
    );

    // The false-policy replacement was explicitly terminalized above. Restoring
    // the current policy must not revive a terminal task or manufacture a new
    // promotion; it only selects the configured profile for this no-op resume.
    json_success(&dir, &["batch", "resume"], Some("mock"));
    assert_eq!(head(&dir), mixed_head, "idempotent resume must not commit");
    assert_eq!(
        fs::read(&tool_lock_path).unwrap(),
        true_policy_tool_lock,
        "a noop must select the configured bbox profile, not path ordering"
    );
}

#[test]
fn ct4_promotion_partial_and_stale_outputs_never_advance_head() {
    let partial = tempfile::tempdir().unwrap();
    init(&partial);
    fs::write(
        partial.path().join("partial.pdf"),
        fake_pdf(&["promotion partial one", "promotion partial two"]),
    )
    .unwrap();
    json_success(&partial, &["index", "--yes"], None);
    let partial_head = head(&partial);
    let output = kio(&partial, &["batch", "resume"], Some("partial"))
        .assert()
        .code(4)
        .get_output()
        .stdout
        .clone();
    let response: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(response["tasks_attempted"], 1);
    assert_eq!(response["tasks_executed"], 0);
    assert_eq!(response["tasks_failed"], 1);
    assert_eq!(
        head(&partial),
        partial_head,
        "Partial output must not promote"
    );

    let stale = tempfile::tempdir().unwrap();
    init(&stale);
    fs::write(
        stale.path().join("stale.pdf"),
        fake_pdf(&["promotion stale old"]),
    )
    .unwrap();
    json_success(&stale, &["index", "--yes"], None);
    let stale_head = head(&stale);
    fs::write(
        stale.path().join("stale.pdf"),
        fake_pdf(&["promotion stale changed"]),
    )
    .unwrap();
    let output = kio(&stale, &["batch", "resume"], Some("mock"))
        .assert()
        .code(4)
        .get_output()
        .stdout
        .clone();
    let response: Value = serde_json::from_slice(&output).unwrap();
    assert!(response["tasks_failed"].as_u64().unwrap() > 0);
    assert_eq!(
        head(&stale),
        stale_head,
        "edited deferred input must not promote"
    );
}

#[test]
fn ct4_promotion_004_fault_before_head_preserves_old_live_tool_lock_and_retries_once() {
    let dir = tempfile::tempdir().unwrap();
    init(&dir);
    fs::write(
        dir.path().join("before-head.pdf"),
        fake_pdf(&["promotion before head fault"]),
    )
    .unwrap();
    json_success(&dir, &["index", "--yes"], None);
    let baseline_head = head(&dir);
    let baseline_tool_lock = fs::read(dir.path().join(".kio/tool-lock.json")).unwrap();

    faulted_batch(&dir, "before_head");
    assert_eq!(head(&dir), baseline_head, "HEAD must remain unpromoted");
    assert_eq!(
        fs::read(dir.path().join(".kio/tool-lock.json")).unwrap(),
        baseline_tool_lock,
        "a pre-HEAD failure must not publish the staged online tool-lock"
    );
    assert!(
        promotion_state_path(&dir).is_file(),
        "the prepared transaction must be discoverable for retry"
    );
    let charged = cost_ledger(&dir);

    json_success(&dir, &["batch", "resume"], Some("mock"));
    let promoted_head = head(&dir);
    assert_ne!(promoted_head, baseline_head);
    let repo = Repository::open(dir.path()).unwrap();
    assert_eq!(
        repo.read_commit(&promoted_head).unwrap().parent,
        Some(baseline_head),
        "retry must publish exactly one promotion commit"
    );
    assert_live_tool_lock_matches_head(&dir);
    assert!(!promotion_state_path(&dir).exists());
    assert_eq!(
        cost_ledger(&dir),
        charged,
        "retrying publication must not recharge the completed task"
    );
}

#[test]
fn ct4_promotion_004_after_head_and_after_index_swap_faults_converge() {
    let after_head = tempfile::tempdir().unwrap();
    init(&after_head);
    fs::write(
        after_head.path().join("after-head.pdf"),
        fake_pdf(&["promotion after head fault"]),
    )
    .unwrap();
    json_success(&after_head, &["index", "--yes"], None);
    let baseline_head = head(&after_head);

    faulted_batch(&after_head, "after_head");
    let promoted_head = head(&after_head);
    assert_ne!(
        promoted_head, baseline_head,
        "HEAD publication must survive"
    );
    assert_live_tool_lock_matches_head(&after_head);
    assert!(promotion_state_path(&after_head).is_file());
    assert_eq!(
        sqlite_head_rows(&after_head, &promoted_head),
        0,
        "the after-HEAD seam must fire before SQLite publication"
    );
    let rebuilding = kio(
        &after_head,
        &["search", "mock ocr", "--scope", ".", "--mode", "text"],
        None,
    )
    .assert()
    .code(3)
    .get_output()
    .stderr
    .clone();
    let rebuilding: Value = serde_json::from_slice(&rebuilding).unwrap();
    assert_eq!(
        rebuilding["error_code"], "KIO-E-INDEX-REBUILDING-001",
        "the post-HEAD/pre-swap window must be loud, never a false empty result"
    );
    let charged = cost_ledger(&after_head);

    json_success(&after_head, &["repair", "rebuild-db"], None);
    assert_eq!(head(&after_head), promoted_head);
    assert!(sqlite_head_rows(&after_head, &promoted_head) > 0);
    assert!(!promotion_state_path(&after_head).exists());
    assert_eq!(cost_ledger(&after_head), charged);
    let search = json_success(
        &after_head,
        &["search", "mock ocr", "--scope", ".", "--mode", "text"],
        None,
    );
    assert_eq!(search["results"].as_array().unwrap().len(), 1);

    let after_swap = tempfile::tempdir().unwrap();
    init(&after_swap);
    fs::write(
        after_swap.path().join("after-swap.pdf"),
        fake_pdf(&["promotion after index swap fault"]),
    )
    .unwrap();
    json_success(&after_swap, &["index", "--yes"], None);
    let baseline_head = head(&after_swap);

    faulted_batch(&after_swap, "after_index_swap");
    let promoted_head = head(&after_swap);
    assert_ne!(promoted_head, baseline_head);
    assert_live_tool_lock_matches_head(&after_swap);
    assert!(promotion_state_path(&after_swap).is_file());
    assert!(
        sqlite_head_rows(&after_swap, &promoted_head) > 0,
        "the index-swap seam must fire only after the new database is live"
    );
    let charged = cost_ledger(&after_swap);

    json_success(&after_swap, &["batch", "resume"], Some("mock"));
    assert_eq!(
        head(&after_swap),
        promoted_head,
        "resuming a published promotion must not create another commit"
    );
    assert!(!promotion_state_path(&after_swap).exists());
    assert_eq!(cost_ledger(&after_swap), charged);
    assert!(sqlite_head_rows(&after_swap, &promoted_head) > 0);
}
