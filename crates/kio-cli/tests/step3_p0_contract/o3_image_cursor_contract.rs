//! External O3 draft: runtime unexecuted. Parent helpers remain private to this test binary.
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

type Leaves = Vec<(PathBuf, Option<(Vec<u8>, std::time::SystemTime)>)>;

// Observe bytes/mtime/Present|Absent without opening SQLite or changing lifecycle state.
fn ledger_leaves(dir: &TempDir) -> Leaves {
    let main = dir.path().join(".test-data/kio/cost-ledger.sqlite");
    let mut paths = [
        "",
        "-wal",
        "-shm",
        ".authority.json",
        ".checkpoint.json",
        ".init.pending",
    ]
    .map(|suffix| PathBuf::from(format!("{}{suffix}", main.display())))
    .to_vec();
    paths.push(main.parent().unwrap().join("ledger.lifecycle.lock"));
    paths
        .into_iter()
        .map(|path| {
            let state = match fs::symlink_metadata(&path) {
                Ok(meta) => {
                    assert!(meta.file_type().is_file(), "{}", path.display());
                    Some((fs::read(&path).unwrap(), meta.modified().unwrap()))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("{}: {error}", path.display()),
            };
            (path, state)
        })
        .collect()
}

fn run(dir: &TempDir, args: &[&str], remote: bool, trace: &Path) -> std::process::Output {
    let mut cmd = kio(dir, args);
    cmd.env(
        if remote {
            TEST_ADOPTED_EMBEDDING_ENV
        } else {
            TEST_DETERMINISTIC_EMBEDDING_ENV
        },
        if remote { "mock" } else { "scale-v3" },
    );
    cmd.env("KIO_TEST_QUERY_EMBED_TRACE", trace)
        .arg("--json")
        .output()
        .unwrap()
}

fn success(dir: &TempDir, args: &[&str], remote: bool, trace: &Path) -> Value {
    let out = run(dir, args, remote, trace);
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn cursor_error(dir: &TempDir, args: &[&str], trace: &Path) -> Value {
    let out = run(dir, args, false, trace);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        out.stdout.is_empty(),
        "failed replay must not emit rows: {out:?}"
    );
    let error: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(error["error_code"], "KIO-E-SEARCH-CURSOR-001", "{error}");
    error
}

// Fixture-only HMAC-SHA256 for the device's existing 32-byte test key.
fn cursor_mac(payload: &[u8], key: &[u8]) -> Vec<u8> {
    assert_eq!(key.len(), 32, "fixture cursor key must be 32 bytes");
    let mut inner_pad = [0x36_u8; 64];
    let mut outer_pad = [0x5c_u8; 64];
    for ((inner, outer), byte) in inner_pad.iter_mut().zip(&mut outer_pad).zip(key) {
        *inner ^= *byte;
        *outer ^= *byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(payload);
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner.finalize());
    outer.finalize().to_vec()
}

fn encode_fixture_cursor(payload: &Value, key: &[u8]) -> String {
    let bytes = serde_jcs::to_vec(payload).unwrap();
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&bytes),
        URL_SAFE_NO_PAD.encode(cursor_mac(&bytes, key))
    )
}

fn token(dir: &TempDir, response: &Value) -> (String, Value) {
    let encoded = response["paging"]["next_cursor"]
        .as_str()
        .expect("fixture must page at limit one, before replay behavior is tested")
        .to_owned();
    let key = fs::read(dir.path().join(".test-data/kio/cursor-key")).unwrap();
    let (payload, signature) = encoded.rsplit_once('.').unwrap();
    let bytes = URL_SAFE_NO_PAD.decode(payload).unwrap();
    let decoded: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        serde_jcs::to_vec(&decoded).unwrap(),
        bytes,
        "original cursor must be canonical JCS"
    );
    assert_eq!(
        URL_SAFE_NO_PAD.decode(signature).unwrap(),
        cursor_mac(&bytes, &key),
        "fixture HMAC must verify the actual original cursor"
    );
    assert_eq!(
        encode_fixture_cursor(&decoded, &key),
        encoded,
        "unchanged wire must round-trip exactly before rebinding"
    );
    (encoded, decoded)
}

