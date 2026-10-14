// DTO-direct Search projection goldens.
use super::search::{
    AggregatorResponse, ChunkSearchHit, DiversifySummary, ExcludedScopeResponse,
    FileDispositionResponse, ImageSearchHit, IndexStatusError, IndexStatusResponse,
    OfficeConversionUnavailableResponse, PagingResponse, SearchGcRecoveryPending, SearchHit,
    SearchResponse, SearchedScopeResponse,
};
use kio_search::evidence::{
    EvidencePointer, EvidencePointerIssueRequest, evidence_pointer_to_uri, issue_evidence_pointer,
};
use kio_search::object_uri::{RelatedImage, parse_object_uri};
use kio_search::query::{DiversifyStrategy, SearchMode};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;

fn assert_projection<T: Serialize>(payload: T, legacy: Value) -> Value {
    let actual = serde_json::to_value(payload).expect("project concrete Search DTO");
    assert_eq!(actual, legacy);
    assert_eq!(
        serde_json::to_vec(&actual).expect("typed Search DTO bytes"),
        serde_json::to_vec(&legacy).expect("legacy Search builder bytes")
    );
    actual
}

fn hash(digit: char) -> String {
    format!("sha256:{}", digit.to_string().repeat(64))
}

fn pointer() -> EvidencePointer {
    issue_evidence_pointer(EvidencePointerIssueRequest {
        scope_id: "scope-fixture".to_owned(),
        scope_path: Some("scope/日本語/.kio".to_owned()),
        commit: hash('a'),
        tree: None,
        raw_hash: hash('b'),
        tool_profile_hash: hash('c'),
        chunk_hash: hash('d'),
        path_at_commit: Some("日本語.md".to_owned()),
        heading_path: Some(vec!["見出し".to_owned()]),
        section_id: None,
        byte_start: Some(0),
        byte_end: Some(12),
    })
    .expect("valid evidence pointer fixture")
}

fn image_uri() -> String {
    let uri = format!("kio://scope-fixture/object/image/{}", hash('e'));
    parse_object_uri(&uri).expect("valid image object URI fixture");
    uri
}

fn empty_index_status() -> IndexStatusResponse {
    IndexStatusResponse {
        enriched_ratio: 1.0,
        pending_enrichment_tasks: 0,
        budget_paused: false,
        unsupported_inputs: Vec::new(),
        unsupported_input_errors: Vec::new(),
        unsupported_inputs_complete: true,
        task_errors: Vec::new(),
        tasks_complete: true,
        office_conversion_unavailable: OfficeConversionUnavailableResponse {
            count: 0,
            files: Vec::new(),
        },
    }
}

fn legacy_empty_index_status() -> Value {
    json!({
        "enriched_ratio": 1.0,
        "pending_enrichment_tasks": 0,
        "budget_paused": false,
        "unsupported_inputs": [],
        "unsupported_input_errors": [],
        "unsupported_inputs_complete": true,
        "task_errors": [],
        "tasks_complete": true,
        "office_conversion_unavailable": {"count": 0, "files": []},
    })
}

fn empty_search() -> SearchResponse {
    SearchResponse {
        query: "fixture 日本語".to_owned(),
        requested_mode: SearchMode::Auto,
        resolved_mode: SearchMode::Hybrid,
        fallback: false,
        fallback_reason: None,
        error_code: None,
        warnings: Vec::new(),
        diversify: DiversifySummary {
            strategy: DiversifyStrategy::Off,
            mmr_lambda: None,
        },
        aggregator: AggregatorResponse {
            collection_generation: "collection-fixture".to_owned(),
        },
        paging: PagingResponse {
            limit: 5,
            next_cursor: None,
        },
        searched_scopes: Vec::new(),
        excluded_scopes: Vec::new(),
        index_status: empty_index_status(),
        results: Vec::new(),
        gc_recovery_pending: None,
    }
}

fn legacy_empty_search() -> Value {
    json!({
        "query": "fixture 日本語",
        "requested_mode": "auto",
        "resolved_mode": "hybrid",
        "fallback": false,
        "fallback_reason": null,
        "error_code": null,
        "warnings": [],
        "diversify": {"strategy": "off"},
        "aggregator": {"collection_generation": "collection-fixture"},
        "paging": {"limit": 5, "next_cursor": null},
        "searched_scopes": [],
        "excluded_scopes": [],
        "index_status": legacy_empty_index_status(),
        "results": [],
    })
}

