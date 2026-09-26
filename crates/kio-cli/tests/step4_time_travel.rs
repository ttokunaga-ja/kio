mod support;

use support::canonical_tempdir;

use std::collections::BTreeSet;
use std::fs;

use assert_cmd::Command;
use kio_core::cas::{ObjectKind, ObjectStore, hash_bytes};
use kio_core::dag::{CommitObject, CommitStats, CommitType};
use kio_core::gc::ShallowReceipt;
use kio_core::scope::Repository;
use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::TempDir;

// These are contract tests, not live-service tests. Keep every child hermetic even
// on a developer machine that happens to have adapter credentials or test seams.
const KIO_CHILD_ENV_DENYLIST: &[&str] = &[
    "GEMINI_API_KEY",
    "MISTRAL_API_KEY",
    "KIO_FIXED_NOW",
    "KIO_TEST_GEMINI_EMBED",
    "KIO_TEST_MISTRAL_OCR",
    "KIO_TEST_MISTRAL_BATCH",
    "KIO_TEST_MARKDOWNIZE_ADAPTER",
    "KIO_TEST_QUERY_EMBED_TRACE",
    "KIO_TEST_HOLD_LOCK_READY",
    "KIO_TEST_R13_2_AUTH",
    "KIO_TEST_R13_2_DECLARED",
    "KIO_TEST_R13_2_FALLBACK",
    "KIO_TEST_WINDOWS_PROFILE",
];

fn kio(dir: &TempDir, args: &[&str], fixed_now: Option<&str>) -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in KIO_CHILD_ENV_DENYLIST {
        command.env_remove(name);
    }
    command
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".test-config"))
        .env("XDG_DATA_HOME", dir.path().join(".test-data"))
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .args(args);
    if let Some(now) = fixed_now {
        command.env("KIO_FIXED_NOW", now);
    }
    command
}

fn json_success(dir: &TempDir, args: &[&str]) -> Value {
    json_success_at(dir, args, None)
}