// Full row identity includes the historical alias; payload alone is deliberately NOT unique.
fn signatures(response: &Value) -> Vec<String> {
    response["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            assert_eq!(row["result_type"], "image", "{row}");
            serde_json::json!([
                row["payload_uri"],
                row["chunk_hash"],
                row["evidence_pointer"]
            ])
            .to_string()
        })
        .collect()
}

fn traces(trace: &Path) -> Vec<u8> {
    fs::read(trace).unwrap()
}

fn write_image_search_config(dir: &TempDir, search: &str) {
    let path = dir.path().join(".kio/config.toml");
    let mut doc = fs::read_to_string(&path)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    let addition = search.parse::<toml_edit::DocumentMut>().unwrap();
    doc["search"] = addition["search"].clone();
    fs::write(path, doc.to_string()).unwrap();
}

fn index_figure_alias(dir: &TempDir) {
    kio(dir, &["index", "--online", "--json"])
        .env(TEST_DETERMINISTIC_EMBEDDING_ENV, "scale-v3")
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock_link_image")
        .assert()
        .success();
    kio(dir, &["batch", "resume", "--json"])
        .env(TEST_DETERMINISTIC_EMBEDDING_ENV, "scale-v3")
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock_link_image")
        .assert()
        .success();
    kio(dir, &["index", "--online", "--json"])
        .env(TEST_DETERMINISTIC_EMBEDDING_ENV, "scale-v3")
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock_link_image")
        .assert()
        .success();
}

fn document_images() -> TempDir {
    let dir = canonical_tempdir();
    fs::write(
        dir.path().join("figures.pdf"),
        fake_pdf(&[
            "cursorfigure page alpha",
            "cursorfigure page beta",
            "cursorfigure page gamma",
        ]),
    )
    .unwrap();
    kio(&dir, &["init"]).assert().success();
    initialize_paid_ledger(&dir);
    json_success_deterministic_embed(&dir, &["index"]);
    approve_markdownize(&dir, "mock_link_image");
    kio(&dir, &["batch", "resume"])
        .env(TEST_DETERMINISTIC_EMBEDDING_ENV, "scale-v3")
        .env(TEST_STANDARD_ONLINE_MARKDOWNIZE_ENV, "mock_link_image")
        .arg("--json")
        .assert()
        .success();
    json_success_deterministic_embed(&dir, &["index"]);
    write_image_search_config(&dir, "[search.diversify]\nstrategy = \"off\"\n");
    // Every SQLite connection is an immutable, short-lived observation, dropped
    // BEFORE any later CLI invocation. Do not call the parent's writable index_db.
    {
        let db = support::immutable_sqlite(dir.path().join(".kio/index/sqlite.db"));
        let n: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE target_type='image'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            n, 3,
            "fixture prerequisite: three normalized page owners/image embeddings"
        );
    }
    let control = json_success_deterministic_embed(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "vector",
            "--scope",
            ".",
            "--limit",
            "100",
        ],
    );
    assert_eq!(
        image_rows(&control).len(),
        3,
        "existing-mode fixture prerequisite: {control}"
    );
    assert!(
        image_rows(&control)
            .iter()
            .all(|row| row["evidence_pointer"]["path_at_commit"] == "figures.pdf")
    );
    dir
}

fn image_page(dir: &TempDir, trace: &Path) -> Value {
    success(
        dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "image",
            "--scope",
            ".",
            "--limit",
            "1",
        ],
        false,
        trace,
    )
}