fn chunk_hit() -> ChunkSearchHit {
    let evidence_pointer = pointer();
    let evidence_uri = evidence_pointer_to_uri(&evidence_pointer).expect("pointer URI");
    ChunkSearchHit {
        result_type: "chunk".to_owned(),
        chunk_hash: hash('d'),
        evidence_pointer,
        evidence_uri,
        score: 0.125,
        scope_path: PathBuf::from("scope/日本語/.kio"),
        title: "日本語.md".to_owned(),
        snippet: "日本語の本文\nsecond line".to_owned(),
        related_images: Vec::new(),
        current_paths: None,
        current_path: None,
    }
}

fn legacy_chunk_hit() -> Value {
    let evidence_pointer = pointer();
    let evidence_uri = evidence_pointer_to_uri(&evidence_pointer).expect("pointer URI");
    json!({
        "result_type": "chunk",
        "chunk_hash": hash('d'),
        "evidence_pointer": evidence_pointer,
        "evidence_uri": evidence_uri,
        "score": 0.125,
        "scope_path": PathBuf::from("scope/日本語/.kio"),
        "title": "日本語.md",
        "snippet": "日本語の本文\nsecond line",
    })
}

fn image_hit() -> ImageSearchHit {
    let evidence_pointer = pointer();
    let evidence_uri = evidence_pointer_to_uri(&evidence_pointer).expect("pointer URI");
    ImageSearchHit {
        result_type: "image".to_owned(),
        chunk_hash: hash('d'),
        evidence_pointer,
        evidence_uri,
        score: 1.0 / 3.0,
        scope_path: PathBuf::from("scope/日本語/.kio"),
        title: "日本語.md".to_owned(),
        payload_uri: image_uri(),
        current_paths: None,
        current_path: None,
    }
}

fn legacy_image_hit() -> Value {
    let evidence_pointer = pointer();
    let evidence_uri = evidence_pointer_to_uri(&evidence_pointer).expect("pointer URI");
    json!({
        "result_type": "image",
        "chunk_hash": hash('d'),
        "evidence_pointer": evidence_pointer,
        "evidence_uri": evidence_uri,
        "score": 1.0 / 3.0,
        "scope_path": PathBuf::from("scope/日本語/.kio"),
        "title": "日本語.md",
        "payload_uri": image_uri(),
    })
}

#[test]
fn search_root_projection_preserves_arrays_nulls_modes_and_fallback_fields() {
    let actual = assert_projection(empty_search(), legacy_empty_search());
    assert_eq!(actual["requested_mode"], "auto");
    assert_eq!(actual["fallback_reason"], Value::Null);
    assert_eq!(actual["error_code"], Value::Null);
    assert_eq!(actual["paging"]["next_cursor"], Value::Null);
    assert!(
        !actual
            .as_object()
            .unwrap()
            .contains_key("gc_recovery_pending")
    );

    for (requested, resolved, requested_wire, resolved_wire) in [
        (SearchMode::Auto, SearchMode::Hybrid, "auto", "hybrid"),
        (SearchMode::Text, SearchMode::Text, "text", "text"),
        (SearchMode::Vector, SearchMode::Vector, "vector", "vector"),
        (SearchMode::Hybrid, SearchMode::Hybrid, "hybrid", "hybrid"),
    ] {
        let mut payload = empty_search();
        payload.requested_mode = requested;
        payload.resolved_mode = resolved;
        let mut legacy = legacy_empty_search();
        legacy["requested_mode"] = json!(requested_wire);
        legacy["resolved_mode"] = json!(resolved_wire);
        assert_projection(payload, legacy);
    }
    let mut payload = empty_search();
    payload.requested_mode = SearchMode::Hybrid;
    payload.resolved_mode = SearchMode::Text;
    payload.fallback = true;
    payload.fallback_reason = Some("query_embedding_unavailable".to_owned());
    payload.error_code = Some("KIO-E-SEARCH-VEC-UNAVAIL-001".to_owned());
    payload.warnings = vec![
        "vector search unavailable (query_embedding_unavailable); fell back to text".to_owned(),
    ];
    let mut legacy = legacy_empty_search();
    legacy["requested_mode"] = json!("hybrid");
    legacy["resolved_mode"] = json!("text");
    legacy["fallback"] = json!(true);
    legacy["fallback_reason"] = json!("query_embedding_unavailable");
    legacy["error_code"] = json!("KIO-E-SEARCH-VEC-UNAVAIL-001");
    legacy["warnings"] =
        json!(["vector search unavailable (query_embedding_unavailable); fell back to text"]);
    assert_projection(payload, legacy);
}

