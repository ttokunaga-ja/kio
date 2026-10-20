//! Image-only sync accounting: mock dispatch observations, not real HTTP traffic.
use super::*;

const NOW: &str = "2026-10-20T00:00:00Z";
// Exact documented USD upper bound: 8192 IMAGE tokens at $0.45 / million.
const IMAGE_UPPER_BOUND: f64 = 0.0036864;

fn fixture() -> OriginalOnlinePngFixture {
    let fixture = original_online_png_fixture();
    write_scope_config(&fixture.dir, "[embedding]\nexternal_send = \"text_only\"\n");
    json_success_embed(&fixture.dir, "mock", &["index", "--online", "--realtime"]);
    let status = json_success_embed(&fixture.dir, "mock", &["status"]);
    let text = tasks_of_type(&status, "embedding")
        .into_iter()
        .filter(|task| task["embedding_target"]["kind"] == "chunk")
        .collect::<Vec<_>>();
    assert!(!text.is_empty());
    assert!(text.iter().all(|task| task["status"] == "done"), "{status}");
    let image = image_task(&fixture);
    assert_ne!(image["status"], "done");
    assert!(image["reservation_id"].is_null());
    assert_no_projection(&fixture);
    write_scope_config(
        &fixture.dir,
        "[embedding]\nexternal_send = \"text_and_images\"\n",
    );
    fixture
}

fn image_task(fixture: &OriginalOnlinePngFixture) -> Value {
    let repo = Repository::open(fixture.dir.path()).unwrap();
    let rows = kio_pipeline::task::TaskStore::new(repo.kio_dir()).all().unwrap()
        .into_iter().filter(|task| matches!(task.embedding_target.as_ref(),
            Some(kio_pipeline::task::EmbeddingTaskTarget::Image { image_hash, .. }) if image_hash == &fixture.image_hash))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "one original typed image task");
    serde_json::to_value(&rows[0]).unwrap()
}

fn assert_no_projection(fixture: &OriginalOnlinePngFixture) {
    kio_index::vec::ensure_registered();
    let db = support::immutable_sqlite(fixture.dir.path().join(".kio/index/sqlite.db"));
    let rows: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE target_type='image' AND target_id=?1",
            [&fixture.image_hash],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0, "failed image must not publish an embedding");
    let vectors: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM image_vec WHERE image_id=?1",
            [&fixture.image_hash],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(vectors, 0, "failed image must not publish a vector");
}