#[test]
fn o3_image_alias_cursor_pages_exact_final_stream_without_resend_or_ledger_write() {
    let dir = document_images();
    // Same normalized identity through rename + simultaneous live copy.
    fs::rename(
        dir.path().join("figures.pdf"),
        dir.path().join("renamed.pdf"),
    )
    .unwrap();
    index_figure_alias(&dir);
    fs::copy(dir.path().join("renamed.pdf"), dir.path().join("copy.pdf")).unwrap();
    index_figure_alias(&dir);
    let legacy = json_success_deterministic_embed(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "vector",
            "--scope",
            ".",
            "--all-history",
            "--limit",
            "100",
        ],
    );
    let legacy_images = serde_json::json!({"results":image_rows(&legacy)});
    let legacy_signatures = signatures(&legacy_images)
        .into_iter()
        .collect::<BTreeSet<_>>();
    // Current stream expands retained commit/path bindings, including repeated
    // paths at distinct commits; the existing Vector control defines that unit.
    assert!(
        legacy_signatures.len() >= 9,
        "all three paths need authentic history: {legacy}"
    );
    let retained_bindings = {
        let db = support::immutable_sqlite(dir.path().join(".test-cache/kio/aggregator.sqlite"));
        let mut stmt=db.prepare("SELECT DISTINCT b.pointer_commit,b.path_at_commit FROM agg_bindings b JOIN agg_image_refs r ON r.scope_id=b.scope_id AND r.chunk_id=b.chunk_id WHERE b.selector_kind='all_history' AND b.path_at_commit IN ('figures.pdf','renamed.pdf','copy.pdf')").unwrap();
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .collect::<std::result::Result<BTreeSet<_>, _>>()
            .unwrap()
    };
    assert!(retained_bindings.len() >= 3);
    let trace = dir.path().join("image-cursor.trace");
    let full = success(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "image",
            "--scope",
            ".",
            "--all-history",
            "--limit",
            "100",
        ],
        false,
        &trace,
    );
    let expected = signatures(&full);
    assert_eq!(
        expected.iter().cloned().collect::<BTreeSet<_>>(),
        legacy_signatures,
        "Image follows existing chunk alias stream: {full}"
    );
    assert_eq!(
        expected.len(),
        legacy_signatures.len(),
        "no duplicate output rows"
    );
    for payload in full["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["payload_uri"].clone())
    {
        let rows = full["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["payload_uri"] == payload)
            .collect::<Vec<_>>();
        assert_eq!(
            rows.iter()
                .map(|row| row["evidence_pointer"]["path_at_commit"].as_str().unwrap())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["copy.pdf", "figures.pdf", "renamed.pdf"])
        );
        assert_eq!(
            rows.iter()
                .map(|row| (
                    row["evidence_pointer"]["commit"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    row["evidence_pointer"]["path_at_commit"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                ))
                .collect::<BTreeSet<_>>(),
            retained_bindings,
            "each image expands exactly the retained commit/path bindings"
        );
        for row in rows {
            assert_eq!(row["evidence_pointer"]["chunk_hash"], row["chunk_hash"]);
            assert_eq!(
                row["current_paths"],
                serde_json::json!(["copy.pdf", "renamed.pdf"])
            );
            assert!(
                row.get("current_path").is_none(),
                "twins are ambiguous: {row}"
            );
        }
    }
    let mut page = success(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "image",
            "--scope",
            ".",
            "--all-history",
            "--limit",
            "1",
        ],
        false,
        &trace,
    );
    let frozen_trace = traces(&trace);
    assert_eq!(
        String::from_utf8(frozen_trace.clone())
            .unwrap()
            .lines()
            .count(),
        2,
        "full search and fresh page one are positive controls for the query trace"
    );
    let frozen_ledger = ledger_leaves(&dir);
    let mut got = signatures(&page);
    let (first_cursor, _) = token(&dir, &page);
    let mut ended = false;
    for _ in 0..(expected.len() + 1) {
        let Some(cursor) = page["paging"]["next_cursor"].as_str().map(str::to_owned) else {
            ended = true;
            break;
        };
        // Omit --all-history: replay inherits the frozen selector, as existing chunk cursors do.
        page = success(
            &dir,
            &[
                "search",
                "cursorfigure",
                "--mode",
                "image",
                "--scope",
                ".",
                "--cursor",
                &cursor,
                "--limit",
                "1",
            ],
            false,
            &trace,
        );
        got.extend(signatures(&page));
        assert_eq!(traces(&trace), frozen_trace);
        assert_eq!(ledger_leaves(&dir), frozen_ledger);
    }
    assert!(ended, "cursor must terminate");
    assert_eq!(
        got, expected,
        "no alias duplicate/drop/reordering across page boundaries"
    );
    assert_eq!(got.iter().collect::<BTreeSet<_>>().len(), got.len());
    let repeated = success(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "image",
            "--scope",
            ".",
            "--cursor",
            &first_cursor,
            "--limit",
            "1",
        ],
        false,
        &trace,
    );
    assert_eq!(signatures(&repeated), expected[1..2]);
    assert_eq!(traces(&trace), frozen_trace);
    assert_eq!(ledger_leaves(&dir), frozen_ledger);

    // Current selector still chooses a single live binding per image candidate.
    let current = success(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "image",
            "--scope",
            ".",
            "--limit",
            "100",
        ],
        false,
        &trace,
    );
    assert_eq!(signatures(&current).len(), 3, "{current}");
    assert!(
        current["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["evidence_pointer"]["path_at_commit"] == "copy.pdf")
    );
    // Same shared raw cap BEFORE aliases, not a one-document aggregation or post-alias cap.
    write_image_search_config(&dir, "[search.diversify]\nmax_per_raw_hash = 1\n");
    let capped = success(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "image",
            "--scope",
            ".",
            "--all-history",
            "--limit",
            "100",
        ],
        false,
        &trace,
    );
    assert_eq!(
        signatures(&capped).len(),
        retained_bindings.len(),
        "one semantic image expands its retained commit/path aliases: {capped}"
    );
    assert_eq!(
        capped["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["payload_uri"].as_str().unwrap())
            .collect::<BTreeSet<_>>()
            .len(),
        1
    );
}

