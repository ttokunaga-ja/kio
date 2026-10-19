//! Postattempt query acceptance at the physical-ledger boundary.
use super::{QueryEmbeddingOutcome, run_query_embedding_job};
use kio_adapter::catalog::{AdoptedEmbeddingExecution, EmbeddingExecution};
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_core::control::ControlHandle;
use kio_core::test_control::DebugTestControl;
use kio_pipeline::ledger::TaskKey;
use std::path::Path;
use std::sync::Arc;

#[test]
fn query_invalid_completion_never_settles_success_or_publishes_a_vector() {
    const NAME: &str = "embedding::transport::query_acceptance_tests::query_invalid_completion_never_settles_success_or_publishes_a_vector";
    // The existing isolated fixture explicitly initializes the device ledger
    // through LedgerCommand::Init and supplies private, empty device roots.
    if !crate::contextual_embedding_tests::isolated_child_for(NAME) {
        return;
    }
    let root = std::env::current_dir().unwrap();
    let trace = root.join("query-send-test-trace");
    let mut debug = DebugTestControl::from_env();
    debug.cli.query_embed_trace = Some(trace.clone());
    let control = ControlHandle::new(debug);
    let context = crate::context::test_support::context(&root);
    let settings = Arc::new(AdapterRuntimeSettings::default());
    let locks = kio_core::scope::LockSession::default();
    let invocation = crate::invocation::Invocation {
        context: &context,
        settings: &settings,
        control: &control,
        locks: &locks,
    };
    const QUERY: &str = "query acceptance private body 57f3b915";
    // This deliberately calls below normal CLI admission: NonMultimodal
    // declares canonical profile hash + modality=text, while its actual
    // adapter completes with modality=multimodal. Normal CLI materialization
    // rejects the declared modality earlier; this test isolates acceptance
    // after an actual mocked send and a durable device claim.
    let outcome = run_query_embedding_job(
        &invocation,
        EmbeddingExecution::Online(AdoptedEmbeddingExecution::NonMultimodal),
        QUERY,
    )
    .expect("the attempted query must produce a classified terminal result");
    assert_eq!(
        std::fs::read_to_string(&trace).unwrap(),
        format!("{QUERY}\n"),
        "the postattempt oracle requires exactly one mocked query send"
    );
    let path = crate::ledger_access::ledger_db_path(&control);
    let conn = immutable_sqlite(path);
    let attempts: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM batch_requests WHERE scope_id=?1 AND adapter_kind='embedding'",
            [TaskKey::DEVICE_SCOPE_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        attempts, 1,
        "the query must create one physical device intent"
    );
    let (settled, succeeded): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(outcome='succeeded'),0) FROM cost_ledger WHERE scope_id=?1 AND adapter_kind='embedding'",
        [TaskKey::DEVICE_SCOPE_ID], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(
        settled, 1,
        "an invalid attempted response still needs conservative settlement"
    );
    assert_eq!(
        succeeded, 0,
        "invalid completion must never be settled as successful"
    );
    assert!(
        matches!(outcome, Some(QueryEmbeddingOutcome::ContractViolation)),
        "invalid completion must not publish a query vector"
    );
    drop(conn);
    assert_no_query_body(&crate::settings::data_home(&control), QUERY.as_bytes());
    assert_no_query_body(
        &std::path::PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()),
        QUERY.as_bytes(),
    );
}

fn assert_no_query_body(path: &Path, query: &[u8]) {
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            assert_no_query_body(&entry.unwrap().path(), query);
        }
    } else if path.is_file() {
        let bytes = std::fs::read(path).unwrap();
        assert!(
            !bytes.windows(query.len()).any(|window| window == query),
            "query body persisted at {}",
            path.display()
        );
    }
}

/// Inspect a completed CLI invocation without creating SQLite coordination files.
/// Drop this snapshot before another invocation and reopen it for fresh assertions.
fn immutable_sqlite(path: impl AsRef<std::path::Path>) -> rusqlite::Connection {
    let path = path.as_ref().canonicalize().unwrap();
    let path = path.to_str().expect("fixture database path must be UTF-8");
    // Canonical Windows paths use verbatim prefixes; SQLite file URIs use
    // forward slashes, an empty authority and an absolute drive/UNC path.
    let path = if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!("//{}", unc.replace('\\', "/"))
    } else if let Some(verbatim) = path.strip_prefix(r"\\?\") {
        format!("/{}", verbatim.replace('\\', "/"))
    } else if path.as_bytes().get(1) == Some(&b':') {
        format!("/{}", path.replace('\\', "/"))
    } else {
        path.to_owned()
    };
    let mut uri = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(uri, "%{byte:02X}").unwrap();
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap()
}