fn run(fixture: &OriginalOnlinePngFixture, selector: &str, now: &str, trace: &Path, args: &[&str]) {
    let output = kio(&fixture.dir, args)
        .env(TEST_ADOPTED_EMBEDDING_ENV, selector)
        .env("KIO_FIXED_NOW", now)
        .env("KIO_TEST_TASK_EMBED_TRACE", trace)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        serde_json::from_slice::<Value>(&output.stdout).is_ok(),
        "CLI must reach image runtime: exit={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn trace_rows(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assert_one_terminal_charge(
    fixture: &OriginalOnlinePngFixture,
    trace: &Path,
    expected_outcome: &str,
    usd: f64,
    estimated: bool,
) {
    let traces = trace_rows(trace);
    assert_eq!(
        traces.len(),
        1,
        "only the isolated IMAGE dispatch: {traces:?}"
    );
    assert_eq!(traces[0]["input_type"], "image");
    assert_eq!(
        traces[0]["member_ids"],
        serde_json::json!([fixture.image_hash])
    );
    let token = traces[0]["intent_token"].as_str().unwrap();
    assert!(!token.is_empty());
    let task = image_task(fixture);
    let target = &task["embedding_target"];
    let repo = Repository::open(fixture.dir.path()).unwrap();
    let scope = repo.scope_identity().unwrap().scope_id;
    drop(repo);
    let db = ledger_db(&fixture.dir);
    let mut query = db.prepare("SELECT b.submission_seq,b.state,b.intent_token,b.estimated_usd,c.submission_seq,c.batch_job_id,c.usd,c.estimated,c.outcome FROM batch_requests b JOIN cost_ledger c ON c.scope_id=b.scope_id AND c.adapter_kind=b.adapter_kind AND c.input_hash=b.input_hash AND c.tool_profile_hash=b.tool_profile_hash AND c.submission_seq=b.submission_seq WHERE b.scope_id=?1 AND b.adapter_kind='embedding' AND b.input_hash=?2 AND b.tool_profile_hash=?3").unwrap();
    let rows = query
        .query_map(
            rusqlite::params![
                scope,
                target["embedding_hash"].as_str().unwrap(),
                target["profile_hash"].as_str().unwrap()
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, f64>(6)?,
                    row.get::<_, bool>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "one original physical attempt and one terminal charge: {rows:?}"
    );
    let row = &rows[0];
    assert_eq!(row.0, row.4);
    assert_eq!(row.1, 3, "failed/rejected sync attempt is terminal");
    assert!(row.2.is_none(), "terminal settlement clears intent");
    assert_eq!(
        row.3, IMAGE_UPPER_BOUND,
        "reserve IMAGE upper bound, never TEXT price"
    );
    assert_eq!(
        row.5, token,
        "physical settlement binds exact dispatched intent"
    );
    assert_eq!(row.6, usd);
    assert_eq!(row.7, estimated);
    assert_eq!(row.8, expected_outcome);
}

#[test]
fn o3_image_sync_429_is_nonbillable_and_honors_retry_after() {
    let fixture = fixture();
    let evidence = canonical_tempdir();
    let trace = evidence.path().join("dispatch.jsonl");
    run(
        &fixture,
        "rate_limit_after",
        NOW,
        &trace,
        &["index", "--online", "--realtime"],
    );
    assert_one_terminal_charge(&fixture, &trace, "submit_rejected", 0.0, false);
    let task = image_task(&fixture);
    // Shared paid-work failure semantics: terminal audit is physical in the
    // device ledger; a fresh failed Task does not acquire a new claim stamp.
    for field in ["reservation_id", "reserved_usd", "reserved_month"] {
        assert!(task[field].is_null(), "fresh failed claim {field}: {task}");
    }
    assert_eq!(task["status"], "pending");
    assert_eq!(task["fallback_reason"], "rate_limit");
    assert_eq!(task["attempts"], 0, "429 does not consume retry budget");
    assert_eq!(task["next_retry_at"], "2026-10-20T00:00:30Z");
    assert_no_projection(&fixture);
    let ledger = o2_golden_ledger_rows(&fixture.dir);
    run(
        &fixture,
        "mock",
        "2026-10-20T00:00:29Z",
        &trace,
        &["index", "--online", "--realtime"],
    );
    assert_eq!(
        trace_rows(&trace).len(),
        1,
        "no dispatch before Retry-After"
    );
    assert_eq!(o2_golden_ledger_rows(&fixture.dir), ledger);
    assert_no_projection(&fixture);
}

#[test]
fn o3_image_sync_unknown_settles_image_upper_bound_and_fences_implicit_resend() {
    let fixture = fixture();
    let evidence = canonical_tempdir();
    let trace = evidence.path().join("dispatch.jsonl");
    run(
        &fixture,
        "network_error",
        NOW,
        &trace,
        &["index", "--online", "--realtime"],
    );
    assert_one_terminal_charge(&fixture, &trace, "unknown_settled", IMAGE_UPPER_BOUND, true);
    let task = image_task(&fixture);
    // Shared paid-work failure semantics: terminal audit is physical in the
    // device ledger; a fresh failed Task does not acquire a new claim stamp.
    for field in ["reservation_id", "reserved_usd", "reserved_month"] {
        assert!(task[field].is_null(), "fresh failed claim {field}: {task}");
    }
    assert_eq!(task["status"], "failed");
    assert_eq!(task["fallback_reason"], "result_unknown");
    assert_eq!(task["attempts"], 1);
    assert!(
        task["next_retry_at"].is_null(),
        "result_unknown has max_attempts=0"
    );
    assert_no_projection(&fixture);
    let ledger = o2_golden_ledger_rows(&fixture.dir);
    for args in [
        vec!["index", "--online", "--realtime"],
        vec!["batch", "retry", "--realtime"],
        vec!["batch", "resume"],
    ] {
        run(&fixture, "mock", "2026-10-20T00:01:00Z", &trace, &args);
        assert_eq!(
            trace_rows(&trace).len(),
            1,
            "no implicit resend via {args:?}"
        );
        assert_eq!(
            o2_golden_ledger_rows(&fixture.dir),
            ledger,
            "no fresh reservation/charge via {args:?}"
        );
        assert_no_projection(&fixture);
    }
}