#[test]
fn o3_image_cursor_binds_mode_and_digest_even_with_a_valid_matching_cache_entry() {
    let dir = document_images();
    let trace = dir.path().join("image-digest.trace");
    let first = image_page(&dir, &trace);
    let (cursor, decoded) = token(&dir, &first);
    let vector = success(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "vector",
            "--scope",
            ".",
            "--limit",
            "1",
        ],
        false,
        &trace,
    );
    let (vector_cursor, vector_decoded) = token(&dir, &vector);
    assert_eq!(
        decoded["query_vector_digest"], vector_decoded["query_vector_digest"],
        "same deterministic query/profile must provide the same vector control"
    );
    assert_ne!(
        decoded["query_hash"], vector_decoded["query_hash"],
        "image must remain a distinct mode in query_hash"
    );
    let frozen_trace = traces(&trace);
    let frozen_ledger = ledger_leaves(&dir);
    cursor_error(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "vector",
            "--scope",
            ".",
            "--cursor",
            &cursor,
            "--limit",
            "1",
        ],
        &trace,
    );
    cursor_error(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "image",
            "--scope",
            ".",
            "--cursor",
            &vector_cursor,
            "--limit",
            "1",
        ],
        &trace,
    );

    let original_digest = decoded["query_vector_digest"].as_str().unwrap();
    let cache = dir.path().join(".test-cache/kio/search-query-cache");
    let original = fs::read(cache.join(original_digest.strip_prefix("sha256:").unwrap())).unwrap();
    assert!(!original.is_empty() && original.len().is_multiple_of(4));
    let mut different = original.clone();
    // A distinct, finite same-dimension vector, with its REAL content digest and matching cache.
    let old = f32::from_le_bytes(different[..4].try_into().unwrap());
    different[..4].copy_from_slice(&(if old == 0.25 { 0.5_f32 } else { 0.25_f32 }).to_le_bytes());
    let digest = kio_core::cas::hash_bytes(&different);
    assert_ne!(digest.as_str(), original_digest);
    fs::write(
        cache.join(digest.strip_prefix("sha256:").unwrap()),
        &different,
    )
    .unwrap();
    let mut rebound = decoded.clone();
    rebound["query_vector_digest"] = Value::String(digest);
    assert_eq!(rebound["query_hash"], decoded["query_hash"]);
    // Test fixture's own private key only. Keep query_hash unchanged to exercise digest binding,
    // rather than the unrelated HMAC-invalid or cache-missing branches.
    let key = fs::read(dir.path().join(".test-data/kio/cursor-key")).unwrap();
    let signed = encode_fixture_cursor(&rebound, &key);
    let error = cursor_error(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "image",
            "--scope",
            ".",
            "--cursor",
            &signed,
            "--limit",
            "1",
        ],
        &trace,
    );
    assert!(
        error["context"]["expected"].is_string(),
        "query-hash check must own refusal: {error}"
    );
    assert_eq!(error["context"]["actual"], decoded["query_hash"]);
    assert_eq!(traces(&trace), frozen_trace);
    assert_eq!(ledger_leaves(&dir), frozen_ledger);
}

