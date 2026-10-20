//! Known paid image result: a publication cut must recover without a second send.
use super::*;

fn image_source_rows(dir: &TempDir, image_hash: &str) -> i64 {
    let source = support::immutable_sqlite(dir.path().join(".kio/index/sqlite.db"));
    source
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE target_type='image' AND target_id=?1",
            [image_hash],
            |row| row.get(0),
        )
        .unwrap()
}

fn run_image_known_publication_cut(fault: bool) {
    let fixture = original_online_png_fixture();
    let dir = &fixture.dir;
    write_scope_config(dir, "[embedding]\nexternal_send = \"text_only\"\n");
    json_success_embed(dir, "mock", &["index", "--online", "--realtime"]);
    let preflight = json_success_embed(dir, "mock", &["status"]);
    let text = tasks_of_type(&preflight, "embedding")
        .into_iter()
        .filter(|task| task["embedding_target"]["kind"] == "chunk")
        .collect::<Vec<_>>();
    assert!(!text.is_empty(), "text control missing: {preflight}");
    assert!(text.iter().all(|task| task["status"] == "done"));
    let image = tasks_of_type(&preflight, "embedding")
        .into_iter()
        .find(|task| task["embedding_target"]["kind"] == "image")
        .expect("original PNG must have one image task");
    let original_target = image["embedding_target"].clone();
    let key = original_target["embedding_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(original_target["image_hash"], fixture.image_hash);
    assert_ne!(image["status"], "done");
    assert_eq!(image_source_rows(dir, &fixture.image_hash), 0);
    assert!(
        !ObjectStore::new(dir.path().join(".kio"))
            .embedding_hashes()
            .unwrap()
            .contains(&key)
    );

    write_scope_config(dir, "[embedding]\nexternal_send = \"text_and_images\"\n");
    let capture = dir.path().join("o3-image-known-publication-capture.jsonl");
    let attribution = dir
        .path()
        .join("o3-image-known-publication-attribution.json");
    let job_name = "batches/o3-image-known-publication";
    let pending = serde_json::json!({
        "job_name":job_name, "capture_path":capture,
        "attribution_path":attribution, "state_sequence":["BATCH_STATE_PENDING"]
    })
    .to_string();
    kio(dir, &["index", "--online"])
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env("KIO_TEST_GEMINI_BATCH", &pending)
        .arg("--json")
        .assert()
        .success();
    let creates = || {
        fs::read_to_string(&capture)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line["call"] == "create_embedding_job")
            .collect::<Vec<_>>()
    };
    assert_eq!(creates().len(), 1);
    assert_eq!(
        creates()[0]["keys"],
        serde_json::json!([fixture.image_hash])
    );

    // Close every connection before the CLI advances the original reservation.
    let original: (i64, String, i64, i64) = {
        let db = ledger_db(dir);
        let row: (i64, String, i64) = db.query_row(
            "SELECT submission_seq,intent_token,state FROM batch_requests WHERE adapter_kind='embedding' AND input_hash=?1",
            [&key], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))
        ).unwrap();
        let charges: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM cost_ledger WHERE adapter_kind='embedding' AND input_hash=?1",
                [&key],
                |row| row.get(0),
            )
            .unwrap();
        (row.0, row.1, row.2, charges)
    };
    assert_eq!(
        (original.2, original.3),
        (1, 0),
        "must be one pending paid attempt"
    );
    assert_eq!(
        creates()[0]["display_name"],
        kio_adapter::gemini_batch_client::batch_display_name(&original.1)
    );

    // Isolate embedding collection from optional OCR journal work in native ingestion.
    {
        let repo = Repository::open(dir.path()).unwrap();
        let store = kio_pipeline::task::TaskStore::new(repo.kio_dir());
        let tasks = store
            .all()
            .unwrap()
            .into_iter()
            .filter(|task| task.task_type == kio_pipeline::task::TaskType::Embedding)
            .collect::<Vec<_>>();
        store.replace_all(&tasks).unwrap();
    }
    let complete = serde_json::json!({
        "job_name":job_name, "capture_path":capture, "attribution_path":attribution,
        "state_sequence":["BATCH_STATE_SUCCEEDED"],
        "inlined_responses":[{"metadata":{"key":fixture.image_hash}, "response":{
            "embedding":{"values":(0..768).map(|i|if i==0 {1.0}else{0.0}).collect::<Vec<_>>()},
            "usageMetadata":{"promptTokenCount":7,
                "promptTokenDetails":[{"modality":"IMAGE","tokenCount":7}]}
        }}]
    })
    .to_string();
    let mut collect = kio(dir, &["batch", "resume"]);
    collect
        .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
        .env("KIO_TEST_GEMINI_BATCH", &complete)
        .arg("--json");
    if fault {
        collect.env(
            "KIO_TEST_EMBEDDING_PUBLICATION_FAULT",
            "before_source_write",
        );
    }
    let first = collect.output().unwrap();
    let diagnostic = format!(
        "exit={:?} stdout={} stderr={}",
        first.status.code(),
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    // The paid provider result is terminal before local image publication.
    // Capture and close the immutable state before any recovery CLI invocation.
    let settled: Vec<(i64, String, f64, bool, String)> = {
        let db = ledger_db(dir);
        let (seq,token,state):(i64,Option<String>,i64) = db.query_row(
            "SELECT submission_seq,intent_token,state FROM batch_requests WHERE adapter_kind='embedding' AND input_hash=?1",
            [&key], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))
        ).unwrap();
        assert_eq!((seq, state), (original.0, 2), "{diagnostic}");
        assert!(token.is_none(), "settlement must clear token: {diagnostic}");
        let mut statement = db.prepare("SELECT submission_seq,batch_job_id,usd,estimated,outcome FROM cost_ledger WHERE adapter_kind='embedding' AND input_hash=?1 ORDER BY submission_seq").unwrap();
        statement
            .query_map([&key], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(settled.len(), 1, "{diagnostic}");
    assert_eq!(
        (
            settled[0].0,
            settled[0].1.as_str(),
            settled[0].3,
            settled[0].4.as_str()
        ),
        (original.0, job_name, false, "succeeded"),
        "{diagnostic}"
    );
    assert!(
        (settled[0].2 - 7.0 * 0.000000225).abs() < 1e-15,
        "{diagnostic}"
    );
    assert_eq!(creates().len(), 1, "{diagnostic}");
    if fault {
        assert_eq!(
            image_source_rows(dir, &fixture.image_hash),
            0,
            "{diagnostic}"
        );
        assert!(
            !ObjectStore::new(dir.path().join(".kio"))
                .embedding_hashes()
                .unwrap()
                .contains(&key),
            "{diagnostic}"
        );
        let held = fs::read_to_string(dir.path().join(".kio/tasks.jsonl")).unwrap();
        assert!(
            held.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|task| task["type"] == "embedding"
                    && task["embedding_target"] == original_target
                    && task["status"] != "done"),
            "fault must leave original task unpublished: {held}"
        );
        // A no-fault new process must publish the staged known result locally.
        kio(dir, &["batch", "resume"])
            .env(TEST_ADOPTED_EMBEDDING_ENV, "mock")
            .env("KIO_TEST_GEMINI_BATCH", &complete)
            .arg("--json")
            .assert()
            .success();
    } else {
        assert!(first.status.success(), "positive control: {diagnostic}");
    }
    assert_eq!(image_source_rows(dir, &fixture.image_hash), 1);
    let record = ObjectStore::new(dir.path().join(".kio"))
        .read_embedding(&key)
        .unwrap();
    assert_eq!(record.target_type, "image");
    assert_eq!(record.target_hash, fixture.image_hash);
    assert_eq!(
        record.profile_hash,
        original_target["profile_hash"].as_str().unwrap()
    );
    assert_eq!(record.vector.len(), 768);
    assert_eq!(o2_golden_embedding_record(dir, &fixture.image_hash).0, key);
    let final_status = json_success_embed(dir, "mock", &["status"]);
    assert!(
        tasks_of_type(&final_status, "embedding")
            .into_iter()
            .any(|task| task["embedding_target"] == original_target && task["status"] == "done"),
        "known image target must reach Done: {final_status}"
    );
    let final_rows = {
        let db = ledger_db(dir);
        let mut statement = db.prepare("SELECT submission_seq,batch_job_id,usd,estimated,outcome FROM cost_ledger WHERE adapter_kind='embedding' AND input_hash=?1 ORDER BY submission_seq").unwrap();
        statement
            .query_map([&key], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        final_rows, settled,
        "publication must not create another charge"
    );
    assert_eq!(
        creates().len(),
        1,
        "publication must not send another paid job"
    );
}

#[test]
fn o3_image_known_result_control_publishes_once() {
    run_image_known_publication_cut(false);
}

#[test]
fn o3_image_known_result_after_publication_fault_recovers_without_resend() {
    run_image_known_publication_cut(true);
}
