//! Reserve-to-send race: debug mock dispatch evidence, never a real HTTP claim.
use super::*;

const TASK_TRACE: &str = "KIO_TEST_TASK_EMBED_TRACE";

fn trace_rows(path: &Path) -> Vec<Value> {
    match fs::read_to_string(path) {
        Ok(raw) => raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("task trace {}: {error}", path.display()),
    }
}

fn image_projection_count(dir: &TempDir, hash: &str) -> i64 {
    support::immutable_sqlite(dir.path().join(".kio/index/sqlite.db"))
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE target_type='image' AND target_id=?1",
            [hash],
            |row| row.get(0),
        )
        .unwrap()
}

fn run_reserved_image(change_policy: bool) {
    // Public native-image ingestion creates the retained owner and immutable CAS.
    let fixture = original_online_png_fixture();
    let dir = &fixture.dir;
    write_scope_config(dir, "[embedding]\nexternal_send = \"text_only\"\n");
    json_success_embed(dir, "mock", &["index", "--online", "--realtime"]);
    let before = json_success_embed(dir, "mock", &["status"]);
    let images = tasks_of_type(&before, "embedding")
        .into_iter()
        .filter(|task| task["embedding_target"]["kind"] == "image")
        .collect::<Vec<_>>();
    assert_eq!(
        images.len(),
        1,
        "single-image fixture precondition: {before}"
    );
    let original_task = images[0].clone();
    let target = original_task["embedding_target"].clone();
    assert_eq!(target["image_hash"], fixture.image_hash);
    assert_ne!(original_task["status"], "done");
    assert!(original_task["reservation_id"].is_null());
    let text = tasks_of_type(&before, "embedding")
        .into_iter()
        .filter(|task| task["embedding_target"]["kind"] == "chunk")
        .collect::<Vec<_>>();
    assert!(
        !text.is_empty(),
        "fixture must contain a TEXT control: {before}"
    );
    assert!(
        text.iter().all(|task| task["status"] == "done"),
        "TEXT must already be completed: {before}"
    );
    let embedding_hash = target["embedding_hash"].as_str().unwrap().to_owned();
    let profile_hash = target["profile_hash"].as_str().unwrap().to_owned();
    let scope_id = {
        let repo = Repository::open(dir.path()).unwrap();
        assert_eq!(
            repo.object_store()
                .read_content_object_bytes(
                    ContentObjectKind::Image,
                    &fixture.image_hash,
                    16 * 1024 * 1024
                )
                .unwrap(),
            fixture.png
        );
        repo.scope_identity().unwrap().scope_id
    };
    assert_eq!(image_projection_count(dir, &fixture.image_hash), 0);
    {
        let db = ledger_db(dir);
        let count: i64 = db.query_row("SELECT COUNT(*) FROM batch_requests WHERE scope_id=?1 AND adapter_kind='embedding' AND input_hash=?2 AND tool_profile_hash=?3", rusqlite::params![scope_id, embedding_hash, profile_hash], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "fixture must have no previous image attempt");
    }
    // No Repository/SQLite handle survives the spawn or the config edit.
    write_scope_config(dir, "[embedding]\nexternal_send = \"text_and_images\"\n");
    let markers = canonical_tempdir();
    let ready = markers.path().join("image-reserved.ready");
    let release = ready.with_extension("release");
    let trace = markers.path().join("task-dispatch.jsonl");
    let stdout_path = markers.path().join("stdout");
    let stderr_path = markers.path().join("stderr");
    let bin = assert_cmd::cargo::cargo_bin("kio");
    let mut child = hermetic_process_command(&bin)
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env("KIO_TEST_DURABILITY_POINT", "embedding_reserved")
        .env("KIO_TEST_DURABILITY_READY", &ready)
        .env(TASK_TRACE, &trace)
        .args(["index", "--online", "--realtime", "--json"])
        .stdout(fs::File::create(&stdout_path).unwrap())
        .stderr(fs::File::create(&stderr_path).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        if let Some(exit) = child.try_wait().unwrap() {
            panic!(
                "SCAFFOLD: index exited before image reservation barrier: {exit}; stdout={} stderr={}",
                fs::read_to_string(&stdout_path).unwrap(),
                fs::read_to_string(&stderr_path).unwrap()
            );
        }
        if Instant::now() >= deadline {
            // Release first: a slow child may still arrive at its bounded hook.
            fs::write(&release, b"release").unwrap();
            let exit = child.wait().unwrap();
            panic!("SCAFFOLD: image reservation barrier missing; child={exit}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fs::read_to_string(&ready).unwrap(),
        format!("point=embedding_reserved\npid={}\n", child.id())
    );
    assert!(
        trace_rows(&trace).is_empty(),
        "SCAFFOLD: dispatch must follow reservation checkpoint"
    );
    let source_before = fs::read(dir.path().join("scan.png")).unwrap();
    let config_path = dir.path().join(".kio/config.toml");
    let config_before = fs::read_to_string(&config_path).unwrap();
    if change_policy {
        assert_eq!(config_before.matches("text_and_images").count(), 1);
        fs::write(
            &config_path,
            config_before.replacen("text_and_images", "text_only", 1),
        )
        .unwrap();
    }
    fs::write(&release, b"release").unwrap();
    let exit = child.wait().unwrap();
    let rows = trace_rows(&trace);
    let dispatched = rows
        .iter()
        .filter(|row| row["input_type"] == "image")
        .collect::<Vec<_>>();
    if change_policy {
        // Intended Runtime Red: independently observed forbidden dispatch first,
        // before interpreting CLI exit, publication, or terminal lifecycle.
        assert_eq!(
            dispatched.len(),
            0,
            "image dispatch after external_send became text_only: {rows:?}; exit={exit}"
        );
    } else {
        assert!(
            exit.success(),
            "SCAFFOLD positive control: stdout={} stderr={}",
            fs::read_to_string(&stdout_path).unwrap(),
            fs::read_to_string(&stderr_path).unwrap()
        );
        assert_eq!(
            dispatched.len(),
            1,
            "SCAFFOLD: mock task dispatch must be observable: {rows:?}"
        );
        assert_eq!(
            rows.len(),
            1,
            "TEXT must not create another request: {rows:?}"
        );
        assert_eq!(
            dispatched[0]["member_ids"],
            serde_json::json!([fixture.image_hash])
        );
        assert!(
            dispatched[0]["intent_token"]
                .as_str()
                .is_some_and(|token| !token.is_empty())
        );
    }
    assert_eq!(
        fs::read(dir.path().join("scan.png")).unwrap(),
        source_before
    );
    let after = json_success_embed(dir, "mock", &["status"]);
    let task = tasks_of_type(&after, "embedding")
        .into_iter()
        .find(|task| task["task_id"] == original_task["task_id"])
        .unwrap();
    assert_eq!(
        task["embedding_target"], target,
        "same original typed target"
    );
    if !change_policy {
        assert_metered_sync_image_completion(dir, task);
        let db = ledger_db(dir);
        let token: String = db.query_row("SELECT batch_job_id FROM cost_ledger WHERE scope_id=?1 AND adapter_kind='embedding' AND input_hash=?2 AND tool_profile_hash=?3 AND outcome='succeeded'", rusqlite::params![scope_id, embedding_hash, profile_hash], |row| row.get(0)).unwrap();
        assert_eq!(
            dispatched[0]["intent_token"], token,
            "trace must attribute the original physical ledger attempt"
        );
        return;
    }
    assert_ne!(task["status"], "done");
    assert!(
        task["reservation_id"].is_null(),
        "unsent task must not retain a reusable claim: {task}"
    );
    assert_eq!(image_projection_count(dir, &fixture.image_hash), 0);
    {
        let db = ledger_db(dir);
        let (attempts, open): (i64, i64) = db.query_row("SELECT COUNT(*),COALESCE(SUM(CASE WHEN state IN (0,1) OR intent_token IS NOT NULL THEN 1 ELSE 0 END),0) FROM batch_requests WHERE scope_id=?1 AND adapter_kind='embedding' AND input_hash=?2 AND tool_profile_hash=?3", rusqlite::params![scope_id, embedding_hash, profile_hash], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(
            attempts, 1,
            "checkpoint must belong to one real reserved image attempt"
        );
        assert_eq!(
            open, 0,
            "proven-unsent attempt must not remain reusable/in-flight"
        );
        let invalid_charges: i64 = db.query_row("SELECT COUNT(*) FROM cost_ledger WHERE scope_id=?1 AND adapter_kind='embedding' AND input_hash=?2 AND tool_profile_hash=?3 AND (usd<>0 OR estimated<>0 OR outcome='succeeded')", rusqlite::params![scope_id, embedding_hash, profile_hash], |row| row.get(0)).unwrap();
        assert_eq!(
            invalid_charges, 0,
            "no positive/unknown/succeeded charge for a call proven unsent"
        );
        let settlements: i64 = db.query_row("SELECT COUNT(*) FROM cost_ledger WHERE scope_id=?1 AND adapter_kind='embedding' AND input_hash=?2 AND tool_profile_hash=?3 AND usd=0 AND estimated=0", rusqlite::params![scope_id, embedding_hash, profile_hash], |row| row.get(0)).unwrap();
        assert_eq!(
            settlements, 1,
            "proven-unsent reservation requires one durable zero settlement"
        );
    }
    let settled = o2_golden_ledger_rows(dir);
    kio(dir, &["index", "--online", "--realtime", "--json"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env(TASK_TRACE, &trace)
        .output()
        .unwrap();
    assert_eq!(
        trace_rows(&trace),
        rows,
        "text_only must not implicitly resend"
    );
    assert_eq!(
        o2_golden_ledger_rows(dir),
        settled,
        "denied attempt remains settled without new reservation"
    );
    assert_eq!(image_projection_count(dir, &fixture.image_hash), 0);
}

#[test]
fn o3_image_reserved_positive_control_dispatches_exactly_once() {
    run_reserved_image(false);
}

#[test]
fn o3_image_reserved_then_text_only_sends_no_image_and_clears_unsent_claim() {
    // This exact test establishes its own instrumentation positive control first.
    run_reserved_image(false);
    run_reserved_image(true);
}