#[test]
fn o3_image_cursor_missing_or_corrupt_cache_fails_closed_without_resend() {
    for corrupt in [false, true] {
        let dir = document_images();
        let trace = dir.path().join("image-cache.trace");
        let first = image_page(&dir, &trace);
        let (cursor, decoded) = token(&dir, &first);
        let digest = decoded["query_vector_digest"].as_str().unwrap();
        let leaf = dir
            .path()
            .join(".test-cache/kio/search-query-cache")
            .join(digest.strip_prefix("sha256:").unwrap());
        assert!(
            leaf.is_file(),
            "page one must actually persist its query cache"
        );
        let frozen_trace = traces(&trace);
        assert_eq!(
            String::from_utf8(frozen_trace.clone())
                .unwrap()
                .lines()
                .count(),
            1
        );
        let frozen_ledger = ledger_leaves(&dir);
        if corrupt {
            fs::remove_file(&leaf).unwrap();
            fs::write(&leaf, b"not a query vector").unwrap();
        } else {
            fs::remove_file(&leaf).unwrap();
        }
        let error = cursor_error(
            &dir,
            &[
                "search",
                "cursorfigure",
                "--mode",
                "image",
                "--scope",
                ".",
                "--cursor",
                &cursor,
                "--limit",
                "1",
            ],
            &trace,
        );
        assert_eq!(error["context"]["reason"], "query_vector_unavailable");
        assert_eq!(
            traces(&trace),
            frozen_trace,
            "cache failure must not resend"
        );
        assert_eq!(ledger_leaves(&dir), frozen_ledger);
    }
}

