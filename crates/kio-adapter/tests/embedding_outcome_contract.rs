//! O1 observable contract: local computation declares an explicit unmetered
//! completion rather than making absence of a provider invoice ambiguous.
use kio_adapter::deterministic_embedding::DeterministicEmbeddingAdapter;
use kio_adapter::traits::EmbeddingAdapter;
use kio_adapter::types::{EmbeddingInputType, EmbeddingItem, EmbeddingRequest};

#[test]
fn deterministic_embedding_returns_explicit_completed_unmetered_members() {
    let response = DeterministicEmbeddingAdapter
        .embed(EmbeddingRequest {
            input_type: EmbeddingInputType::Query,
            items: vec![EmbeddingItem::text("member-a", "alpha")],
            idempotency_token: None,
        })
        .unwrap();
    let value = serde_json::to_value(response).unwrap();
    assert_eq!(value["kind"], "completed", "local execution is completed");
    assert_eq!(value["usage"]["kind"], "unmetered");
    assert_eq!(value["members"][0]["id"], "member-a");
    assert_eq!(value["members"][0]["result"]["kind"], "vector");
    assert_eq!(
        value["members"][0]["result"]["vector"]
            .as_array()
            .unwrap()
            .len(),
        768
    );
    assert!(value.get("remote_handle").is_none());
}

#[test]
fn deterministic_descriptor_declares_zero_external_effects_and_no_batch() {
    let adapter = DeterministicEmbeddingAdapter;
    let before = adapter.profile().tool_profile_hash;
    let descriptor = adapter.descriptor();
    assert!(!descriptor.effects.egress);
    assert!(!descriptor.effects.paid_authorization);
    assert!(descriptor.capability.text);
    assert!(
        descriptor
            .capability
            .image_mime_types
            .contains(&"image/png".to_owned())
    );
    assert!(descriptor.completion.sync);
    assert!(!descriptor.completion.batch);
    assert!(descriptor.pricing.is_empty());
    assert_eq!(before, adapter.profile().tool_profile_hash);
}

#[test]
fn embedding_usage_and_job_ids_are_explicit_serializable_contracts() {
    use kio_adapter::types::{
        AdapterUsage, EmbeddingLogicalJobId, EmbeddingOutcome, EmbeddingRemoteHandle,
        EmbeddingUsage,
    };
    let logical = EmbeddingLogicalJobId("local-job".to_owned());
    let remote = EmbeddingRemoteHandle("remote-operation".to_owned());
    assert_eq!(serde_json::to_value(logical).unwrap(), "local-job");
    let pending = EmbeddingOutcome::Pending {
        remote_handle: remote,
    };
    assert_eq!(
        serde_json::to_value(pending).unwrap(),
        serde_json::json!({
            "kind": "pending", "remote_handle": "remote-operation"
        })
    );
    let metered = EmbeddingUsage::Metered {
        usage: AdapterUsage::Usd { usd: 0.25 },
    };
    assert_eq!(
        serde_json::to_value(metered).unwrap(),
        serde_json::json!({
            "kind": "metered", "usage": { "usd": 0.25 }
        })
    );
    assert_eq!(
        serde_json::to_value(EmbeddingUsage::Estimated).unwrap()["kind"],
        "estimated"
    );
}
