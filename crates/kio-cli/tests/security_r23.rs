mod support;

use support::canonical_tempdir;

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use kio_core::cas::hash_bytes;
use serde_json::{Value, json};

const TEST_ADOPTED_EMBEDDING_ENV: &str = "KIO_TEST_GEMINI_EMBED";

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

fn hermetic_kio_command() -> Command {
    let mut command = Command::cargo_bin("kio").unwrap();
    for name in KIO_CHILD_ENV_DENYLIST {
        command.env_remove(name);
    }
    command
}

fn kio_at(cwd: &Path, xdg: &Path, args: &[&str]) -> Command {
    let mut command = hermetic_kio_command();
    command
        .current_dir(cwd)
        .env("XDG_CONFIG_HOME", xdg.join("config"))
        .env("XDG_DATA_HOME", xdg.join("data"))
        .env("XDG_CACHE_HOME", xdg.join("cache"))
        .args(args);
    command
}

fn kio_embed_at(cwd: &Path, xdg: &Path, args: &[&str]) -> Command {
    let mut command = kio_at(cwd, xdg, args);
    command.env(TEST_ADOPTED_EMBEDDING_ENV, "mock");
    command
}

fn json_success(cwd: &Path, xdg: &Path, args: &[&str]) -> Value {
    let output = kio_at(cwd, xdg, args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn json_success_embed(cwd: &Path, xdg: &Path, args: &[&str]) -> Value {
    let output = kio_embed_at(cwd, xdg, args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn json_failure(cwd: &Path, xdg: &Path, args: &[&str], code: i32) -> Value {
    let output = kio_at(cwd, xdg, args)
        .arg("--json")
        .assert()
        .code(code)
        .get_output()
        .stderr
        .clone();
    serde_json::from_slice(&output).unwrap()
}

/// 05 §1.7.2 / §4.2 (2026-08-11): `kio view --json` no longer returns a
/// `text` field -- recover chunk-adjacent text the same way any other caller
/// must: read `view_path` and slice it to `[view_byte_start, view_byte_end)`.
fn view_slice(viewed: &Value) -> String {
    let view_path = viewed["view_path"]
        .as_str()
        .unwrap_or_else(|| panic!("view_path must be a resolvable path: {viewed}"));
    let start = viewed["view_byte_start"]
        .as_u64()
        .unwrap_or_else(|| panic!("view_byte_start must be present: {viewed}"))
        as usize;
    let end = viewed["view_byte_end"]
        .as_u64()
        .unwrap_or_else(|| panic!("view_byte_end must be present: {viewed}"))
        as usize;
    let bytes = fs::read(view_path)
        .unwrap_or_else(|err| panic!("failed to read view_path {view_path}: {err}"));
    String::from_utf8(bytes[start..end].to_vec()).unwrap()
}

fn init_scope(root: &Path, xdg: &Path) {
    json_success(root, xdg, &["init"]);
}

fn first_search_result(search: &Value) -> &Value {
    &search["results"].as_array().unwrap()[0]
}

fn tasks_of_type<'a>(status: &'a Value, kind: &str) -> Vec<&'a Value> {
    status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|task| task["type"] == kind)
        .collect()
}

fn active_permitted_grant(status: &Value, tool_id: &str) -> bool {
    let active = status["grants"].as_array().is_some_and(|grants| {
        grants
            .iter()
            .any(|grant| grant["state"] == "active" && grant["binding"]["tool_id"] == tool_id)
    });
    active
        && status["effective"]
            .as_array()
            .is_some_and(|effective| effective.iter().any(|grant| grant["permitted"] == true))
}

#[test]
fn cand_069_inline_pointer_rejects_malformed_hash_before_resolution() {
    let temp = canonical_tempdir();
    let xdg = temp.path().join("xdg");
    let scope = temp.path().join("scope");
    fs::create_dir_all(&scope).unwrap();
    fs::write(
        scope.join("note.md"),
        "# Note\n\nalphaunique evidence text\n",
    )
    .unwrap();

    init_scope(&scope, &xdg);
    json_success(&scope, &xdg, &["index"]);
    let search = json_success(&scope, &xdg, &["search", "alphaunique"]);
    let pointer = first_search_result(&search)["evidence_pointer"].clone();

    let control = json_success(&scope, &xdg, &["view", &pointer.to_string()]);
    assert!(
        view_slice(&control).contains("alphaunique evidence text"),
        "valid inline pointer must still resolve: {control}"
    );

    let bait = temp.path().join("bait.txt");
    fs::write(&bait, "this file must not be used as a raw hash").unwrap();
    let mut malformed = pointer;
    malformed["raw_hash"] = json!(bait.display().to_string());
    malformed["path_at_commit"] = json!("bait.txt");

    let err = json_failure(&scope, &xdg, &["view", &malformed.to_string()], 2);
    assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
    let message = err["message"].as_str().unwrap();
    assert!(
        message.contains("raw_hash") && message.contains("64 lowercase hexadecimal"),
        "malformed inline pointer should fail at pointer validation: {err}"
    );
    assert!(
        !err.to_string().contains("this file must not be used"),
        "malformed pointer must not reach file resolution: {err}"
    );
}

#[test]
fn cand_025_portable_approvals_do_not_grant_new_root_but_local_approval_does() {
    let temp = canonical_tempdir();
    let xdg = temp.path().join("xdg");
    let approved = temp.path().join("approved");
    let copied = temp.path().join("copied");
    fs::create_dir_all(&approved).unwrap();
    fs::create_dir_all(&copied).unwrap();
    fs::write(
        approved.join("approved.md"),
        "# Approved\n\nalpha approval source\n",
    )
    .unwrap();
    fs::write(
        copied.join("copied.md"),
        "# Copied\n\nbeta approval target\n",
    )
    .unwrap();

    init_scope(&approved, &xdg);
    init_scope(&copied, &xdg);
    json_success(&approved, &xdg, &["ledger", "init"]);
    json_success_embed(
        &approved,
        &xdg,
        &["adapter", "approve", "gemini_embedding_2", "--yes"],
    );
    let approved_status = json_success_embed(
        &approved,
        &xdg,
        &["adapter", "status", "gemini_embedding_2"],
    );
    assert!(
        active_permitted_grant(&approved_status, "gemini_embedding_2"),
        "explicit source approval must permit the embedding adapter: {approved_status}"
    );

    // Scope approval references are portable audit data, while the matching
    // authenticated device grant remains bound to the original root. Copy the
    // portable row only to prove that it cannot authorize this new root.
    let approved_scope: Value =
        serde_json::from_slice(&fs::read(approved.join(".kio/scope.json")).unwrap()).unwrap();
    let mut copied_scope: Value =
        serde_json::from_slice(&fs::read(copied.join(".kio/scope.json")).unwrap()).unwrap();
    copied_scope["approvals"] = approved_scope["approvals"].clone();
    fs::write(
        copied.join(".kio/scope.json"),
        serde_json::to_vec_pretty(&copied_scope).unwrap(),
    )
    .unwrap();

    let copied_without_local = json_success_embed(&copied, &xdg, &["index"]);
    assert_eq!(copied_without_local["network_allowed"], false);
    assert_eq!(copied_without_local["network_opt_in"], false);
    let status = json_success_embed(&copied, &xdg, &["status"]);
    let embedding = tasks_of_type(&status, "embedding");
    assert!(
        !embedding.is_empty(),
        "embedding task should be enqueued: {status}"
    );
    assert!(
        embedding.iter().all(|task| {
            task["status"] == "pending" && task["fallback_reason"] == "network_opt_in_required"
        }),
        "copied portable approvals must not execute embedding: {status}"
    );

    json_success_embed(
        &copied,
        &xdg,
        &["adapter", "approve", "gemini_embedding_2", "--yes"],
    );
    let copied_approval_status =
        json_success_embed(&copied, &xdg, &["adapter", "status", "gemini_embedding_2"]);
    assert!(
        active_permitted_grant(&copied_approval_status, "gemini_embedding_2"),
        "explicit local approval must permit the copied root: {copied_approval_status}"
    );
    json_success_embed(&copied, &xdg, &["index"]);
    let status = json_success_embed(&copied, &xdg, &["status"]);
    let embedding = tasks_of_type(&status, "embedding");
    assert!(
        embedding.iter().all(|task| task["status"] == "done"),
        "explicit local approval should authorize this canonical root: {status}"
    );
}

#[test]
fn cand_064_malformed_tool_lock_blocks_batch_before_task_mutation() {
    let temp = canonical_tempdir();
    let xdg = temp.path().join("xdg");
    let scope = temp.path().join("scope");
    fs::create_dir_all(&scope).unwrap();
    fs::write(scope.join("doc.md"), "# Doc\n\nbatch preflight\n").unwrap();
    init_scope(&scope, &xdg);

    let input_hash = hash_bytes(b"# Doc\n\nbatch preflight\n");
    let task_path = scope.join(".kio/tasks.jsonl");
    let rows = [
        json!({
            "task_id": "cand064-paused",
            "type": "markdownize",
            "mode": "full",
            "input_path": "doc.md",
            "input_hash": input_hash.clone(),
            "previous_raw_hash": null,
            "parent_run_id": null,
            "changed_unit_keys": [],
            "output_ref": "online:mistral_ocr_markdownize",
            "unit_keys": null,
            "status": "paused",
            "attempts": 0,
            "next_retry_at": null,
            "deadline": null,
            "heartbeat_at": null,
            "fallback_reason": "budget_exceeded",
            "created_at": "2026-07-12T00:00:00Z"
        }),
        json!({
            "task_id": "cand064-failed",
            "type": "markdownize",
            "mode": "full",
            "input_path": "doc.md",
            "input_hash": input_hash,
            "previous_raw_hash": null,
            "parent_run_id": null,
            "changed_unit_keys": [],
            "output_ref": "online:mistral_ocr_markdownize",
            "unit_keys": null,
            "status": "failed",
            "attempts": 0,
            "next_retry_at": null,
            "deadline": null,
            "heartbeat_at": null,
            "fallback_reason": "network_error",
            "created_at": "2026-07-12T00:00:01Z"
        }),
    ];
    let mut task_jsonl = String::new();
    for row in rows {
        task_jsonl.push_str(&serde_json::to_string(&row).unwrap());
        task_jsonl.push('\n');
    }
    fs::write(&task_path, task_jsonl).unwrap();
    let before = fs::read(&task_path).unwrap();

    fs::write(
        scope.join(".kio/tool-lock.json"),
        r#"{"spec_version":"bad"}"#,
    )
    .unwrap();

    for args in [
        &["batch", "resume", "--override-budget"][..],
        &["batch", "retry"][..],
    ] {
        let err = json_failure(&scope, &xdg, args, 2);
        assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
        assert!(
            err["message"].as_str().unwrap().contains("tool-lock"),
            "tool-lock schema failure should be surfaced before batch state changes: {err}"
        );
        assert_eq!(
            fs::read(&task_path).unwrap(),
            before,
            "batch command {args:?} mutated tasks despite malformed tool-lock"
        );
    }

    fs::remove_file(scope.join(".kio/tool-lock.json")).unwrap();
    for args in [
        &["batch", "resume", "--override-budget"][..],
        &["batch", "retry"][..],
    ] {
        let err = json_failure(&scope, &xdg, args, 2);
        assert_eq!(err["error_code"], "KIO-E-CONFIG-SCHEMA-001");
        assert!(
            err["message"].as_str().unwrap().contains("tool-lock"),
            "missing tool-lock must block persisted batch authority: {err}"
        );
        assert_eq!(
            fs::read(&task_path).unwrap(),
            before,
            "batch command {args:?} mutated tasks despite missing tool-lock"
        );
    }
}

#[test]
fn cand_059_human_log_escapes_controls_while_json_preserves_message() {
    let temp = canonical_tempdir();
    let xdg = temp.path().join("xdg");
    let scope = temp.path().join("scope");
    fs::create_dir_all(&scope).unwrap();
    fs::write(scope.join("note.md"), "plain snapshot body\n").unwrap();
    init_scope(&scope, &xdg);

    let message = "safe\x1b]8;;https://example.invalid\x07label\x1b]8;;\x07\u{202e}";
    json_success(&scope, &xdg, &["snapshot", "create", "-m", message]);

    let human = kio_at(&scope, &xdg, &["log"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(!human.contains(&0x1b));
    assert!(!human.contains(&0x07));
    let human_text = String::from_utf8(human).unwrap();
    assert!(
        human_text.contains("\\x1b"),
        "human output must show escaped ESC"
    );
    assert!(
        human_text.contains("\\x07"),
        "human output must show escaped BEL"
    );
    assert!(
        human_text.contains("\\u{202e}"),
        "human output must show escaped bidi override"
    );

    let output = kio_at(&scope, &xdg, &["log", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    // JSON transport escapes control bytes, but parsing must recover the exact
    // logical message for machine consumers.
    assert!(!output.contains(&0x1b));
    assert!(!output.contains(&0x07));
    let log: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(log["commits"][0]["message"], message);
}
