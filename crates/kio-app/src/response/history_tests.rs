use super::history::{
    DiffResponse, InspectResponse, LogResponse, RawInspection, TagDeleteResponse, TagResponse,
    TagSetResponse,
};
use super::{CommandResponse, Completion, ResponseBody};
use kio_core::dag::{CommitObject, CommitStats, CommitType, TreeEntry, TreeObject};
use kio_core::scope::{DiffEntry, LogEntry};
use serde_json::{Value, json};
use std::path::PathBuf;

fn commit() -> CommitObject {
    CommitObject {
        commit_type: CommitType::Manual,
        created_at: "2026-10-10T00:00:00Z".to_owned(),
        message: "history fixture".to_owned(),
        object_type: "commit".to_owned(),
        parent: None,
        stats: CommitStats {
            files_added: 1,
            files_modified: 0,
            files_deleted: 0,
        },
        tool_lock_hash: "a".repeat(64),
        tree: "b".repeat(64),
        purged_raws: Vec::new(),
        restore_provenance: None,
    }
}

fn assert_projection(body: ResponseBody, legacy: Value) {
    let response = CommandResponse {
        body,
        completion: Completion::Complete,
    };
    assert_eq!(response.completion, Completion::Complete);
    let actual = serde_json::to_value(response.body).expect("project typed history response");
    assert_eq!(actual, legacy);
    assert_eq!(
        serde_json::to_vec(&actual).expect("typed response bytes"),
        serde_json::to_vec(&legacy).expect("legacy response bytes")
    );
}

#[test]
fn log_projection_preserves_empty_populated_and_truncated_history() {
    for entries in [
        Vec::new(),
        vec![LogEntry {
            commit_hash: "c".repeat(64),
            commit: commit(),
        }],
    ] {
        for truncated in [false, true] {
            let legacy = json!({ "commits": entries, "truncated": truncated });
            assert_projection(
                ResponseBody::Log(LogResponse {
                    commits: entries.clone(),
                    truncated,
                }),
                legacy,
            );
        }
    }
}

#[test]
fn diff_projection_preserves_empty_changes_and_domain_field_omissions() {
    for changes in [
        Vec::new(),
        vec![
            DiffEntry {
                path: Some(PathBuf::from("notes.md")),
                relative_path: "notes.md".to_owned(),
                change: "added".to_owned(),
                old_raw_hash: None,
                new_raw_hash: Some("d".repeat(64)),
            },
            DiffEntry {
                path: None,
                relative_path: "removed.md".to_owned(),
                change: "deleted".to_owned(),
                old_raw_hash: Some("e".repeat(64)),
                new_raw_hash: None,
            },
        ],
    ] {
        let legacy = json!({ "changes": changes });
        assert_projection(ResponseBody::Diff(DiffResponse { changes }), legacy);
    }
}

#[test]
fn inspect_projection_preserves_unwrapped_tree_commit_and_raw_objects() {
    let tree = TreeObject {
        chunking_config_hash: "f".repeat(64),
        entries: vec![TreeEntry {
            path: "notes.md".to_owned(),
            entry_type: "file".to_owned(),
            raw_hash: "d".repeat(64),
            normalize: None,
        }],
        object_type: "tree".to_owned(),
    };
    let legacy = serde_json::to_value(tree.clone()).expect("legacy tree");
    assert_projection(ResponseBody::Inspect(InspectResponse::Tree(tree)), legacy);

    let commit = commit();
    let legacy = serde_json::to_value(commit.clone()).expect("legacy commit");
    assert_eq!(legacy["parent"], Value::Null);
    assert_projection(
        ResponseBody::Inspect(InspectResponse::Commit(commit)),
        legacy,
    );

    let raw_hash = "d".repeat(64);
    let size_bytes = u64::MAX;
    let legacy = json!({
        "object_type": "raw",
        "raw_hash": raw_hash,
        "size_bytes": size_bytes,
    });
    assert_projection(
        ResponseBody::Inspect(InspectResponse::Raw(RawInspection {
            object_type: "raw".to_owned(),
            raw_hash,
            size_bytes,
        })),
        legacy,
    );
}

#[test]
fn tag_projection_preserves_set_and_delete_distinct_shapes() {
    let name = "release/日本語".to_owned();
    let commit_hash = "c".repeat(64);
    let path = PathBuf::from("scope/.kio/refs/tags/fixture");
    let legacy = json!({
        "tag": name,
        "commit_hash": commit_hash,
        "path": path,
    });
    assert!(
        !legacy
            .as_object()
            .expect("tag object")
            .contains_key("operation")
    );
    assert!(
        !legacy
            .as_object()
            .expect("tag object")
            .contains_key("status")
    );
    assert_projection(
        ResponseBody::Tag(TagResponse::Set(TagSetResponse {
            tag: name.clone(),
            commit_hash: commit_hash.clone(),
            path: path.clone(),
        })),
        legacy,
    );
    let legacy = json!({
        "operation": "tag_delete",
        "status": "deleted",
        "tag": name,
        "commit_hash": commit_hash,
        "path": path,
    });
    assert_projection(
        ResponseBody::Tag(TagResponse::Delete(TagDeleteResponse {
            operation: "tag_delete".to_owned(),
            status: "deleted".to_owned(),
            tag: name,
            commit_hash,
            path,
        })),
        legacy,
    );
}

#[test]
fn legacy_glue_preserves_noncomplete_outcomes_during_migration() {
    for (completion, _code) in [
        (Completion::Partial, 3),
        (Completion::Permanent, 4),
        (Completion::AuthRequired, 5),
        (Completion::BudgetBlocked, 6),
    ] {
        let response = CommandResponse {
            body: ResponseBody::Log(LogResponse {
                commits: Vec::new(),
                truncated: false,
            }),
            completion,
        };
        assert_eq!(response.completion, completion);
        assert_eq!(
            serde_json::to_value(response.body).expect("legacy completion projection"),
            json!({ "commits": [], "truncated": false })
        );
    }
}
