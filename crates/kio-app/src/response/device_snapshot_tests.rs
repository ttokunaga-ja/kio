use super::device::{
    InitResponse, RetiredChild, RootRegisterAppliedResponse, RootRegisterPreviewResponse,
};
use super::snapshot::SnapshotCreateResponse;
use kio_core::dag::{CommitObject, CommitStats, CommitType};
use kio_core::scope::SnapshotOutcome;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;

fn assert_projection<T: Serialize>(payload: T, legacy: Value) {
    let actual = serde_json::to_value(payload).expect("project concrete response DTO");
    assert_eq!(actual, legacy);
    assert_eq!(
        serde_json::to_vec(&actual).expect("typed response bytes"),
        serde_json::to_vec(&legacy).expect("legacy response bytes")
    );
}

#[test]
fn init_projection_preserves_status_repair_list_and_native_paths() {
    for (status, repaired) in [
        ("initialized", Vec::new()),
        ("already initialized", Vec::new()),
        ("repaired", vec!["publication".to_owned()]),
    ] {
        let path = PathBuf::from("scope/日本語");
        let kio_path = path.join(".kio");
        let legacy = json!({
            "status": status,
            "repaired": repaired,
            "path": path,
            "kio_path": kio_path,
        });
        assert_projection(
            InitResponse {
                status: status.to_owned(),
                repaired,
                path,
                kio_path,
                independent: None,
            },
            legacy,
        );
    }
}

#[test]
fn root_preview_projection_preserves_nullable_operation_and_retired_children() {
    for (op, detaches_from_former_parent, populated) in [
        (None, false, false),
        (Some("registration-fixture"), true, true),
    ] {
        let path = PathBuf::from("scope/日本語");
        let scope_id = "root-scope".to_owned();
        let scopes_registered = if populated { 3_usize } else { 1_usize };
        let relative = PathBuf::from("child");
        let name = "旧子";
        let child_scope_id = "retired-child-scope".to_owned();
        let retired = if populated {
            vec![json!({
                "path": relative.join(name),
                "scope_id": child_scope_id,
                "reason": "directory_absent",
            })]
        } else {
            Vec::new()
        };
        let child_memberships_retired = retired.len();
        let effects = [
            "rebind management path and directory identity for enrolled subtree",
            "revoke device grants",
            "refresh registry registration as unindexed",
            "retire memberships of absent children",
            "detachment preserves descendant parent IDs and enrollment tokens but changes their root scope",
        ];
        let legacy = json!({
            "operation": "root_register",
            "operation_id": op,
            "path": path,
            "scope_id": scope_id,
            "preview": true,
            "requires_yes": true,
            "detaches_from_former_parent": detaches_from_former_parent,
            "scopes_registered": scopes_registered,
            "child_memberships_retired": child_memberships_retired,
            "retired_children": retired,
            "effects": effects,
        });
        let retired_children = if populated {
            vec![RetiredChild {
                path: relative.join(name),
                scope_id: child_scope_id,
                reason: "directory_absent".to_owned(),
            }]
        } else {
            Vec::new()
        };
        assert_projection(
            RootRegisterPreviewResponse {
                operation: "root_register".to_owned(),
                operation_id: op.map(str::to_owned),
                path,
                scope_id,
                preview: true,
                requires_yes: true,
                detaches_from_former_parent,
                scopes_registered,
                child_memberships_retired,
                retired_children,
                effects: effects.into_iter().map(str::to_owned).collect(),
            },
            legacy,
        );
    }
}

#[test]
fn root_applied_projection_preserves_generation_counts_and_distinct_shape() {
    for (registration_generation, scopes_registered, child_memberships_retired) in
        [(2_u64, 1_usize, 0_usize), (u64::MAX, 3_usize, 2_usize)]
    {
        let operation_id = "registration-fixture".to_owned();
        let scope_id = "root-scope".to_owned();
        let path = PathBuf::from("scope/日本語");
        let legacy = json!({
            "status": "registered",
            "operation_id": operation_id,
            "scope_id": scope_id,
            "path": path,
            "registration_generation": registration_generation,
            "scopes_registered": scopes_registered,
            "child_memberships_retired": child_memberships_retired,
            "grants_revoked": true,
        });
        assert_projection(
            RootRegisterAppliedResponse {
                status: "registered".to_owned(),
                operation_id,
                scope_id,
                path,
                registration_generation,
                scopes_registered,
                child_memberships_retired,
                grants_revoked: true,
            },
            legacy,
        );
    }
}

fn snapshot_outcome(noop: bool, parent: Option<String>) -> SnapshotOutcome {
    let stats = CommitStats {
        files_added: u64::from(!noop),
        files_modified: 0,
        files_deleted: 0,
    };
    SnapshotOutcome {
        noop,
        message: "snapshot 日本語".to_owned(),
        tree_hash: "a".repeat(64),
        commit_hash: (!noop).then(|| "b".repeat(64)),
        commit: (!noop).then(|| CommitObject {
            commit_type: CommitType::Manual,
            created_at: "2026-10-10T00:00:00Z".to_owned(),
            message: "snapshot 日本語".to_owned(),
            object_type: "commit".to_owned(),
            parent,
            stats: stats.clone(),
            tool_lock_hash: "c".repeat(64),
            tree: "a".repeat(64),
            purged_raws: Vec::new(),
            restore_provenance: None,
        }),
        stats,
    }
}

fn assert_snapshot_base(outcome: SnapshotOutcome) {
    let legacy = json!({
        "status": if outcome.noop { "noop" } else { "created" },
        "message": outcome.message,
        "tree_hash": outcome.tree_hash,
        "commit_hash": outcome.commit_hash,
        "commit": outcome.commit,
        "stats": {
            "files_added": outcome.stats.files_added,
            "files_modified": outcome.stats.files_modified,
            "files_deleted": outcome.stats.files_deleted,
        }
    });
    assert_projection(
        SnapshotCreateResponse {
            status: if outcome.noop { "noop" } else { "created" }.to_owned(),
            message: outcome.message,
            tree_hash: outcome.tree_hash,
            commit_hash: outcome.commit_hash,
            commit: outcome.commit,
            stats: outcome.stats,
        },
        legacy,
    );
}

#[test]
fn snapshot_created_base_projection_preserves_commit_domain_fields() {
    assert_snapshot_base(snapshot_outcome(false, None));
    assert_snapshot_base(snapshot_outcome(false, Some("d".repeat(64))));
}

#[test]
fn snapshot_noop_base_projection_preserves_null_commit_fields_and_zero_stats() {
    assert_snapshot_base(snapshot_outcome(true, None));
}
