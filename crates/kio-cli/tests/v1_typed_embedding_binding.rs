mod support;

use std::fs;
use std::process::{Command, Output};

use kio_core::cas::hash_bytes;
use kio_pipeline::task::{EmbeddingTaskTarget, TaskStore, TaskType};
use tempfile::TempDir;

fn run(root: &TempDir, args: &[&str]) -> Output {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("kio"));
    command.env_clear();
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command
        .current_dir(root.path())
        .env("HOME", root.path())
        .env("USERPROFILE", root.path())
        .env("XDG_DATA_HOME", root.path().join(".test-data"))
        .env("XDG_CONFIG_HOME", root.path().join(".test-config"))
        .env("XDG_CACHE_HOME", root.path().join(".test-cache"))
        .env("KIO_TEST_GEMINI_EMBED", "require_idempotency_token")
        .env("KIO_FIXED_NOW", "2026-09-08T00:00:00Z")
        .args(args)
        .arg("--json")
        .output()
        .unwrap()
}

#[test]
fn forged_typed_embedding_profile_or_contextual_input_cannot_reserve_send_or_publish() {
    for field in ["profile", "input"] {
        let root = support::canonical_tempdir();
        fs::write(
            root.path().join("Contextual Name.md"),
            "# Binding\n\nfixed body\n",
        )
        .unwrap();
        for args in [
            vec!["init"],
            vec!["ledger", "init"],
            vec!["index", "--offline"],
            vec!["adapter", "approve", "gemini_embedding_2", "--yes"],
        ] {
            let output = run(&root, &args);
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let store = TaskStore::new(root.path().join(".kio"));
        let mut tasks = store.all().unwrap();
        let task = tasks
            .iter_mut()
            .find(|task| task.task_type == TaskType::Embedding)
            .unwrap();
        let Some(EmbeddingTaskTarget::Chunk {
            profile_hash,
            input_hash,
            ..
        }) = &mut task.embedding_target
        else {
            panic!("the fixture needs its current typed chunk task");
        };
        match field {
            "profile" => *profile_hash = hash_bytes(b"another profile"),
            "input" => *input_hash = hash_bytes(b"another contextual input"),
            _ => unreachable!(),
        }
        store.replace_all(&tasks).unwrap();
        let journal_before = fs::read(root.path().join(".kio/tasks.jsonl")).unwrap();
        let output = run(&root, &["batch", "resume", "--online", "--realtime"]);
        let ledger =
            support::immutable_sqlite(root.path().join(".test-data/kio/cost-ledger.sqlite"));
        let requests: i64 = ledger
            .query_row(
                "SELECT COUNT(*) FROM batch_requests WHERE adapter_kind = 'embedding'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            requests,
            0,
            "forged {field} must be refused before reservation/send: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let source = support::immutable_sqlite(root.path().join(".kio/index/sqlite.db"));
        let vectors: i64 = source
            .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(vectors, 0);
        assert_eq!(
            fs::read(root.path().join(".kio/tasks.jsonl")).unwrap(),
            journal_before
        );
        assert!(!output.status.success());
    }
}