#[test]
fn chunk_projection_preserves_snippet_and_omits_empty_related_images() {
    for related_images in [
        Vec::new(),
        vec![RelatedImage {
            image_uri: image_uri(),
            order: 0_usize,
        }],
    ] {
        let mut legacy = legacy_chunk_hit();
        if !related_images.is_empty() {
            legacy["related_images"] = json!(related_images);
        }
        let mut payload = chunk_hit();
        payload.related_images = related_images;
        let actual = assert_projection(SearchHit::Chunk(payload), legacy);
        assert!(!actual.as_object().unwrap().contains_key("payload_uri"));
        assert!(
            !actual["evidence_pointer"]
                .as_object()
                .unwrap()
                .contains_key("tree")
        );
        assert!(
            !actual["evidence_pointer"]
                .as_object()
                .unwrap()
                .contains_key("section_id")
        );
    }
}

#[test]
fn image_projection_preserves_citing_chunk_and_image_only_payload_uri() {
    let actual = assert_projection(SearchHit::Image(image_hit()), legacy_image_hit());
    let object = actual.as_object().unwrap();
    assert!(!object.contains_key("snippet"));
    assert!(!object.contains_key("related_images"));
    assert!(!object.contains_key("image_hash"));
    assert_eq!(
        actual["chunk_hash"],
        actual["evidence_pointer"]["chunk_hash"]
    );
}

#[test]
fn hit_projection_preserves_zero_one_and_multiple_current_path_shapes() {
    for paths in [
        Vec::new(),
        vec!["current.md".to_owned()],
        vec!["first.md".to_owned(), "second.md".to_owned()],
    ] {
        let current_paths = (!paths.is_empty()).then(|| paths.clone());
        let current_path = (paths.len() == 1).then(|| paths[0].clone());
        let mut chunk = chunk_hit();
        chunk.current_paths = current_paths.clone();
        chunk.current_path = current_path.clone();
        let mut chunk_legacy = legacy_chunk_hit();
        let mut image = image_hit();
        image.current_paths = current_paths;
        image.current_path = current_path.clone();
        let mut image_legacy = legacy_image_hit();
        if !paths.is_empty() {
            chunk_legacy["current_paths"] = json!(paths);
            image_legacy["current_paths"] = json!(paths);
            if let Some(path) = current_path {
                chunk_legacy["current_path"] = json!(path);
                image_legacy["current_path"] = json!(path);
            }
        }
        assert_projection(SearchHit::Chunk(chunk), chunk_legacy);
        assert_projection(SearchHit::Image(image), image_legacy);
    }
}

#[test]
fn scope_projection_preserves_shallow_omission_and_asymmetric_exclusions() {
    for shallow_skipped in [0_u64, u64::MAX] {
        let scope_id = "scope-fixture".to_owned();
        let scope_path = PathBuf::from("scope/日本語/.kio");
        let snapshot_at = hash('a');
        let mut legacy = json!({
            "scope_id": scope_id,
            "scope_path": scope_path,
            "snapshot_at": snapshot_at,
        });
        if shallow_skipped > 0 {
            legacy["shallow_skipped"] = json!(shallow_skipped);
        }
        assert_projection(
            SearchedScopeResponse {
                scope_id,
                scope_path,
                snapshot_at,
                shallow_skipped,
            },
            legacy,
        );
    }
    let candidates = vec!["first/.kio".to_owned(), "second/.kio".to_owned()];
    assert_projection(
        ExcludedScopeResponse {
            scope_id: "duplicate-scope".to_owned(),
            scope_path: None,
            reason: "registry_duplicate".to_owned(),
            candidates: Some(candidates.clone()),
            recovery: None,
            error_code: None,
        },
        json!({
            "scope_id": "duplicate-scope",
            "scope_path": null,
            "reason": "registry_duplicate",
            "candidates": candidates,
        }),
    );
    for hinted in [false, true] {
        let recovery = "store_corrupt: try `kio repair all`; if it still fails, restore the corrupt commit/tree object from objects/refs";
        let scope_path = PathBuf::from("scope/日本語/.kio");
        let mut legacy = json!({
            "scope_id": "scope-fixture",
            "scope_path": scope_path,
            "reason": "store_corrupt",
        });
        if hinted {
            legacy["recovery"] = json!(recovery);
            legacy["error_code"] = json!("KIO-E-STORE-CORRUPT-001");
        }
        assert_projection(
            ExcludedScopeResponse {
                scope_id: "scope-fixture".to_owned(),
                scope_path: Some(scope_path),
                reason: "store_corrupt".to_owned(),
                candidates: None,
                recovery: hinted.then(|| recovery.to_owned()),
                error_code: hinted.then(|| "KIO-E-STORE-CORRUPT-001".to_owned()),
            },
            legacy,
        );
    }
}

