//! Current embedding record contract, docs/03 §8.1 and docs/07 §2/5.3.
use super::*;

fn identity() -> Value {
    EmbeddingObject::identity_value(
        "chunk",
        &hash_bytes(b"text"),
        &hash_bytes(b"profile"),
        "multimodal",
        2,
        "cosine",
        None,
    )
}

fn metadata() -> Value {
    serde_json::json!({
        "record_format": 2,
        "producer": {
            "execution_mode": "online_api",
            "os": "linux",
            "arch": "x86_64",
            "runtime_version": null,
            "runtime_library_sha256": null,
            "profile_hash": hash_bytes(b"profile"),
            "cpu_backend": null,
            "threads": null
        },
        "image_input_receipt": null
    })
}

fn record(header: &Value, vector: &[f32], metadata: &Value) -> Vec<u8> {
    let vector = vector_to_le_bytes(vector);
    let metadata = canonical_json_bytes(metadata).unwrap();
    format!(
        "{}\n{}\n{}\n{}\n{}\n",
        String::from_utf8(canonical_json_bytes(header).unwrap()).unwrap(),
        base64_encode(&vector),
        lower_hex(&Sha256::digest(&vector)),
        String::from_utf8(metadata.clone()).unwrap(),
        lower_hex(&Sha256::digest(&metadata)),
    )
    .into_bytes()
}

#[test]
fn current_embedding_record_round_trips_five_canonical_lf_lines_with_provenance() {
    let bytes = record(&identity(), &[1.0, 0.0], &metadata());
    let parsed = EmbeddingObject::from_bytes(&bytes).expect("current five-line record must parse");
    assert_eq!(parsed.to_bytes().unwrap(), bytes);
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 5);
    let lines = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 5);
    let saved: Value = serde_json::from_str(lines[3]).unwrap();
    assert_eq!(saved, metadata());
    assert_eq!(
        saved["producer"]["profile_hash"],
        identity()["profile_hash"]
    );
    assert!(saved["image_input_receipt"].is_null());
}

#[test]
fn current_embedding_record_rejects_old_three_line_bytes_without_provenance() {
    let bytes = record(&identity(), &[1.0, 0.0], &metadata());
    let old = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .take(3)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        EmbeddingObject::from_bytes(old.as_bytes()).is_err(),
        "old records must fail closed"
    );
    assert!(EmbeddingObject::from_bytes(format!("{old}\n").as_bytes()).is_err());
}

#[test]
fn current_embedding_record_rejects_unknown_missing_or_unbound_metadata() {
    let mut cases = Vec::new();
    let mut unknown = metadata();
    unknown["unknown"] = Value::Bool(true);
    cases.push(unknown);
    let mut producer_unknown = metadata();
    producer_unknown["producer"]["unknown"] = Value::Bool(true);
    cases.push(producer_unknown);
    for key in [
        "runtime_version",
        "runtime_library_sha256",
        "cpu_backend",
        "threads",
    ] {
        let mut missing = metadata();
        missing["producer"].as_object_mut().unwrap().remove(key);
        cases.push(missing);
    }
    let mut missing_receipt = metadata();
    missing_receipt
        .as_object_mut()
        .unwrap()
        .remove("image_input_receipt");
    cases.push(missing_receipt);
    let mut wrong_profile = metadata();
    wrong_profile["producer"]["profile_hash"] = Value::String(hash_bytes(b"different"));
    cases.push(wrong_profile);
    let mut wrong_mode = metadata();
    wrong_mode["producer"]["execution_mode"] = Value::String("unknown".to_owned());
    cases.push(wrong_mode);
    let mut fabricated_native = metadata();
    fabricated_native["producer"]["runtime_version"] = Value::String("0.18.0".to_owned());
    cases.push(fabricated_native);
    let mut incomplete_native = metadata();
    incomplete_native["producer"]["execution_mode"] = Value::String("native_library".to_owned());
    cases.push(incomplete_native);
    let mut unauthenticated_native = metadata();
    unauthenticated_native["producer"]["execution_mode"] =
        Value::String("native_library".to_owned());
    unauthenticated_native["producer"]["runtime_version"] = Value::String("0.18.0".to_owned());
    unauthenticated_native["producer"]["runtime_library_sha256"] =
        Value::String(hash_bytes(b"library"));
    unauthenticated_native["producer"]["cpu_backend"] = Value::String("cpu".to_owned());
    unauthenticated_native["producer"]["threads"] = Value::from(2);
    cases.push(unauthenticated_native);
    let mut unbound_receipt = metadata();
    unbound_receipt["image_input_receipt"] = serde_json::json!({
        "source_hash": hash_bytes(b"source"), "source_mime": "image/webp",
        "sent_hash": hash_bytes(b"png"), "sent_mime": "image/png", "transcoder_pin": "fixed-pin"
    });
    cases.push(unbound_receipt);
    for case in cases {
        assert!(
            EmbeddingObject::from_bytes(&record(&identity(), &[1.0, 0.0], &case)).is_err(),
            "accepted {case}"
        );
    }
}

#[test]
fn current_embedding_record_rejects_noncanonical_encoding_and_bad_digests() {
    let bytes = record(&identity(), &[1.0, 0.0], &metadata());
    let text = String::from_utf8(bytes.clone()).unwrap();
    let lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut cases = vec![
        bytes[..bytes.len() - 1].to_vec(),
        [bytes.clone(), vec![b'\n']].concat(),
    ];
    for line in [0, 3] {
        let mut modified = lines.clone();
        modified[line].insert(1, ' ');
        if line == 3 {
            modified[4] = lower_hex(&Sha256::digest(modified[3].as_bytes()));
        }
        cases.push(format!("{}\n", modified.join("\n")).into_bytes());
    }
    let mut unpadded = lines.clone();
    unpadded[1] = unpadded[1].trim_end_matches('=').to_owned();
    cases.push(format!("{}\n", unpadded.join("\n")).into_bytes());
    for line in [2, 4] {
        let mut modified = lines.clone();
        modified[line] = "0".repeat(64);
        cases.push(format!("{}\n", modified.join("\n")).into_bytes());
    }
    let mut unknown_identity = identity();
    unknown_identity["unknown"] = Value::Bool(true);
    cases.push(record(&unknown_identity, &[1.0, 0.0], &metadata()));
    for case in cases {
        assert!(EmbeddingObject::from_bytes(&case).is_err());
    }
}

#[test]
fn current_embedding_record_rejects_zero_nonunit_nonfinite_or_wrong_length_vectors() {
    for vector in [
        vec![0.0, 0.0],
        vec![2.0, 0.0],
        vec![f32::NAN, 0.0],
        vec![f32::INFINITY, 0.0],
        vec![1.0],
    ] {
        assert!(EmbeddingObject::from_bytes(&record(&identity(), &vector, &metadata())).is_err());
    }
}

#[test]
fn current_embedding_record_producer_provenance_does_not_change_identity() {
    let mut other = metadata();
    other["producer"]["os"] = Value::String("macos".to_owned());
    other["producer"]["arch"] = Value::String("aarch64".to_owned());
    let first =
        EmbeddingObject::from_bytes(&record(&identity(), &[1.0, 0.0], &metadata())).unwrap();
    let second = EmbeddingObject::from_bytes(&record(&identity(), &[1.0, 0.0], &other)).unwrap();
    assert_eq!(
        first.identity_hash().unwrap(),
        second.identity_hash().unwrap()
    );
    assert_eq!(first.vector, second.vector);
    assert_ne!(first.to_bytes().unwrap(), second.to_bytes().unwrap());
}