#[test]
fn o3_image_purge_discards_saved_cursor_and_preserves_only_the_surviving_source() {
    let dir = document_images();
    fs::write(
        dir.path().join("survivor.png"),
        include_bytes!("../fixtures/o2_golden/png-original-control.png"),
    )
    .unwrap();
    // Native source ownership is retained by the first index, enriched on the next.
    for _ in 0..2 {
        json_success_deterministic_embed(&dir, &["index", "--offline"]);
    }
    json_success_deterministic_embed(&dir, &["index"]);
    let legacy = json_success_deterministic_embed(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "vector",
            "--scope",
            ".",
            "--limit",
            "100",
        ],
    );
    assert_eq!(
        image_rows(&legacy).len(),
        4,
        "existing-mode purge survivor prerequisite: {legacy}"
    );
    let trace = dir.path().join("image-purge.trace");
    let full = success(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "image",
            "--scope",
            ".",
            "--limit",
            "100",
        ],
        false,
        &trace,
    );
    assert_eq!(
        signatures(&full).len(),
        4,
        "fixture needs three figures plus a surviving standalone image: {full}"
    );
    let survivor_hash = kio_core::cas::hash_bytes(include_bytes!(
        "../fixtures/o2_golden/png-original-control.png"
    ));
    let survivor = full["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["evidence_pointer"]["path_at_commit"] == "survivor.png")
        .expect("pre-purge current Image query must expose the authentic standalone owner");
    assert_eq!(survivor["result_type"], "image");
    assert!(
        survivor["payload_uri"]
            .as_str()
            .unwrap()
            .ends_with(&survivor_hash)
    );
    let survivor_embedding: String = {
        let db = support::immutable_sqlite(dir.path().join(".kio/index/sqlite.db"));
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE target_type='image' AND target_id=?1",
                [&survivor_hash],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "pre-purge survivor must have its own canonical source image vector"
        );
        db.query_row(
            "SELECT id FROM embeddings WHERE target_type='image' AND target_id=?1",
            [&survivor_hash],
            |r| r.get(0),
        )
        .unwrap()
    };
    {
        let repo = Repository::open(dir.path()).unwrap();
        let object = repo
            .object_store()
            .read_embedding(&survivor_embedding)
            .unwrap();
        assert_eq!(object.target_type, "image");
        assert_eq!(object.target_hash, survivor_hash);
    }
    let purged = full["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["evidence_pointer"]["path_at_commit"] == "figures.pdf")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(purged.len(), 3);
    let raw = purged[0]["evidence_pointer"]["raw_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let first = image_page(&dir, &trace);
    let (cursor, _) = token(&dir, &first);
    fs::remove_file(dir.path().join("figures.pdf")).unwrap();
    let purge = json_success_deterministic_embed(
        &dir,
        &["purge", "--raw-hash", &raw, "--reason", "legal", "--yes"],
    );
    assert_eq!(purge["status"], "purged", "{purge}");
    // Test-only read observations close all immutable handles before replay.
    kio_index::vec::ensure_registered();
    {
        let repo = Repository::open(dir.path()).unwrap();
        let head = repo.head_commit_hash().unwrap().unwrap();
        let tree = repo
            .read_tree(&repo.read_commit(&head).unwrap().tree)
            .unwrap();
        let cas = repo
            .object_store()
            .read_embedding(&survivor_embedding)
            .unwrap();
        assert_eq!(cas.target_type, "image");
        assert_eq!(cas.target_hash, survivor_hash);
        assert!(
            tree.entries.iter().any(|entry| entry.path == "survivor.png"
                && entry.raw_hash == survivor_hash
                && entry.normalize.is_some()),
            "purged HEAD must retain survivor typed normalization"
        );
        let source = support::immutable_sqlite(dir.path().join(".kio/index/sqlite.db"));
        let replica =
            support::immutable_sqlite(dir.path().join(".test-cache/kio/aggregator.sqlite"));
        let source_counts: Vec<(String, i64)> = [
            (
                "current_head_tree_entries",
                "SELECT COUNT(*) FROM tree_entries WHERE commit_hash=?1",
            ),
            (
                "survivor_embeddings",
                "SELECT COUNT(*) FROM embeddings WHERE target_type='image' AND target_id=?1",
            ),
            (
                "survivor_image_vec",
                "SELECT COUNT(*) FROM image_vec WHERE image_id=?1",
            ),
            (
                "survivor_chunks",
                "SELECT COUNT(*) FROM chunks WHERE raw_hash=?1",
            ),
        ]
        .into_iter()
        .map(|(name, sql)| {
            (
                name.to_owned(),
                source
                    .query_row(
                        sql,
                        [if name == "current_head_tree_entries" {
                            &head
                        } else {
                            &survivor_hash
                        }],
                        |r| r.get(0),
                    )
                    .unwrap(),
            )
        })
        .collect();
        let replica_counts:Vec<(String,i64)>=[
            ("current_bindings","SELECT COUNT(*) FROM agg_bindings WHERE selector_kind='current' AND raw_hash=?1"),
            ("history_bindings","SELECT COUNT(*) FROM agg_bindings WHERE selector_kind='all_history' AND raw_hash=?1"),
            ("image_refs","SELECT COUNT(*) FROM agg_image_refs WHERE image_id=?1"),
            ("image_vectors_profiles","SELECT COUNT(*) FROM agg_image_embeddings WHERE image_id=?1")
        ].into_iter().map(|(name,sql)| (name.to_owned(),replica.query_row(sql,[&survivor_hash],|r| r.get(0)).unwrap())).collect();
        for (name, count) in source_counts {
            if name == "current_head_tree_entries" {
                assert!(count > 0, "finalized HEAD cache must contain survivors");
            } else {
                assert_eq!(count, 1, "survivor source surface {name}");
            }
        }
        for (name, count) in replica_counts {
            assert_eq!(count, 1, "survivor replica surface {name}");
        }
    }
    let frozen_trace = traces(&trace);
    let frozen_ledger = ledger_leaves(&dir);
    let error = cursor_error(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "image",
            "--scope",
            ".",
            "--cursor",
            &cursor,
            "--limit",
            "1",
        ],
        &trace,
    );
    let serialized = error.to_string();
    for row in &purged {
        for field in ["payload_uri", "evidence_uri", "chunk_hash"] {
            assert!(
                !serialized.contains(row[field].as_str().unwrap()),
                "{error}"
            );
        }
    }
    assert!(!serialized.contains("figures.pdf"), "{error}");
    assert_eq!(traces(&trace), frozen_trace);
    assert_eq!(ledger_leaves(&dir), frozen_ledger);
    let fresh = success(
        &dir,
        &[
            "search",
            "cursorfigure",
            "--mode",
            "image",
            "--scope",
            ".",
            "--limit",
            "100",
        ],
        false,
        &trace,
    );
    let rows = fresh["results"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "purged figures must not displace or leak beside the survivor: {fresh}"
    );
    assert_eq!(rows[0]["result_type"], "image");
    assert_eq!(
        rows[0]["evidence_pointer"]["path_at_commit"],
        "survivor.png"
    );
    for row in &purged {
        assert_ne!(rows[0]["payload_uri"], row["payload_uri"]);
    }
}