fn json_success_at(dir: &TempDir, args: &[&str], fixed_now: Option<&str>) -> Value {
    let output = kio(dir, args, fixed_now)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn json_partial(dir: &TempDir, args: &[&str]) -> Value {
    let output = kio(dir, args, None)
        .arg("--json")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn json_success_embed(dir: &TempDir, args: &[&str]) -> Value {
    let output = kio(dir, args, None)
        .env("KIO_TEST_GEMINI_EMBED", "mock")
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn json_failure(dir: &TempDir, args: &[&str], code: i32) -> Value {
    json_failure_at(dir, args, code, None)
}

fn json_failure_at(dir: &TempDir, args: &[&str], code: i32, fixed_now: Option<&str>) -> Value {
    let output = kio(dir, args, fixed_now)
        .arg("--json")
        .assert()
        .code(code)
        .get_output()
        .stderr
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn init(dir: &TempDir) {
    json_success(dir, &["init"]);
}

fn index_at(dir: &TempDir, fixed_now: &str) -> Value {
    // `--offline` is deliberate: time travel must be reproducible from local
    // objects and must never make an adapter call in these fixtures.
    json_success_at(dir, &["index", "--offline", "--yes"], Some(fixed_now))
}

fn result_path(result: &Value) -> &str {
    result["evidence_pointer"]["path_at_commit"]
        .as_str()
        .expect("historical result path")
}

fn result_raw(result: &Value) -> &str {
    result["evidence_pointer"]["raw_hash"]
        .as_str()
        .expect("historical result raw hash")
}

fn result_commit(result: &Value) -> &str {
    result["evidence_pointer"]["commit"]
        .as_str()
        .expect("historical result commit")
}

fn results(value: &Value) -> &[Value] {
    value["results"].as_array().expect("search results array")
}

fn result_signatures(value: &Value) -> Vec<(String, String, String)> {
    results(value)
        .iter()
        .map(|result| {
            (
                result["chunk_hash"].as_str().unwrap().to_owned(),
                result_path(result).to_owned(),
                result_commit(result).to_owned(),
            )
        })
        .collect()
}

fn decode_base64url_no_pad(input: &str) -> Vec<u8> {
    fn sextet(byte: u8) -> u8 {
        match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => panic!("invalid base64url byte"),
        }
    }

    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut accumulator = 0_u32;
    let mut bits = 0_u8;
    for byte in input.bytes() {
        accumulator = (accumulator << 6) | u32::from(sextet(byte));
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((accumulator >> bits) & 0xff) as u8);
            accumulator &= if bits == 0 { 0 } else { (1_u32 << bits) - 1 };
        }
    }
    output
}

fn cursor_payload(token: &str) -> Value {
    let payload = token.rsplit_once('.').expect("signed cursor wire form").0;
    serde_json::from_slice(&decode_base64url_no_pad(payload)).unwrap()
}

fn search_all_history(dir: &TempDir, query: &str, limit: &str) -> Value {
    json_success(
        dir,
        &[
            "search",
            query,
            "--scope",
            ".",
            "--mode",
            "text",
            "--all-history",
            "--limit",
            limit,
        ],
    )
}

/// Follow a v2 cursor while omitting the selector after page 1. This intentionally
/// exercises the rule that replay inherits the signed canonical selector.
fn collect_all_history_by_cursor(dir: &TempDir, query: &str) -> Vec<(String, String, String)> {
    let mut page = json_success(
        dir,
        &[
            "search",
            query,
            "--scope",
            ".",
            "--mode",
            "text",
            "--all-history",
            "--limit",
            "1",
        ],
    );
    let mut collected = result_signatures(&page);
    for _ in 0..100 {
        let Some(cursor) = page["paging"]["next_cursor"].as_str().map(str::to_owned) else {
            return collected;
        };
        page = json_success(
            dir,
            &[
                "search", query, "--scope", ".", "--mode", "text", "--cursor", &cursor, "--limit",
                "1",
            ],
        );
        collected.extend(result_signatures(&page));
    }
    panic!("cursor did not terminate within the bounded fixture stream")
}

#[test]
fn ct4_timetravel_001_parser_exclusivity_duration_and_no_mutation() {
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(
        dir.path().join("parser.md"),
        "# Parser\n\nselectorfixture duration parser contract\n",
    )
    .unwrap();
    index_at(&dir, "2026-07-12T00:00:00Z");

    // Every positive integer duration unit executes. Redundant --all-history is
    // accepted with --since because it canonicalizes to one effective selector.
    for duration in ["1s", "1m", "1h", "1d", "1w"] {
        json_success_at(
            &dir,
            &[
                "search",
                "selectorfixture",
                "--scope",
                ".",
                "--mode",
                "text",
                "--since",
                duration,
            ],
            Some("2026-07-13T00:00:00Z"),
        );
    }
    for selector in [
        vec!["--at", "HEAD"],
        vec!["--all-history"],
        vec!["--include-deleted"],
    ] {
        let mut args = vec![
            "search",
            "selectorfixture",
            "--scope",
            ".",
            "--mode",
            "text",
        ];
        args.extend(selector);
        json_success(&dir, &args);
    }
    let since = json_success_at(
        &dir,
        &[
            "search",
            "selectorfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--since",
            "7d",
            "--limit",
            "100",
        ],
        Some("2026-07-13T00:00:00Z"),
    );
    let redundant = json_success_at(
        &dir,
        &[
            "search",
            "selectorfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--all-history",
            "--since",
            "7d",
            "--limit",
            "100",
        ],
        Some("2026-07-13T00:00:00Z"),
    );
    assert_eq!(since["results"], redundant["results"]);

    let db_path = dir.path().join(".kio/index/sqlite.db");
    let registry_path = dir.path().join(".test-data/kio/scope-registry.sqlite");
    let db_before = fs::read(&db_path).unwrap();
    let registry_before = fs::read(&registry_path).unwrap();
    let head_before = fs::read(dir.path().join(".kio/HEAD")).unwrap();
    let invalid: &[&[&str]] = &[
        &["--at", "HEAD", "--all-history"],
        &["--at", "HEAD", "--include-deleted"],
        &["--at", "HEAD", "--since", "1d"],
        &["--include-deleted", "--all-history"],
        &["--include-deleted", "--since", "1d"],
        &["--since", "0d"],
        &["--since", "7"],
        &["--since", "2026-07-01"],
        &["--since", "1.5d"],
        &["--since", "7x"],
        &["--since", "18446744073709551616s"],
        &["--since", "18446744073709551615w"],
    ];
    for suffix in invalid {
        let mut args = vec![
            "search",
            "selectorfixture",
            "--scope",
            ".",
            "--mode",
            "text",
        ];
        args.extend_from_slice(suffix);
        let error = json_failure_at(&dir, &args, 2, Some("2026-07-13T00:00:00Z"));
        assert_eq!(error["error_code"], "KIO-E-CONFIG-USAGE-001");
        assert_eq!(fs::read(&db_path).unwrap(), db_before, "args={args:?}");
        assert_eq!(
            fs::read(&registry_path).unwrap(),
            registry_before,
            "args={args:?}"
        );
        assert_eq!(
            fs::read(dir.path().join(".kio/HEAD")).unwrap(),
            head_before,
            "args={args:?}"
        );
    }
}

#[test]
fn ct4_timetravel_002_exact_at_hash_tag_head_and_normalize_none() {
    let dir = canonical_tempdir();
    init(&dir);
    let a = b"# Version\n\nversionfixture payload alpha\n";
    let b = b"# Version\n\nversionfixture payload beta\n";
    fs::write(dir.path().join("version.md"), a).unwrap();
    let c1 = index_at(&dir, "2026-07-10T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    json_success(&dir, &["tag", "old", &c1]);

    fs::write(dir.path().join("version.md"), b).unwrap();
    let c2 = index_at(&dir, "2026-07-11T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    // Remove the source-side historical projection while leaving canonical
    // commit/tree CAS intact. The replica already received C1 at index time,
    // so `--at` must still answer exactly without rewriting source sqlite.
    let conn = Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    conn.execute(
        "DELETE FROM tree_entries WHERE commit_hash = ?1",
        params![c1],
    )
    .unwrap();
    drop(conn);

    for operand in [&c1, "old"] {
        let at = json_success(
            &dir,
            &[
                "search",
                "versionfixture",
                "--scope",
                ".",
                "--mode",
                "text",
                "--at",
                operand,
            ],
        );
        assert_eq!(at["searched_scopes"][0]["snapshot_at"], c1);
        assert!(!results(&at).is_empty());
        assert!(
            results(&at).iter().all(|result| {
                result_raw(result) == hash_bytes(a) && result_commit(result) == c1
            })
        );
        let conn = Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
        let projected: i64 = conn
            .query_row(
                "SELECT count(*) FROM tree_entries WHERE commit_hash = ?1",
                params![c1],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            projected, 0,
            "--at must answer from the replica without lazily rewriting source sqlite"
        );
    }
    let head = json_success(
        &dir,
        &[
            "search",
            "versionfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--at",
            "HEAD",
        ],
    );
    assert_eq!(head["searched_scopes"][0]["snapshot_at"], c2);
    assert!(
        results(&head)
            .iter()
            .all(|result| result_raw(result) == hash_bytes(b))
    );

    // A manual snapshot captures the raw file but deliberately has no normalize
    // binding. A later index may create normalized/chunk/cache rows for the exact
    // same raw bytes; --at must still return no chunk for the raw-only tree entry.
    fs::write(
        dir.path().join("latent.md"),
        "# Latent\n\nlatentnormalizefixture must not leak backward\n",
    )
    .unwrap();
    let raw_only = json_success_at(
        &dir,
        &["snapshot", "create", "-m", "raw only"],
        Some("2026-07-11T01:00:00Z"),
    )["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    index_at(&dir, "2026-07-11T02:00:00Z");
    let absent = json_success(
        &dir,
        &[
            "search",
            "latentnormalizefixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--at",
            &raw_only,
        ],
    );
    assert!(results(&absent).is_empty(), "{absent}");
}

/// A snapshot retained by the source cache can become disconnected from the
/// current HEAD when a writer moves onto another branch. Its `--at` projection
/// must survive the next writer refresh; direct search may not reopen source
/// SQLite to reconstruct it.
#[test]
fn ct4_timetravel_013_disconnected_at_uses_writer_published_replica() {
    let dir = canonical_tempdir();
    init(&dir);

    fs::write(
        dir.path().join("branch.md"),
        b"# Branch\n\nbranchfixture root payload\n",
    )
    .unwrap();
    index_at(&dir, "2026-07-12T00:00:00Z");

    let retained = b"# Branch\n\nbranchfixture disconnected target\n";
    fs::write(dir.path().join("branch.md"), retained).unwrap();
    let disconnected = index_at(&dir, "2026-07-12T01:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    // C2's initial writer has already published its exact `--at` projection.
    // Remove the only source-cache discovery row before the next normal
    // writer. That writer can neither rediscover nor regenerate C2; it must
    // retain the existing replica marker and bindings incrementally.
    let conn = Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    conn.execute(
        "DELETE FROM tree_entries WHERE commit_hash = ?1",
        params![disconnected],
    )
    .unwrap();
    drop(conn);
    let scope_id =
        serde_json::from_slice::<Value>(&fs::read(dir.path().join(".kio/scope.json")).unwrap())
            .unwrap()["scope_id"]
            .as_str()
            .unwrap()
            .to_owned();
    let replica_path = dir.path().join(".test-cache/kio/aggregator.sqlite");
    let replica = Connection::open(&replica_path).unwrap();
    let before_marker: (i64, i64) = replica
        .query_row(
            "SELECT rowid, completed_at FROM agg_projection_markers \
             WHERE scope_id = ?1 AND selector_kind = 'at' AND snapshot_commit = ?2",
            params![scope_id, disconnected],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let before_binding_rowids = replica
        .prepare(
            "SELECT rowid FROM agg_bindings \
             WHERE scope_id = ?1 AND selector_kind = 'at' AND snapshot_commit = ?2 \
             ORDER BY rowid",
        )
        .unwrap()
        .query_map(params![scope_id, disconnected], |row| row.get::<_, i64>(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert!(!before_binding_rowids.is_empty());
    drop(replica);

    // Publish C3 as C2's child. The already-completed `--at C2` replica
    // projection must remain stable while the linear HEAD advances.
    fs::write(
        dir.path().join("branch.md"),
        b"# Branch\n\nbranchfixture current sibling\n",
    )
    .unwrap();
    let sibling = index_at(&dir, "2026-07-12T02:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(
        sibling, disconnected,
        "the test must advance the linear chain"
    );

    let replica = Connection::open(&replica_path).unwrap();
    let after_marker: (i64, i64) = replica
        .query_row(
            "SELECT rowid, completed_at FROM agg_projection_markers \
             WHERE scope_id = ?1 AND selector_kind = 'at' AND snapshot_commit = ?2",
            params![scope_id, disconnected],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let after_binding_rowids = replica
        .prepare(
            "SELECT rowid FROM agg_bindings \
             WHERE scope_id = ?1 AND selector_kind = 'at' AND snapshot_commit = ?2 \
             ORDER BY rowid",
        )
        .unwrap()
        .query_map(params![scope_id, disconnected], |row| row.get::<_, i64>(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        after_marker, before_marker,
        "normal refresh must not rewrite C2's completed --at marker"
    );
    assert_eq!(
        after_binding_rowids, before_binding_rowids,
        "normal refresh must preserve C2's existing --at bindings in place"
    );
    drop(replica);

    // The completed writer projection is now the only index search may read.
    let source = dir.path().join(".kio/index/sqlite.db");
    let hidden = dir.path().join(".kio/index/sqlite.db.hidden-for-at");
    fs::rename(&source, &hidden).unwrap();

    let response = json_success(
        &dir,
        &[
            "search",
            "disconnected target",
            "--scope",
            ".",
            "--mode",
            "text",
            "--at",
            &disconnected,
        ],
    );
    assert_eq!(response["searched_scopes"][0]["snapshot_at"], disconnected);
    assert!(!results(&response).is_empty(), "{response}");
    assert!(results(&response).iter().all(|result| {
        result_raw(result) == hash_bytes(retained) && result_commit(result) == disconnected
    }));
    assert!(
        !source.exists(),
        "--at must not recreate or read the hidden source SQLite index"
    );
}

/// An empty tree has no `tree_entries` row by which a later writer can discover
/// an otherwise valid disconnected snapshot. A targeted historical reindex is
/// the writer that knows this exact `--at` target, so it must publish the
/// completed empty marker before a replica-only reader needs it.
#[test]
fn ct4_timetravel_014_historical_reindex_publishes_empty_disconnected_at_snapshot() {
    let dir = canonical_tempdir();
    init(&dir);

    fs::write(
        dir.path().join("root.md"),
        b"# Root\n\nemptyatfixture root payload\n",
    )
    .unwrap();
    index_at(&dir, "2026-07-12T00:00:00Z");

    fs::remove_file(dir.path().join("root.md")).unwrap();
    let c2 = index_at(&dir, "2026-07-12T01:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let repo = Repository::open(dir.path()).unwrap();
    let c2_tree = repo
        .read_tree(&repo.read_commit(&c2).unwrap().tree)
        .unwrap();
    assert!(
        c2_tree.entries.is_empty(),
        "C2 must be the empty target tree"
    );

    // Advance the single chain after the empty C2 snapshot. It remains
    // explicitly addressable even when the current source cache lacks rows.
    fs::write(
        dir.path().join("sibling.md"),
        b"# Sibling\n\nemptyatfixture current sibling\n",
    )
    .unwrap();
    let c3 = index_at(&dir, "2026-07-12T02:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(c3, c2, "C3 must leave C2 disconnected from current HEAD");

    let reindexed = json_success(&dir, &["reindex", "--at", &c2]);
    assert_eq!(reindexed["status"], "reindexed");
    assert_eq!(reindexed["snapshot_at"], c2);

    // A valid empty snapshot deliberately has no tree-entry sentinel. The
    // replica completion marker is the only durable way to distinguish it
    // from a projection miss once source SQLite is unavailable to search.
    let source = dir.path().join(".kio/index/sqlite.db");
    let conn = Connection::open(&source).unwrap();
    let cached_rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM tree_entries WHERE commit_hash = ?1",
            params![c2],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cached_rows, 0);
    drop(conn);

    let hidden = dir.path().join(".kio/index/sqlite.db.hidden-for-empty-at");
    fs::rename(&source, &hidden).unwrap();
    let response = json_success(
        &dir,
        &[
            "search",
            "emptyatfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--at",
            &c2,
        ],
    );
    assert_eq!(response["searched_scopes"][0]["snapshot_at"], c2);
    assert!(
        results(&response).is_empty(),
        "the explicitly selected empty tree must not leak C1/C3 chunks: {response}"
    );
    assert!(
        !source.exists(),
        "--at must answer the writer-published empty marker without reopening source SQLite"
    );
}

/// A historical reindex can select a disconnected commit whose normalized
/// binding and chunk/config association already exist in the ledger.  That
/// must still durably record the selected commit as a *publication* (rather
/// than relying on a new creation row), so a later SQLite rebuild can answer
/// the exact historical selector without source-index history.
#[test]
fn ct4_timetravel_015_historical_reindex_durably_publishes_existing_disconnected_chunk() {
    let dir = canonical_tempdir();
    init(&dir);

    let content = b"# Durable publication\n\ndurablepublicationfixture existing chunk binding\n";
    fs::write(dir.path().join("publication.md"), content).unwrap();
    let a = index_at(&dir, "2026-07-12T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    let repo = Repository::open(dir.path()).unwrap();
    let a_object = repo.read_commit(&a).unwrap();
    let store = ObjectStore::new(repo.kio_dir());
    // B deliberately reuses A's immutable tree as a direct child. It is not
    // published as HEAD, but remains on the one valid ancestry line and can
    // therefore be authenticated by explicit historical reindexing.
    let b = write_commit(
        &store,
        &synthetic_commit(
            a_object.tree,
            Some(a.clone()),
            "2026-07-12T01:00:00Z",
            "disconnected duplicate binding",
            a_object.tool_lock_hash,
            CommitType::Manual,
        ),
    );
    assert_eq!(
        repo.head_commit_hash().unwrap().as_deref(),
        Some(a.as_str())
    );

    let chunks_path = dir.path().join(".kio/index/chunks.jsonl");
    let creation = fs::read_to_string(&chunks_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|row| row.get("event").is_none())
        .expect("A must create a durable chunk association");
    let chunk_id = creation["chunk_id"].as_str().unwrap();
    let config_hash = creation["chunking_config_hash"].as_str().unwrap();

    let reindexed = json_success(&dir, &["reindex", "--at", &b]);
    assert_eq!(reindexed["status"], "reindexed");
    assert_eq!(reindexed["snapshot_at"], b);

    let has_a_publication = fs::read_to_string(&chunks_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .any(|row| {
            row["event"] == "publication"
                && row["chunk_id"] == chunk_id
                && row["chunking_config_hash"] == config_hash
                && row["introduction_commit"] == a
        });
    assert!(
        has_a_publication,
        "historical reindex must retain A's durable publication event"
    );

    // B is an unpublished linear descendant. Fsck must nevertheless
    // authenticate the original durable publication and include the tree closure;
    // treating the JSONL event as either ignorable or self-authenticating would
    // respectively lose history or let a forged row choose arbitrary roots.
    let verified = json_success(&dir, &["repair", "verify-objects"]);
    assert_eq!(verified["status"], "ok", "{verified}");
    assert!(
        verified["remaining_findings"]
            .as_array()
            .expect("verify report has findings array")
            .is_empty(),
        "a disconnected reindex publication must be a clean verified root: {verified}"
    );

    let response = json_success(
        &dir,
        &[
            "search",
            "durablepublicationfixture",
            "--at",
            &b,
            "--scope",
            ".",
            "--mode",
            "text",
        ],
    );
    assert_eq!(response["searched_scopes"][0]["snapshot_at"], b);
    assert!(
        results(&response).iter().any(|result| {
            result_raw(result) == hash_bytes(content) && result_commit(result) == b
        }),
        "the linear projection must retain B's selected snapshot: {response}"
    );
}

#[test]
fn ct4_timetravel_016_historical_reindex_keeps_ancestor_introduction_for_unchanged_descendant() {
    let dir = canonical_tempdir();
    init(&dir);

    let content = b"# Ancestor authority\n\nancestorpublicationfixture unchanged binding\n";
    fs::write(dir.path().join("ancestor.md"), content).unwrap();
    let c1 = index_at(&dir, "2026-07-12T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    let repo = Repository::open(dir.path()).unwrap();
    let c1_object = repo.read_commit(&c1).unwrap();
    let c2 = write_commit(
        &ObjectStore::new(repo.kio_dir()),
        &synthetic_commit(
            c1_object.tree,
            Some(c1.clone()),
            "2026-07-12T01:00:00Z",
            "unchanged descendant",
            c1_object.tool_lock_hash,
            CommitType::Manual,
        ),
    );

    let chunks_path = dir.path().join(".kio/index/chunks.jsonl");
    let creation = fs::read_to_string(&chunks_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|row| row.get("event").is_none())
        .expect("C1 must create the durable association");
    let chunk_id = creation["chunk_id"].as_str().unwrap();
    let config_hash = creation["chunking_config_hash"].as_str().unwrap();

    json_success(&dir, &["reindex", "--at", &c2]);
    json_success(&dir, &["repair", "verify-objects"]);

    let publications = fs::read_to_string(&chunks_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| {
            row["event"] == "publication"
                && row["chunk_id"] == chunk_id
                && row["chunking_config_hash"] == config_hash
        })
        .map(|row| row["introduction_commit"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        publications,
        BTreeSet::from([c1.clone()]),
        "reindexing C2 must not mint C2 as an introduction for C1's unchanged binding"
    );

    fs::remove_file(dir.path().join(".kio/index/sqlite.db")).unwrap();
    json_success(&dir, &["repair", "rebuild-db"]);
    let conn = Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let rebuilt_introductions = conn
        .prepare(
            "SELECT introduction_commit FROM chunk_publications \
             WHERE chunk_id = ?1 AND chunking_config_hash = ?2 \
             ORDER BY introduction_commit",
        )
        .unwrap()
        .query_map(params![chunk_id, config_hash], |row| {
            row.get::<_, String>(0)
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rebuilt_introductions, vec![c1]);
}

#[test]
fn ct4_timetravel_017_historical_reindex_introduces_config_transition_at_descendant() {
    let dir = canonical_tempdir();
    init(&dir);

    fs::write(
        dir.path().join("config-transition.md"),
        "# Config transition\n\nconfigtransitionfixture unchanged normalized identity\n",
    )
    .unwrap();
    let c1 = index_at(&dir, "2026-07-12T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    // Keep the normalized identity untouched but bind C2 to a new chunking
    // configuration.  That composite (identity, config B) relation begins at
    // C2; borrowing C1/config A would make the new association unreachable.
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[chunking]\nstrategy = \"heading\"\nmax_chars = 48\n",
    )
    .unwrap();
    let config_b = kio_index::chunking::chunking_config_hash("heading", 48).unwrap();
    let repo = Repository::open(dir.path()).unwrap();
    let c1_object = repo.read_commit(&c1).unwrap();
    let store = ObjectStore::new(repo.kio_dir());
    let mut c2_tree = repo.read_tree(&c1_object.tree).unwrap();
    c2_tree.chunking_config_hash = config_b.clone();
    let c2_tree_hash = store
        .write_json(ObjectKind::Tree, &serde_json::to_value(&c2_tree).unwrap())
        .unwrap()
        .0;
    let c2 = write_commit(
        &store,
        &synthetic_commit(
            c2_tree_hash,
            Some(c1),
            "2026-07-12T01:00:00Z",
            "same normalized identity under config B",
            c1_object.tool_lock_hash,
            CommitType::Manual,
        ),
    );

    json_success(&dir, &["reindex", "--at", &c2]);
    let publications = fs::read_to_string(dir.path().join(".kio/index/chunks.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["event"] == "publication" && row["chunking_config_hash"] == config_b)
        .map(|row| row["introduction_commit"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(publications, BTreeSet::from([c2]));
}

#[test]
fn ct4_timetravel_018_historical_reindex_keeps_one_linear_introduction_through_copy_and_delete() {
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(dir.path().join("base.md"), "# Base\n\nbasefixture\n").unwrap();
    index_at(&dir, "2026-07-12T00:00:00Z");
    fs::write(
        dir.path().join("merge-intro.md"),
        "# Merge introduction\n\nmergeintroductionfixture\n",
    )
    .unwrap();
    let c1 = index_at(&dir, "2026-07-12T00:01:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    fs::copy(
        dir.path().join("merge-intro.md"),
        dir.path().join("merge-copy.md"),
    )
    .unwrap();
    index_at(&dir, "2026-07-12T00:02:00Z");
    fs::remove_file(dir.path().join("merge-intro.md")).unwrap();
    let c3 = index_at(&dir, "2026-07-12T00:03:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    json_success(&dir, &["reindex", "--at", &c3]);
    let publications = fs::read_to_string(dir.path().join(".kio/index/chunks.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["event"] == "publication")
        .map(|row| row["introduction_commit"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert!(publications.contains(&c1));
    assert!(
        !publications.contains(&c3),
        "copy/delete descendants re-affirm the original linear introduction"
    );
}

#[test]
fn ct4_timetravel_003_edit_rename_aliases_twins_and_cursor_paging() {
    let dir = canonical_tempdir();
    init(&dir);
    let edit_old = b"# Edit\n\nhistoryfixture edited value oldneedle\n";
    let edit_new = b"# Edit\n\nhistoryfixture edited value newneedle\n";
    let renamed = b"# Rename\n\nhistoryfixture unchanged rename payload\n";
    fs::write(dir.path().join("edit.md"), edit_old).unwrap();
    fs::write(dir.path().join("old.md"), renamed).unwrap();
    let c1 = index_at(&dir, "2026-07-09T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    fs::write(dir.path().join("edit.md"), edit_new).unwrap();
    fs::rename(dir.path().join("old.md"), dir.path().join("new.md")).unwrap();
    let c2 = index_at(&dir, "2026-07-10T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    let history = search_all_history(&dir, "historyfixture", "100");
    let raw_set = results(&history)
        .iter()
        .map(result_raw)
        .collect::<BTreeSet<_>>();
    assert!(raw_set.contains(hash_bytes(edit_old).as_str()));
    assert!(raw_set.contains(hash_bytes(edit_new).as_str()));

    let renamed_hash = hash_bytes(renamed);
    let rename_hits = results(&history)
        .iter()
        .filter(|result| result_raw(result) == renamed_hash)
        .collect::<Vec<_>>();
    assert_eq!(
        rename_hits
            .iter()
            .map(|result| result_path(result))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["new.md", "old.md"])
    );
    assert_eq!(
        rename_hits
            .iter()
            .map(|result| result_commit(result))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([c1.as_str(), c2.as_str()])
    );
    for result in &rename_hits {
        assert_eq!(result["current_paths"], serde_json::json!(["new.md"]));
        assert_eq!(result["current_path"], "new.md");
    }

    // Identical bytes under two live names are twins, not inferred rename lineage.
    fs::write(dir.path().join("copy.md"), renamed).unwrap();
    index_at(&dir, "2026-07-11T00:00:00Z");
    let twins = search_all_history(&dir, "historyfixture", "100");
    let twin_hits = results(&twins)
        .iter()
        .filter(|result| result_raw(result) == renamed_hash)
        .collect::<Vec<_>>();
    assert_eq!(
        twin_hits
            .iter()
            .map(|result| result_path(result))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["copy.md", "new.md", "old.md"])
    );
    for result in &twin_hits {
        assert_eq!(
            result["current_paths"],
            serde_json::json!(["copy.md", "new.md"])
        );
        assert!(
            result.get("current_path").is_none(),
            "singular current_path is ambiguous for twins: {result}"
        );
    }

    // Pagination happens after alias expansion. A boundary inside the three aliases
    // must resume each alias exactly once and in the same order as an unpaged search.
    assert_eq!(
        collect_all_history_by_cursor(&dir, "historyfixture"),
        result_signatures(&twins)
    );
}

#[test]
fn ct4_timetravel_004_include_deleted_uses_final_version_and_frozen_tree() {
    let dir = canonical_tempdir();
    init(&dir);
    let a = b"# Gone\n\ntimelinefixture deleted alpha old version\n";
    let b = b"# Gone\n\ntimelinefixture deleted beta final version\n";
    let live = b"# Live\n\ntimelinefixture still live\n";
    fs::write(dir.path().join("gone.md"), a).unwrap();
    index_at(&dir, "2026-07-09T00:00:00Z");
    fs::write(dir.path().join("gone.md"), b).unwrap();
    let c2 = index_at(&dir, "2026-07-10T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::remove_file(dir.path().join("gone.md")).unwrap();
    fs::write(dir.path().join("live.md"), live).unwrap();
    index_at(&dir, "2026-07-11T00:00:00Z");

    let included = json_success(
        &dir,
        &[
            "search",
            "timelinefixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--include-deleted",
            "--limit",
            "100",
        ],
    );
    let raws = results(&included)
        .iter()
        .map(result_raw)
        .collect::<BTreeSet<_>>();
    assert!(raws.contains(hash_bytes(b).as_str()));
    assert!(raws.contains(hash_bytes(live).as_str()));
    assert!(!raws.contains(hash_bytes(a).as_str()));
    let deleted = results(&included)
        .iter()
        .find(|result| result_raw(result) == hash_bytes(b))
        .unwrap();
    assert_eq!(result_path(deleted), "gone.md");
    assert_eq!(result_commit(deleted), c2);
    let repo = Repository::open(dir.path()).unwrap();
    let tree = repo
        .read_tree(&repo.read_commit(&c2).unwrap().tree)
        .unwrap();
    assert!(
        tree.entries
            .iter()
            .any(|entry| entry.path == "gone.md" && entry.raw_hash == hash_bytes(b))
    );

    let page1 = json_success(
        &dir,
        &[
            "search",
            "timelinefixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--include-deleted",
            "--limit",
            "1",
        ],
    );
    let cursor = page1["paging"]["next_cursor"]
        .as_str()
        .expect("two-result fixture must page")
        .to_owned();
    let page2_before = json_success(
        &dir,
        &[
            "search",
            "timelinefixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--cursor",
            &cursor,
            "--limit",
            "1",
        ],
    );

    // Advance HEAD. The same semantic chunk is now live under survivor.md, so live
    // wins and the stale deleted alias at gone.md is not emitted for this raw.
    fs::write(dir.path().join("survivor.md"), b).unwrap();
    index_at(&dir, "2026-07-12T00:00:00Z");
    let fresh = json_success(
        &dir,
        &[
            "search",
            "timelinefixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--include-deleted",
            "--limit",
            "100",
        ],
    );
    let b_paths = results(&fresh)
        .iter()
        .filter(|result| result_raw(result) == hash_bytes(b))
        .map(result_path)
        .collect::<Vec<_>>();
    assert_eq!(b_paths, ["survivor.md"]);

    // PC19/PC21 (05 §1.5 L180-191): the re-index above changed `chunk_fts`
    // content (one of PC20's 6 `index_generation` rotation triggers), so the
    // page-1 cursor from before it is now rejected — "再検索が正" — instead
    // of silently replaying against a stale snapshot whose BM25 ranking basis
    // (corpus-wide document-frequency/average-length statistics) has since
    // shifted. This supersedes the pre-PC19 "cursor replay uses the signed
    // snapshot/tree, never current HEAD/manifest.json" contract this test
    // used to exercise past this exact same re-index — that invariant still
    // holds for anything short of an index_generation change, but a `chunk_fts`
    // content change is no longer one of the cases a frozen cursor survives.
    let err = json_failure(
        &dir,
        &[
            "search",
            "timelinefixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--cursor",
            &cursor,
            "--limit",
            "1",
        ],
        2,
    );
    assert_eq!(err["error_code"], "KIO-E-SEARCH-CURSOR-001");
    assert_eq!(err["context"]["reason"], "index_generation_mismatch");
    // `page2_before` (replayed prior to the re-index) is untouched by this —
    // still a normal, valid page.
    assert!(!page2_before["results"].as_array().unwrap().is_empty());
}

#[test]
fn ct4_timetravel_006_cursor_binds_and_inherits_selector() {
    let dir = canonical_tempdir();
    init(&dir);
    for name in ["a.md", "b.md", "c.md"] {
        fs::write(
            dir.path().join(name),
            format!("# Cursor {name}\n\ncursorfixture historical paging {name}\n"),
        )
        .unwrap();
    }
    let c1 = index_at(&dir, "2026-07-09T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(
        dir.path().join("a.md"),
        "# Cursor A2\n\ncursorfixture historical paging changed\n",
    )
    .unwrap();
    let c2 = index_at(&dir, "2026-07-10T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    let page1 = json_success(
        &dir,
        &[
            "search",
            "cursorfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--all-history",
            "--limit",
            "1",
        ],
    );
    let cursor = page1["paging"]["next_cursor"].as_str().unwrap().to_owned();
    let payload = cursor_payload(&cursor);
    assert_eq!(payload["v"], 3);
    assert_eq!(
        payload["time_travel"],
        serde_json::json!({"all_history": true})
    );
    assert!(
        payload["scopes"][0]["max_association_rowid"]
            .as_u64()
            .is_some()
    );
    assert!(
        payload["scopes"][0]["chunking_config_hash"]
            .as_str()
            .is_some_and(|hash| hash.starts_with("sha256:"))
    );
    let inherited = json_success(
        &dir,
        &[
            "search",
            "cursorfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--cursor",
            &cursor,
            "--limit",
            "1",
        ],
    );
    let repeated = json_success(
        &dir,
        &[
            "search",
            "cursorfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--all-history",
            "--cursor",
            &cursor,
            "--limit",
            "1",
        ],
    );
    assert_eq!(inherited["results"], repeated["results"]);
    let mismatch = json_failure(
        &dir,
        &[
            "search",
            "cursorfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--include-deleted",
            "--cursor",
            &cursor,
        ],
        2,
    );
    assert_eq!(mismatch["error_code"], "KIO-E-SEARCH-CURSOR-001");

    let at_page1 = json_success(
        &dir,
        &[
            "search",
            "cursorfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--at",
            &c1,
            "--limit",
            "1",
        ],
    );
    let at_cursor = at_page1["paging"]["next_cursor"].as_str().unwrap();
    let at_mismatch = json_failure(
        &dir,
        &[
            "search",
            "cursorfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--at",
            &c2,
            "--cursor",
            at_cursor,
        ],
        2,
    );
    assert_eq!(at_mismatch["error_code"], "KIO-E-SEARCH-CURSOR-001");

    // A caller cannot modify any signed maximum/cutoff field. Corrupt one payload
    // byte while keeping the signature, which must fail before the token is trusted.
    let mut tampered = cursor.clone().into_bytes();
    let payload_index = tampered
        .iter()
        .position(|byte| *byte != b'.')
        .expect("non-empty cursor payload");
    tampered[payload_index] = if tampered[payload_index] == b'A' {
        b'B'
    } else {
        b'A'
    };
    let tampered = String::from_utf8(tampered).unwrap();
    let error = json_failure(
        &dir,
        &[
            "search",
            "cursorfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--cursor",
            &tampered,
        ],
        2,
    );
    assert_eq!(error["error_code"], "KIO-E-SEARCH-CURSOR-001");
}

#[test]
fn ct4_timetravel_005_since_includes_cutoff_and_freezes_it_in_cursor() {
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(
        dir.path().join("before.md"),
        "# Before\n\nboundaryfixture one second before cutoff\n",
    )
    .unwrap();
    index_at(&dir, "2026-07-05T23:59:59Z");

    // Make the equality result deliberately lower-ranked than the two short,
    // term-dense after results, so it crosses the first page boundary.
    let equality = format!(
        "# Equal\n\nboundaryfixture exactly cutoff {}\n",
        "lengthpadding ".repeat(250)
    );
    fs::write(dir.path().join("equal.md"), equality).unwrap();
    index_at(&dir, "2026-07-06T00:00:00Z");

    fs::write(
        dir.path().join("after-a.md"),
        "# After A\n\nboundaryfixture boundaryfixture after a\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("after-b.md"),
        "# After B\n\nboundaryfixture boundaryfixture after b\n",
    )
    .unwrap();
    index_at(&dir, "2026-07-12T00:00:00Z");

    let full = json_success_at(
        &dir,
        &[
            "search",
            "boundaryfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--since",
            "7d",
            "--limit",
            "100",
        ],
        Some("2026-07-13T00:00:00Z"),
    );
    let paths = results(&full)
        .iter()
        .map(result_path)
        .collect::<BTreeSet<_>>();
    assert!(paths.contains("equal.md"), "cutoff equality is inclusive");
    assert!(paths.contains("after-a.md"));
    assert!(paths.contains("after-b.md"));
    assert!(
        !paths.contains("before.md"),
        "one second before is excluded"
    );

    let page1 = json_success_at(
        &dir,
        &[
            "search",
            "boundaryfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--since",
            "7d",
            "--limit",
            "2",
        ],
        Some("2026-07-13T00:00:00Z"),
    );
    assert!(
        results(&page1)
            .iter()
            .all(|result| result_path(result) != "equal.md"),
        "fixture requires equality at the page boundary: {page1}"
    );
    let cursor = page1["paging"]["next_cursor"]
        .as_str()
        .expect("equality result remains for page 2")
        .to_owned();
    let payload = cursor_payload(&cursor);
    assert_eq!(payload["v"], 3);
    assert_eq!(
        payload["time_travel"],
        serde_json::json!({"all_history": true, "since": "604800s"})
    );
    assert_eq!(payload["since_cutoff"], "2026-07-06T00:00:00Z");
    let page2_same_now = json_success_at(
        &dir,
        &[
            "search",
            "boundaryfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--cursor",
            &cursor,
            "--limit",
            "10",
        ],
        Some("2026-07-13T00:00:00Z"),
    );
    assert!(
        results(&page2_same_now)
            .iter()
            .any(|result| result_path(result) == "equal.md")
    );

    // At this invocation time a recomputed 7d cutoff would be July 7 and would
    // drop equal.md. Selector-less replay must instead retain the signed July 6
    // page-1 cutoff. Explicit 1w is the same canonical selector and is accepted.
    let page2_next_day = json_success_at(
        &dir,
        &[
            "search",
            "boundaryfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--cursor",
            &cursor,
            "--limit",
            "10",
        ],
        Some("2026-07-14T00:00:00Z"),
    );
    let page2_one_week = json_success_at(
        &dir,
        &[
            "search",
            "boundaryfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--since",
            "1w",
            "--cursor",
            &cursor,
            "--limit",
            "10",
        ],
        Some("2026-07-14T00:00:00Z"),
    );
    assert_eq!(page2_same_now["results"], page2_next_day["results"]);
    assert_eq!(page2_same_now["results"], page2_one_week["results"]);

    let mismatch = json_failure_at(
        &dir,
        &[
            "search",
            "boundaryfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--since",
            "8d",
            "--cursor",
            &cursor,
        ],
        2,
        Some("2026-07-13T00:00:00Z"),
    );
    assert_eq!(mismatch["error_code"], "KIO-E-SEARCH-CURSOR-001");
}

#[test]
fn ct4_timetravel_006_cursor_rejects_write_through_visible_append() {
    let dir = canonical_tempdir();
    init(&dir);
    for name in ["rank-a.md", "rank-b.md"] {
        fs::write(
            dir.path().join(name),
            format!("# {name}\n\nassociationfixture established {name}\n"),
        )
        .unwrap();
    }
    index_at(&dir, "2026-07-10T00:00:00Z");

    let page1 = json_success(
        &dir,
        &[
            "search",
            "associationfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--all-history",
            "--limit",
            "1",
        ],
    );
    let cursor = page1["paging"]["next_cursor"]
        .as_str()
        .expect("two indexed candidates must page")
        .to_owned();

    // A normal writer publishes the new chunk plus its binding to the replica.
    // Direct search must see that published state, while a cursor signed before
    // it must be rejected rather than reading an unannounced source-db edit.
    fs::write(
        dir.path().join("late.md"),
        "# Late\n\nassociationfixture write-through-visible candidate\n",
    )
    .unwrap();
    index_at(&dir, "2026-07-11T00:00:00Z");

    let fresh = search_all_history(&dir, "associationfixture", "100");
    assert!(results(&fresh).iter().any(|result| {
        result["snippet"]
            .as_str()
            .unwrap()
            .contains("write-through-visible")
    }));

    let replay = json_failure(
        &dir,
        &[
            "search",
            "associationfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--cursor",
            &cursor,
            "--limit",
            "100",
        ],
        2,
    );
    assert_eq!(replay["error_code"], "KIO-E-SEARCH-CURSOR-001");
    assert_eq!(replay["context"]["reason"], "index_generation_mismatch");
}

#[test]
fn ct4_timetravel_011_historical_reindex_rejects_config_drift_without_mutation() {
    let dir = canonical_tempdir();
    init(&dir);
    kio(&dir, &["ledger", "init"], None).assert().success();
    json_success(&dir, &["adapter", "approve", "--all", "--yes"]);
    fs::write(
        dir.path().join("history.md"),
        "# Historical selected\n\nhistoricalselectedmarker belongs only to C1. This paragraph is deliberately long enough to produce a stable historical search result.\n",
    )
    .unwrap();
    let c1 = json_success_embed(&dir, &["index", "--yes"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    fs::write(
        dir.path().join("history.md"),
        "# Current nonselected\n\ncurrentnonselectedmarker belongs only to C2. This paragraph is also deliberately long and must retain its own tree-bound configuration.\n",
    )
    .unwrap();
    let c2 = json_success_embed(&dir, &["index", "--yes"])["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(c1, c2);

    let repo = Repository::open(dir.path()).unwrap();
    let c1_tree = repo
        .read_tree(&repo.read_commit(&c1).unwrap().tree)
        .unwrap();
    let c2_tree = repo
        .read_tree(&repo.read_commit(&c2).unwrap().tree)
        .unwrap();
    let c1_entry = c1_tree
        .entries
        .iter()
        .find(|entry| entry.path == "history.md")
        .unwrap();
    let c2_entry = c2_tree
        .entries
        .iter()
        .find(|entry| entry.path == "history.md")
        .unwrap();
    let c1_raw = c1_entry.raw_hash.clone();
    let c2_raw = c2_entry.raw_hash.clone();
    let head_before = fs::read(dir.path().join(".kio/HEAD")).unwrap();

    // Adapter approval is durable scope metadata, separate from the chunking
    // configuration rewritten below.
    fs::write(
        dir.path().join(".kio/config.toml"),
        "[chunking]\nstrategy = \"heading\"\nmax_chars = 48\n",
    )
    .unwrap();
    let current_config = kio_index::chunking::chunking_config_hash("heading", 48).unwrap();
    let count_associations = |raw_hash: &str| -> i64 {
        let conn = Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
        conn.query_row(
            "SELECT COUNT(*)
               FROM chunks c
               JOIN chunk_config_generations cg ON cg.chunk_id = c.chunk_id
              WHERE c.raw_hash = ?1 AND cg.chunking_config_hash = ?2",
            params![raw_hash, current_config],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(count_associations(&c1_raw), 0);
    assert_eq!(count_associations(&c2_raw), 0);

    // A historical tree binds the exact config that created its associations.
    // The hash alone is not a reversible descriptor for a different live
    // config, and publishing freshly chunked rows under C1 would forge C1's
    // tree/config provenance. Reject before touching either durable ledger or
    // its SQLite projection.
    let chunks_path = dir.path().join(".kio/index/chunks.jsonl");
    let sqlite_path = dir.path().join(".kio/index/sqlite.db");
    let chunks_before = fs::read(&chunks_path).unwrap();
    let sqlite_before = fs::read(&sqlite_path).unwrap();
    let output = json_failure(&dir, &["reindex", "--at", &c1], 2);
    assert_eq!(output["error_code"], "KIO-E-CONFIG-USAGE-001");
    assert_eq!(fs::read(&chunks_path).unwrap(), chunks_before);
    assert_eq!(fs::read(&sqlite_path).unwrap(), sqlite_before);

    assert_eq!(fs::read(dir.path().join(".kio/HEAD")).unwrap(), head_before);
    assert_eq!(count_associations(&c1_raw), 0);
    assert_eq!(count_associations(&c2_raw), 0);

    let selected_search = json_success(
        &dir,
        &[
            "search",
            "historicalselectedmarker",
            "--scope",
            ".",
            "--mode",
            "text",
            "--at",
            &c1,
        ],
    );
    assert!(!results(&selected_search).is_empty());
    let nonselected_search = json_success(
        &dir,
        &[
            "search",
            "currentnonselectedmarker",
            "--scope",
            ".",
            "--mode",
            "text",
        ],
    );
    assert!(!results(&nonselected_search).is_empty());

    let force_at = json_failure(&dir, &["reindex", "--regenerate", "--yes", "--at", &c1], 2);
    assert_eq!(force_at["error_code"], "KIO-E-CONFIG-USAGE-001");
}

fn discard_commit_tree(dir: &TempDir, commit_hash: &str) -> std::path::PathBuf {
    let repo = Repository::open(dir.path()).unwrap();
    let tree_hash = repo.read_commit(commit_hash).unwrap().tree;
    let path = ObjectStore::new(repo.kio_dir())
        .object_path(ObjectKind::Tree, &tree_hash)
        .unwrap();
    fs::remove_file(&path).unwrap();
    path
}

fn write_shallow_receipt(dir: &TempDir, commit_hash: &str) {
    let repo = Repository::open(dir.path()).unwrap();
    let tree_hash = repo.read_commit(commit_hash).unwrap().tree;
    let path = repo
        .kio_dir()
        .join("gc/shallowed")
        .join(commit_hash.strip_prefix("sha256:").unwrap());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let receipt = ShallowReceipt::new(
        commit_hash.to_owned(),
        tree_hash,
        "2026-08-14T00:00:00Z".into(),
    )
    .unwrap();
    fs::write(path, receipt.canonical_bytes().unwrap()).unwrap();
}

#[test]
fn ct4_timetravel_007_replica_revalidates_history_after_cas_becomes_shallow() {
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(
        dir.path().join("history.md"),
        "# Old\n\nshallowhistoryfixture old value\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("deleted.md"),
        "# Deleted\n\nshallowdeletedfixture old value\n",
    )
    .unwrap();
    let c1 = index_at(&dir, "2026-07-09T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(
        dir.path().join("history.md"),
        "# New\n\nreplacement without the historical search term\n",
    )
    .unwrap();
    fs::remove_file(dir.path().join("deleted.md")).unwrap();
    fs::write(
        dir.path().join("healthy.md"),
        "# Healthy\n\nshallowhistoryfixture shallowdeletedfixture healthy survivor\n",
    )
    .unwrap();
    index_at(&dir, "2026-07-10T00:00:00Z");

    // C1's tree_entries rows remain in sqlite after C2, but they are only cache.
    let conn = Connection::open(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let cached: i64 = conn
        .query_row(
            "SELECT count(*) FROM tree_entries WHERE commit_hash = ?1",
            params![c1],
            |row| row.get(0),
        )
        .unwrap();
    assert!(cached > 0, "fixture must retain historical cache rows");
    drop(conn);
    write_shallow_receipt(&dir, &c1);
    let c1_tree_path = discard_commit_tree(&dir, &c1);

    // A durable replica projection is never authority for a selected `--at`
    // tree: the CAS target itself must remain readable.
    let at = json_failure(
        &dir,
        &[
            "search",
            "shallowhistoryfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--at",
            c1.as_str(),
        ],
        1,
    );
    assert_eq!(at["error_code"], "KIO-E-COMMIT-SHALLOW-001");
    assert!(!c1_tree_path.exists());

    // A shallow ancestor is a partial walk, but aliases only found in that
    // tree are removed before the replica ranks its candidate depth. C2's
    // readable healthy binding must survive the C1 skip.
    let all_history = json_partial(
        &dir,
        &[
            "search",
            "shallowhistoryfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--all-history",
        ],
    );
    assert!(
        !results(&all_history).is_empty(),
        "a partial history walk must retain healthy survivors: {all_history}"
    );
    assert!(
        results(&all_history)
            .iter()
            .all(|result| result_path(result) == "healthy.md"),
        "only C2's readable binding may survive the shallow C1 skip: {all_history}"
    );
    assert_eq!(all_history["searched_scopes"][0]["shallow_skipped"], 1);
    assert!(!c1_tree_path.exists());

    // The first-parent deleted-alias planner has the same runtime CAS filter:
    // C1 was the only snapshot containing this final-deleted file, while C2's
    // live healthy binding proves --include-deleted stays partial, not empty.
    let include_deleted = json_partial(
        &dir,
        &[
            "search",
            "shallowdeletedfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--include-deleted",
        ],
    );
    assert!(
        !results(&include_deleted).is_empty(),
        "a partial include-deleted walk must retain healthy survivors: {include_deleted}"
    );
    assert!(
        results(&include_deleted)
            .iter()
            .all(|result| result_path(result) == "healthy.md"),
        "the shallow deleted alias must not replace C2's readable binding: {include_deleted}"
    );
    assert_eq!(include_deleted["searched_scopes"][0]["shallow_skipped"], 1);

    // A cursor snapshot that becomes shallow also hard-fails; it cannot serve a
    // partial page from cached HEAD rows.
    let cursor_dir = canonical_tempdir();
    init(&cursor_dir);
    for name in ["a.md", "b.md"] {
        fs::write(
            cursor_dir.path().join(name),
            format!("# {name}\n\nshallowcursorfixture {name}\n"),
        )
        .unwrap();
    }
    let head = index_at(&cursor_dir, "2026-07-10T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let page1 = json_success(
        &cursor_dir,
        &[
            "search",
            "shallowcursorfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--all-history",
            "--limit",
            "1",
        ],
    );
    let cursor = page1["paging"]["next_cursor"].as_str().unwrap();
    // The cursor remains bound to this snapshot after HEAD advances, making it
    // eligible for a completed markerless shallow receipt.
    fs::write(
        cursor_dir.path().join("advanced.md"),
        "# Advanced\n\nnew cursor fixture head\n",
    )
    .unwrap();
    index_at(&cursor_dir, "2026-07-10T00:00:01Z");
    write_shallow_receipt(&cursor_dir, &head);
    let head_tree_path = discard_commit_tree(&cursor_dir, &head);
    let page2 = json_failure(
        &cursor_dir,
        &[
            "search",
            "shallowcursorfixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--cursor",
            cursor,
        ],
        1,
    );
    assert_eq!(page2["error_code"], "KIO-E-COMMIT-SHALLOW-001");
    assert!(!head_tree_path.exists());
}

fn write_commit(store: &ObjectStore, commit: &CommitObject) -> String {
    let value = serde_json::to_value(commit).unwrap();
    store.write_json(ObjectKind::Commit, &value).unwrap().0
}

fn synthetic_commit(
    tree: String,
    parent: Option<String>,
    created_at: &str,
    message: &str,
    tool_lock_hash: String,
    commit_type: CommitType,
) -> CommitObject {
    CommitObject::new(
        tree,
        parent,
        created_at.to_owned(),
        message.to_owned(),
        tool_lock_hash,
        CommitStats {
            files_added: 0,
            files_modified: 0,
            files_deleted: 0,
        },
        commit_type,
    )
    .unwrap()
}

#[test]
fn ct4_timetravel_012_walks_one_linear_parent_chain_and_keeps_original_introduction() {
    let dir = canonical_tempdir();
    init(&dir);
    fs::write(
        dir.path().join("base.md"),
        "# Base\n\ncontent present on every branch\n",
    )
    .unwrap();
    let c0 = index_at(&dir, "2026-07-08T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(
        dir.path().join("old.md"),
        "# Side One\n\nmergefixture side-only binding one\n\n# Side Two\n\nmergefixture side-only binding two\n",
    )
    .unwrap();
    let a1 = index_at(&dir, "2026-07-09T00:00:00Z")["commit_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    let repo = Repository::open(dir.path()).unwrap();
    let c0_object = repo.read_commit(&c0).unwrap();
    let a1_object = repo.read_commit(&a1).unwrap();
    let store = ObjectStore::new(repo.kio_dir());

    // The chain revisits the same normalized binding and then drops it. The
    // original introduction remains the sole historical authority.
    let a2 = write_commit(
        &store,
        &synthetic_commit(
            a1_object.tree.clone(),
            Some(a1.clone()),
            "2026-07-09T02:00:00Z",
            "linear recurrence",
            a1_object.tool_lock_hash.clone(),
            CommitType::Manual,
        ),
    );
    let merge = write_commit(
        &store,
        &synthetic_commit(
            c0_object.tree,
            Some(a2.clone()),
            "2026-07-10T00:00:00Z",
            "linear delete",
            a1_object.tool_lock_hash,
            CommitType::Manual,
        ),
    );
    fs::write(dir.path().join(".kio/HEAD"), format!("{merge}\n")).unwrap();

    // Align the derived HEAD projection with synthetic M. The chunk ledger keeps
    // X, but M's HEAD tree intentionally does not contain it.
    json_success(&dir, &["repair", "rebuild-db"]);
    let history = search_all_history(&dir, "mergefixture", "100");
    assert_eq!(history["searched_scopes"][0]["snapshot_at"], merge);
    assert!(
        !results(&history).is_empty(),
        "the earlier linear binding must survive: {history}"
    );
    let canonical = a1.as_str();
    for result in results(&history) {
        assert_eq!(result_path(result), "old.md");
        assert_eq!(result_commit(result), canonical);
    }

    // Cursor replay retains the same linear introduction.
    let page1 = json_success(
        &dir,
        &[
            "search",
            "mergefixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--all-history",
            "--limit",
            "1",
        ],
    );
    let cursor = page1["paging"]["next_cursor"]
        .as_str()
        .expect("two-heading side fixture must page");
    let page2 = json_success(
        &dir,
        &[
            "search",
            "mergefixture",
            "--scope",
            ".",
            "--mode",
            "text",
            "--cursor",
            cursor,
            "--limit",
            "100",
        ],
    );
    assert_eq!(page2["searched_scopes"][0]["snapshot_at"], merge);
    assert!(
        results(&page2)
            .iter()
            .all(|result| result_commit(result) == canonical)
    );
}
