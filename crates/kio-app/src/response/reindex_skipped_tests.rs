// Typed projection goldens preserve the existing command wire shapes.
use super::reindex::{HeadReindexResponse, HistoricalReindexResponse};
use super::skipped_units::{PublishedSkippedUnit, SkippedDisclosure, SkippedUnit};
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

fn raw_skip() -> SkippedUnit {
    SkippedUnit {
        raw_hash: "sha256:".to_owned() + &"a".repeat(64),
        path: "文書.md".to_owned(),
        r#gen: u64::MAX,
        reason: "KIO-E-STORE-IO-001".to_owned(),
    }
}

fn raw_skip_legacy() -> Value {
    json!({"raw_hash": "sha256:".to_owned() + &"a".repeat(64), "path": "文書.md", "gen": u64::MAX, "reason": "KIO-E-STORE-IO-001"})
}

#[test]
fn skipped_raw_projection_preserves_four_fields_without_published_disclosure() {
    // Restore keeps these raw rows; typing the internal report must not add guidance.
    assert_projection(raw_skip(), raw_skip_legacy());
}

#[test]
fn skipped_disclosure_projection_preserves_empty_searchable_and_unsearchable_guidance() {
    assert_projection(
        SkippedDisclosure {
            skipped_units: Vec::new(),
            skipped_units_guidance: None,
        },
        json!({"skipped_units": []}),
    );
    for searchable in [false, true] {
        let guidance = if searchable {
            "re-serving cached chunks; the normalized source is stale — \
             re-normalize with `kio reindex --force` when convenient"
        } else {
            "no live chunks remain for this document; run `kio reindex --force` \
             to re-normalize it from the raw objects"
        };
        let top_guidance = if searchable {
            "some documents' normalized sources are stale but their cached chunks are \
             still served; re-normalize with `kio reindex --force` when convenient"
        } else {
            "some documents' normalized units are missing or corrupt and have no live \
             chunks; run `kio reindex --force` to re-normalize them from the raw objects"
        };
        let mut entry = raw_skip_legacy();
        entry["searchable"] = json!(searchable);
        entry["guidance"] = json!(guidance);
        assert_projection(
            SkippedDisclosure {
                skipped_units: vec![PublishedSkippedUnit {
                    unit: raw_skip(),
                    searchable,
                    guidance: guidance.to_owned(),
                }],
                skipped_units_guidance: Some(top_guidance.to_owned()),
            },
            json!({"skipped_units": [entry], "skipped_units_guidance": top_guidance}),
        );
    }
}

#[test]
fn reindex_head_projection_preserves_nullable_commit_counts_and_empty_skips() {
    // Same body for enrichment success/auth/budget/permanent-counter outcomes: the completion is external.
    for (executed, failed, paused) in [(0_usize, 0_usize, 0_usize), (1, 1, 0), (1, 0, 2), (0, 2, 0)]
    {
        for commit_hash in [None, Some("b".repeat(64))] {
            let legacy = json!({
                "status": "reindexed", "reindexed_files": u64::MAX, "commit_hash": commit_hash, "rebuilt_chunks": u64::MAX,
                "embedding_tasks_executed": executed, "embedding_tasks_failed": failed, "paused_tasks": paused, "skipped_units": [],
            });
            assert_projection(
                HeadReindexResponse {
                    status: "reindexed".to_owned(),
                    reindexed_files: u64::MAX,
                    commit_hash,
                    rebuilt_chunks: u64::MAX,
                    embedding_tasks_executed: executed,
                    embedding_tasks_failed: failed,
                    paused_tasks: paused,
                    skipped: SkippedDisclosure {
                        skipped_units: Vec::new(),
                        skipped_units_guidance: None,
                    },
                },
                legacy,
            );
        }
    }
}

#[test]
fn reindex_historical_projection_preserves_selected_snapshot_and_distinct_skip_shape() {
    let guidance = "no live chunks remain for this document; run `kio reindex --force` to re-normalize it from the raw objects";
    let top_guidance = "some documents' normalized units are missing or corrupt and have no live chunks; run `kio reindex --force` to re-normalize them from the raw objects";
    let mut skip = raw_skip_legacy();
    skip["searchable"] = json!(false);
    skip["guidance"] = json!(guidance);
    assert_projection(
        HistoricalReindexResponse {
            status: "reindexed".to_owned(),
            snapshot_at: "a".repeat(64),
            head_commit: "b".repeat(64),
            reindexed_files: u64::MAX,
            rebuilt_chunks: u64::MAX,
            embedding_tasks_executed: 1,
            embedding_tasks_failed: 2,
            paused_tasks: 3,
            blocked_raw_hashes: 4,
            skipped: SkippedDisclosure {
                skipped_units: vec![PublishedSkippedUnit {
                    unit: raw_skip(),
                    searchable: false,
                    guidance: guidance.to_owned(),
                }],
                skipped_units_guidance: Some(top_guidance.to_owned()),
            },
        },
        json!({
            "status": "reindexed", "snapshot_at": "a".repeat(64), "head_commit": "b".repeat(64), "reindexed_files": u64::MAX, "rebuilt_chunks": u64::MAX,
            "embedding_tasks_executed": 1_usize, "embedding_tasks_failed": 2_usize, "paused_tasks": 3_usize, "blocked_raw_hashes": 4_usize,
            "skipped_units": [skip], "skipped_units_guidance": top_guidance,
        }),
    );
}