#[test]
fn o3_image_remote_revoke_refuses_cached_query_without_resend_or_result_payloads() {
    let fixture = original_online_png_fixture();
    let dir = &fixture.dir;
    fs::write(
        dir.path().join("scan-other.jpg"),
        include_bytes!("../fixtures/o2_golden/jpeg-original-control.jpg"),
    )
    .unwrap();
    // Preserve the real native typed source owner; no user link confers image authority.
    json_success_embed(dir, "mock", &["index", "--offline"]);
    json_success_embed(dir, "mock", &["index", "--online", "--realtime"]);
    let trace = dir.path().join("image-revoke.trace");
    let control = success(
        dir,
        &[
            "search",
            "scan original",
            "--mode",
            "vector",
            "--scope",
            ".",
            "--limit",
            "100",
        ],
        true,
        &trace,
    );
    let images = image_rows(&control);
    assert_eq!(
        images.len(),
        2,
        "existing-mode remote fixture prerequisite: {control}"
    );
    assert_eq!(
        images
            .iter()
            .map(|row| row["evidence_pointer"]["path_at_commit"].as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["scan-other.jpg", "scan.png"])
    );
    let first = success(
        dir,
        &[
            "search",
            "scan original",
            "--mode",
            "image",
            "--scope",
            ".",
            "--limit",
            "1",
        ],
        true,
        &trace,
    );
    let (cursor, _) = token(dir, &first);
    assert_eq!(first["resolved_mode"], "image");
    json_success_embed(dir, "mock", &["adapter", "revoke", "gemini_embedding_2"]);
    let frozen_trace = traces(&trace);
    let frozen_ledger = ledger_leaves(dir);
    for (attempt, args) in [
        vec![
            "search",
            "scan original",
            "--mode",
            "image",
            "--scope",
            ".",
            "--limit",
            "1",
        ],
        vec![
            "search",
            "scan original",
            "--mode",
            "image",
            "--scope",
            ".",
            "--cursor",
            &cursor,
            "--limit",
            "1",
        ],
    ]
    .into_iter()
    .enumerate()
    {
        let out = run(dir, &args, true, &trace);
        assert!(
            !out.status.success(),
            "revoked paired grant must not be replaced by cache: {out:?}"
        );
        assert!(
            out.stdout.is_empty(),
            "no rows/payloads on denied query: {out:?}"
        );
        let error: Value = serde_json::from_slice(&out.stderr).unwrap();
        if attempt == 0 {
            assert_eq!(out.status.code(), Some(1), "{error}");
            assert_eq!(error["error_code"], "KIO-E-SEARCH-VEC-UNAUTHORIZED-001");
        }
        assert!(
            matches!(
                (out.status.code(), error["error_code"].as_str()),
                (Some(1), Some("KIO-E-SEARCH-VEC-UNAUTHORIZED-001"))
                    | (Some(2), Some("KIO-E-SEARCH-CURSOR-001"))
            ),
            "{error}"
        );
        let serialized = error.to_string();
        for image in &images {
            for field in ["payload_uri", "evidence_uri", "chunk_hash"] {
                assert!(
                    !serialized.contains(image[field].as_str().unwrap()),
                    "{error}"
                );
            }
        }
        assert_eq!(traces(&trace), frozen_trace);
        assert_eq!(ledger_leaves(dir), frozen_ledger);
    }
    // Revocation constrains this remote query path; it does not erase ownership,
    // purge old CAS, or assert that deterministic/local modes lose their images.
}

