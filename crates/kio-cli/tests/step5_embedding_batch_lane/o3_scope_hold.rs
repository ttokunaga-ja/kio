use super::*;

/// A typed violation belongs to the scope/adapter, not the old member set.
/// Complete text over separately approved Sync first so the existing text
/// Batch breaker cannot mask a missing image Batch breaker.
#[test]
fn fresh_image_batch_cannot_bypass_another_member_sets_typed_contract_hold() {
    kio_index::vec::ensure_registered();
    let dir = scope();
    let old_capture = root(&dir).join("old-text-hold-capture.jsonl");
    let old_job = "batches/o3-original-text-hold";
    let pending = serde_json::json!({
        "state_sequence": ["BATCH_STATE_PENDING"],
        "job_name": old_job,
        "capture_path": old_capture,
    })
    .to_string();
    json(&dir, &pending, &["index", "--online"]);
    let old_keys = submitted_keys(&old_capture);
    assert_eq!(old_keys.len(), 1);
    let foreign = kio_core::cas::hash_bytes(b"not the submitted text member");
    assert_ne!(foreign, old_keys[0]);
    let component = 1.0f64 / (768.0f64).sqrt();
    let malformed = serde_json::json!({
        "state_sequence": ["BATCH_STATE_SUCCEEDED"],
        "job_name": old_job,
        "capture_path": old_capture,
        "inlined_responses": [{
            "metadata": {"key": foreign},
            "output": {"response": {
                "embedding": {"values": vec![component; 768]},
                "usageMetadata": {"promptTokenCount": 7,
                    "promptTokenDetails": [{"modality":"TEXT","tokenCount":7}]},
            }},
        }],
    })
    .to_string();
    run_ignoring_output(&dir, &malformed, &["batch", "resume"]);
    // This is the durable hold authority: exact current key/sequence joined to
    // a typed terminal invoice, not the error string or a manually written row.
    let held_attempt = || {
        let ledger = support::immutable_sqlite(dir.path().join("home/data/kio/cost-ledger.sqlite"));
        ledger
            .query_row(
                "SELECT b.input_hash,b.tool_profile_hash,b.submission_seq,
                        b.contract_violation_count,l.outcome,l.usd,l.estimated,
                        b.state,b.intent_token IS NULL
                 FROM batch_requests b JOIN cost_ledger l
                 USING(scope_id,adapter_kind,input_hash,tool_profile_hash,submission_seq)
                 WHERE b.adapter_kind='embedding' AND b.batch_job_id=?1",
                [old_job],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, f64>(5)?,
                        row.get::<_, bool>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, bool>(8)?,
                    ))
                },
            )
            .unwrap()
    };
    let original_hold = held_attempt();
    assert_eq!(original_hold.3, 1);
    assert_eq!(original_hold.4, "contract_violation");
    assert_eq!(original_hold.7, 3);
    assert!(original_hold.8);

    let png = include_bytes!("../fixtures/o2_golden/png-original-control.png");
    let image_hash = kio_core::cas::hash_bytes(png);
    std::fs::write(root(&dir).join("fresh-scan.png"), png).unwrap();
    // Establish the real standalone owner/CAS before online image planning.
    json(&dir, &pending, &["index", "--offline"]);
    std::fs::write(
        root(&dir).join(".kio/config.toml"),
        "[embedding]\nexternal_send = \"text_only\"\n",
    )
    .unwrap();
    json(&dir, &pending, &["index", "--online", "--realtime"]);
    assert_eq!(
        held_attempt(),
        original_hold,
        "Sync cannot reset the old Batch hold"
    );
    {
        let source = support::immutable_sqlite(root(&dir).join(".kio/index/sqlite.db"));
        let (paths, missing, images): (i64, i64, i64) = source
            .query_row(
                "SELECT (SELECT COUNT(DISTINCT raw_path) FROM chunks),
                        (SELECT COUNT(*) FROM chunks c WHERE NOT EXISTS (
                            SELECT 1 FROM embeddings e WHERE e.target_type='chunk'
                            AND e.target_id=c.text_hash)),
                        (SELECT COUNT(*) FROM image_vec)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(
            paths >= 2,
            "fixture includes old text and the real PNG owner"
        );
        assert_eq!(
            missing, 0,
            "all text must be materialized before testing image dispatch"
        );
        assert_eq!(images, 0, "text_only must not have sent the fresh image");
    }
    let source = json(&dir, &pending, &["search", "fresh-scan", "--mode", "text"]);
    let owner = source["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["evidence_pointer"]["path_at_commit"] == "fresh-scan.png")
        .expect("offline baseline must establish the authentic standalone PNG owner");
    assert_eq!(owner["evidence_pointer"]["raw_hash"], image_hash);
    let tasks = std::fs::read_to_string(root(&dir).join(".kio/tasks.jsonl")).unwrap();
    let image_task = tasks
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .rfind(|task| {
            task["type"] == "embedding"
                && task["embedding_target"]["kind"] == "image"
                && task["embedding_target"]["image_hash"] == image_hash
        })
        .expect("actual owner planning must produce the typed image task");
    let image_key = image_task["embedding_target"]["embedding_hash"]
        .as_str()
        .unwrap();
    assert_ne!(image_key, original_hold.0);
    assert_eq!(
        image_task["embedding_target"]["image_input_receipt"],
        Value::Null
    );
    let image_attempts = || {
        let ledger = support::immutable_sqlite(dir.path().join("home/data/kio/cost-ledger.sqlite"));
        ledger
            .query_row(
                "SELECT (SELECT COUNT(*) FROM batch_requests WHERE adapter_kind='embedding' AND input_hash=?1),
                        (SELECT COUNT(*) FROM cost_ledger WHERE adapter_kind='embedding' AND input_hash=?1)",
                [image_key],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap()
    };
    assert_eq!(
        image_attempts(),
        (0, 0),
        "fresh image has no old attempt to reuse"
    );
    std::fs::write(
        root(&dir).join(".kio/config.toml"),
        "[embedding]\nexternal_send = \"text_and_images\"\n",
    )
    .unwrap();
    let capture = root(&dir).join("fresh-image-hold-capture.jsonl");
    let new_job = serde_json::json!({
        "state_sequence": ["BATCH_STATE_PENDING"],
        "job_name": "batches/o3-forbidden-fresh-image",
        "capture_path": capture,
    })
    .to_string();
    let output = kio(&dir)
        .env(
            "KIO_TEST_GEMINI_BATCH",
            script_with_attribution(&dir, &new_job),
        )
        .args(["--json", "index", "--online"])
        .output()
        .unwrap();
    let creates = std::fs::read_to_string(&capture)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| row["call"] == "create_embedding_job")
        .count();
    // Primary O3 Runtime Red: new member sets must not bypass the typed scope
    // hold. The adapter mock records actual submits, not a helper return flag.
    assert_eq!(
        creates,
        0,
        "held scope submitted a fresh image: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        image_attempts(),
        (0, 0),
        "held image must not reserve or settle"
    );
    assert_eq!(output.status.code(), Some(4));
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diagnostic.contains("KIO-E-EMBED-BATCH-CONTRACT-HOLD-001"),
        "{diagnostic}"
    );
    assert_eq!(
        held_attempt(),
        original_hold,
        "new work must preserve old collection/terminal audit"
    );
    let source = support::immutable_sqlite(root(&dir).join(".kio/index/sqlite.db"));
    let images: i64 = source
        .query_row("SELECT COUNT(*) FROM image_vec", [], |row| row.get(0))
        .unwrap();
    assert_eq!(images, 0);
}
