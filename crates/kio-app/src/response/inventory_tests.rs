// Typed projection goldens preserve the existing command wire shapes.
use kio_core::gc::{
    InventoryObject, InventoryStats, ShallowBoundary, UnreachableInventoryLimits,
    UnreachableInventoryPassStats, UnreachableInventoryReport, UnreachableInventorySummary,
};
use serde::Serialize;
use serde_json::{Value, json};

fn assert_projection<T: Serialize>(payload: T, legacy: Value) {
    let actual = serde_json::to_value(payload).expect("project concrete response DTO");
    assert_eq!(actual, legacy);
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        serde_json::to_vec(&legacy).unwrap()
    );
}

#[test]
fn inventory_projection_preserves_empty_read_only_root_and_both_stat_passes() {
    let limits = UnreachableInventoryLimits::default();
    let stats = InventoryStats::default();
    assert_projection(
        UnreachableInventoryReport {
            schema_version: 1,
            operation: "unreachable_object_inventory".to_owned(),
            status: "dry_run".to_owned(),
            read_only: true,
            diagnostic_only: true,
            mutation_authority: false,
            objects: Vec::new(),
            summary: UnreachableInventorySummary {
                object_count: 0,
                physical_bytes: 0,
                candidate_count: 0,
                candidate_bytes: 0,
                protected_count: 0,
                protected_bytes: 0,
                inventory_only_count: 0,
                inventory_only_bytes: 0,
            },
            shallow_boundaries: Vec::new(),
            limits: limits.clone(),
            stats: UnreachableInventoryPassStats {
                inventory_pass: stats.clone(),
                stability_pass: stats.clone(),
            },
        },
        json!({
            "schema_version": 1, "operation": "unreachable_object_inventory", "status": "dry_run", "read_only": true,
            "diagnostic_only": true, "mutation_authority": false, "objects": [],
            "summary": {"object_count": 0_u64, "physical_bytes": 0_u64, "candidate_count": 0_u64, "candidate_bytes": 0_u64,
                "protected_count": 0_u64, "protected_bytes": 0_u64, "inventory_only_count": 0_u64, "inventory_only_bytes": 0_u64},
            "shallow_boundaries": [], "limits": limits,
            "stats": {"inventory_pass": stats, "stability_pass": stats},
        }),
    );
}

#[test]
fn inventory_projection_preserves_classification_rows_shallow_boundaries_and_u64_statistics() {
    let objects = vec![
        InventoryObject {
            kind: "manifest".to_owned(),
            hash: "sha256:".to_owned() + &"a".repeat(64),
            physical_bytes: u64::MAX - 3,
            classification: "candidate".to_owned(),
            reason: "zero_tree_references".to_owned(),
        },
        InventoryObject {
            kind: "raw".to_owned(),
            hash: "sha256:".to_owned() + &"b".repeat(64),
            physical_bytes: 1,
            classification: "protected".to_owned(),
            reason: "evidence_pointer_permanence".to_owned(),
        },
        InventoryObject {
            kind: "image".to_owned(),
            hash: "sha256:".to_owned() + &"c".repeat(64),
            physical_bytes: 2,
            classification: "inventory_only".to_owned(),
            reason: "verify_objects_orphan_lifecycle".to_owned(),
        },
    ];
    let boundary = ShallowBoundary {
        commit_hash: "sha256:".to_owned() + &"d".repeat(64),
        tree_hash: "sha256:".to_owned() + &"e".repeat(64),
    };
    let stats = InventoryStats {
        objects: 3,
        physical_bytes: u64::MAX,
        verified_bytes: u64::MAX,
        refs: 1,
        receipts: 2,
        manifest_units: 3,
        history_steps: 4,
        directory_entries: 5,
    };
    let limits = UnreachableInventoryLimits {
        max_physical_bytes: u64::MAX,
        max_verified_bytes: u64::MAX,
        ..UnreachableInventoryLimits::default()
    };
    let legacy = json!({
        "schema_version": 1, "operation": "unreachable_object_inventory", "status": "dry_run", "read_only": true,
        "diagnostic_only": true, "mutation_authority": false, "objects": [
            {"kind": "manifest", "hash": "sha256:".to_owned() + &"a".repeat(64), "physical_bytes": u64::MAX - 3,
                "classification": "candidate", "reason": "zero_tree_references"},
            {"kind": "raw", "hash": "sha256:".to_owned() + &"b".repeat(64), "physical_bytes": 1_u64,
                "classification": "protected", "reason": "evidence_pointer_permanence"},
            {"kind": "image", "hash": "sha256:".to_owned() + &"c".repeat(64), "physical_bytes": 2_u64,
                "classification": "inventory_only", "reason": "verify_objects_orphan_lifecycle"},
        ],
        "summary": {"object_count": stats.objects, "physical_bytes": stats.physical_bytes,
            "candidate_count": 1_u64, "candidate_bytes": u64::MAX - 3, "protected_count": 1_u64, "protected_bytes": 1_u64,
            "inventory_only_count": 1_u64, "inventory_only_bytes": 2_u64},
        "shallow_boundaries": [{"commit_hash": "sha256:".to_owned() + &"d".repeat(64), "tree_hash": "sha256:".to_owned() + &"e".repeat(64)}], "limits": limits,
        "stats": {"inventory_pass": stats, "stability_pass": InventoryStats::default()},
    });
    assert_projection(
        UnreachableInventoryReport {
            schema_version: 1,
            operation: "unreachable_object_inventory".to_owned(),
            status: "dry_run".to_owned(),
            read_only: true,
            diagnostic_only: true,
            mutation_authority: false,
            objects,
            summary: UnreachableInventorySummary {
                object_count: 3,
                physical_bytes: u64::MAX,
                candidate_count: 1,
                candidate_bytes: u64::MAX - 3,
                protected_count: 1,
                protected_bytes: 1,
                inventory_only_count: 1,
                inventory_only_bytes: 2,
            },
            shallow_boundaries: vec![boundary],
            limits,
            stats: UnreachableInventoryPassStats {
                inventory_pass: stats,
                stability_pass: InventoryStats::default(),
            },
        },
        legacy,
    );
}