#[test]
fn diversify_projection_preserves_all_three_strategy_objects_and_f64() {
    assert_projection(
        DiversifySummary {
            strategy: DiversifyStrategy::Off,
            mmr_lambda: None,
        },
        json!({"strategy": "off"}),
    );
    for mmr_lambda in [0.0, 0.7, 1.0] {
        assert_projection(
            DiversifySummary {
                strategy: DiversifyStrategy::Mmr,
                mmr_lambda: Some(mmr_lambda),
            },
            json!({"strategy": "mmr", "mmr_lambda": mmr_lambda}),
        );
    }
    assert_projection(
        DiversifySummary {
            strategy: DiversifyStrategy::GroupByRawHash,
            mmr_lambda: None,
        },
        json!({"strategy": "group_by_raw_hash"}),
    );
}

#[test]
fn index_status_projection_preserves_empty_and_populated_finite_records() {
    assert_projection(empty_index_status(), legacy_empty_index_status());
    for enriched_ratio in [0.0, 1.0 / 3.0, 1.0] {
        let scope_path = PathBuf::from("scope/日本語/.kio");
        let reason = "unrecognized_binary_without_local_text";
        let office_reason = "office_conversion_unavailable";
        let make_file = |reason: &str| FileDispositionResponse {
            scope_path: scope_path.clone(),
            path: "日本語.bin".to_owned(),
            raw_hash: hash('b'),
            media_type: "application/octet-stream".to_owned(),
            size_bytes: u64::MAX,
            reason: reason.to_owned(),
        };
        let make_error = || IndexStatusError {
            scope_path: scope_path.clone(),
            error_code: "KIO-E-STORE-CORRUPT-001".to_owned(),
            message: "scope fixture failure".to_owned(),
        };
        let file_legacy = |reason: &str| {
            json!({
                "scope_path": scope_path,
                "path": "日本語.bin",
                "raw_hash": hash('b'),
                "media_type": "application/octet-stream",
                "size_bytes": u64::MAX,
                "reason": reason,
            })
        };
        let error_legacy = json!({
            "scope_path": scope_path,
            "error_code": "KIO-E-STORE-CORRUPT-001",
            "message": "scope fixture failure",
        });
        assert_projection(
            IndexStatusResponse {
                enriched_ratio,
                pending_enrichment_tasks: u64::MAX,
                budget_paused: true,
                unsupported_inputs: vec![make_file(reason)],
                unsupported_input_errors: vec![make_error()],
                unsupported_inputs_complete: false,
                task_errors: vec![make_error()],
                tasks_complete: false,
                office_conversion_unavailable: OfficeConversionUnavailableResponse {
                    count: 1_usize,
                    files: vec![make_file(office_reason)],
                },
            },
            json!({
                "enriched_ratio": enriched_ratio,
                "pending_enrichment_tasks": u64::MAX,
                "budget_paused": true,
                "unsupported_inputs": [file_legacy(reason)],
                "unsupported_input_errors": [error_legacy],
                "unsupported_inputs_complete": false,
                "task_errors": [error_legacy],
                "tasks_complete": false,
                "office_conversion_unavailable": {
                    "count": 1_usize,
                    "files": [file_legacy(office_reason)],
                },
            }),
        );
    }
}

#[test]
fn search_gc_hint_projection_preserves_cursor_null_and_finite_optional_leaf() {
    for pending in [false, true] {
        let mut payload = empty_search();
        payload.paging.limit = u64::MAX;
        let mut legacy = legacy_empty_search();
        legacy["paging"]["limit"] = json!(u64::MAX);
        if pending {
            payload.paging.next_cursor = None;
            payload.gc_recovery_pending = Some(SearchGcRecoveryPending {
                reason: "gc_sweep_active".to_owned(),
                next_cursor_suppressed: true,
            });
            legacy["paging"]["next_cursor"] = Value::Null;
            legacy["gc_recovery_pending"] = json!({
                "reason": "gc_sweep_active",
                "next_cursor_suppressed": true,
            });
        } else {
            payload.paging.next_cursor = Some("cursor-fixture".to_owned());
            legacy["paging"]["next_cursor"] = json!("cursor-fixture");
        }
        let actual = assert_projection(payload, legacy);
        assert_eq!(
            actual
                .as_object()
                .unwrap()
                .contains_key("gc_recovery_pending"),
            pending
        );
    }
}