#[test]
fn o3_text_search_ignores_large_untrusted_image_owner_metadata() {
    let dir = document_images();
    let args = [
        "search", "mock", "--mode", "text", "--scope", ".", "--limit", "100",
    ];
    let before = json_success_deterministic_embed(&dir, &args);
    assert_eq!(before["resolved_mode"], "text");
    assert!(
        !before["results"].as_array().unwrap().is_empty(),
        "real lexical positive control: {before}"
    );
    assert!(
        before["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["result_type"] == "chunk")
    );
    let replica_path = dir.path().join(".test-cache/kio/aggregator.sqlite");
    {
        // This disposable replica is an untrusted cache. All added refs use a
        // genuine current owner chunk and canonical local URI/hash syntax, but
        // acquire no CAS ownership. Text ranking must not enumerate them.
        let mut replica = rusqlite::Connection::open(&replica_path).unwrap();
        let (scope, chunk): (String, String) = replica.query_row(
            "SELECT c.scope_id,c.chunk_id FROM agg_chunks c JOIN agg_bindings b ON b.scope_id=c.scope_id AND b.chunk_id=c.chunk_id WHERE b.selector_kind='current' ORDER BY c.scope_id,c.chunk_id LIMIT 1", [],
            |row| Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
        let tx = replica.transaction().unwrap();
        {
            let mut insert = tx.prepare("INSERT INTO agg_image_refs(scope_id,image_id,chunk_id,image_uri) VALUES(?1,?2,?3,?4)").unwrap();
            for n in 1..=100_001_u64 {
                let image = format!("sha256:{n:064x}");
                insert
                    .execute(rusqlite::params![
                        scope,
                        image,
                        chunk,
                        format!("kio://{scope}/object/image/{image}")
                    ])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    }
    {
        let replica = support::immutable_sqlite(&replica_path);
        let count: i64 = replica.query_row(
            "SELECT COUNT(*) FROM (SELECT DISTINCT r.scope_id,r.image_id,b.raw_hash,b.tool_profile_hash,b.gen,b.manifest_hash,c.unit_key FROM agg_image_refs r JOIN agg_chunks c ON c.scope_id=r.scope_id AND c.chunk_id=r.chunk_id JOIN agg_bindings b ON b.scope_id=c.scope_id AND b.chunk_id=c.chunk_id WHERE b.selector_kind='current')", [], |row| row.get(0),
        ).unwrap();
        assert!(
            count > 100_000,
            "fixture must cross the image enumeration bound"
        );
    }
    // Observation handles close before either subsequent CLI invocation.
    let source = fs::read(dir.path().join(".kio/index/sqlite.db")).unwrap();
    let ledger = ledger_leaves(&dir);
    let after = json_success_deterministic_embed(&dir, &args);
    assert_eq!(after["resolved_mode"], "text");
    assert_eq!(
        after["results"], before["results"],
        "irrelevant image cache metadata cannot alter lexical results"
    );
    assert_eq!(
        fs::read(dir.path().join(".kio/index/sqlite.db")).unwrap(),
        source
    );
    assert_eq!(ledger_leaves(&dir), ledger);
}
