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

// O2 raw-record fixtures deliberately use existing parser/hash/serialization
// APIs. Source and sent hashes are symbolic CAS digests, not codec Goldens:
// source bytes/MIME/ownership and conversion are validated at input call sites.
fn converted_image_record(source_mime: &str) -> (Value, Value) {
    let source_hash = hash_bytes(b"converted image source CAS");
    let receipt = serde_json::json!({
        "source_hash": source_hash,
        "source_mime": source_mime,
        "sent_hash": hash_bytes(b"converted PNG CAS"),
        "sent_mime": "image/png",
        "transcoder_pin": "kio_image_png_v1:codec-fixture:settings-fixed"
    });
    let mut header = identity();
    header["target_type"] = Value::from("image");
    header["target_hash"] = receipt["source_hash"].clone();
    header["context"] = serde_json::json!({
        "image_input_hash": hash_bytes(&canonical_json_bytes(&receipt).unwrap())
    });
    let mut saved = metadata();
    saved["image_input_receipt"] = receipt;
    (header, saved)
}

fn bind_receipt_context(header: &mut Value, saved: &Value) {
    header["context"] = serde_json::json!({
        "image_input_hash": hash_bytes(&canonical_json_bytes(&saved["image_input_receipt"]).unwrap())
    });
}

fn assert_converted_record_round_trip(source_mime: &str) {
    let (header, saved) = converted_image_record(source_mime);
    let bytes = record(&header, &[1.0, 0.0], &saved);
    let parsed = EmbeddingObject::from_bytes(&bytes)
        .expect("a canonical converted-image record with bound receipt must parse");
    assert_eq!(parsed.to_bytes().unwrap(), bytes);
    assert_eq!(
        parsed.identity_hash().unwrap(),
        hash_bytes(&canonical_json_bytes(&header).unwrap())
    );
    let text = String::from_utf8(parsed.to_bytes().unwrap()).unwrap();
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 5);
    assert!(text.ends_with('\n'));
    assert_eq!(serde_json::from_str::<Value>(lines[0]).unwrap(), header);
    assert_eq!(serde_json::from_str::<Value>(lines[3]).unwrap(), saved);
    assert_eq!(
        header["target_hash"],
        saved["image_input_receipt"]["source_hash"]
    );
    assert_eq!(
        header["context"]["image_input_hash"],
        hash_bytes(&canonical_json_bytes(&saved["image_input_receipt"]).unwrap())
    );
}

#[test]
fn converted_gif_embedding_record_round_trips_bound_receipt_context() {
    assert_converted_record_round_trip("image/gif");
}

#[test]
fn converted_webp_embedding_record_round_trips_bound_receipt_context() {
    assert_converted_record_round_trip("image/webp");
}

#[test]
fn converted_embedding_record_rejects_unknown_missing_or_invalid_receipt_fields() {
    let (header, saved) = converted_image_record("image/webp");
    let mut cases = Vec::new();
    let mut unknown = saved.clone();
    unknown["image_input_receipt"]["unknown"] = Value::Bool(true);
    cases.push(("unknown receipt key".to_owned(), unknown));
    for key in [
        "source_hash",
        "source_mime",
        "sent_hash",
        "sent_mime",
        "transcoder_pin",
    ] {
        let mut missing = saved.clone();
        missing["image_input_receipt"]
            .as_object_mut()
            .unwrap()
            .remove(key);
        cases.push((format!("missing {key}"), missing));
    }
    for (key, value) in [
        ("source_hash", "SHA256:invalid"),
        ("sent_hash", "sha256:invalid"),
        ("source_mime", "image/png"),
        ("source_mime", "image/jpeg"),
        ("source_mime", "application/octet-stream"),
        ("sent_mime", "image/webp"),
        ("sent_mime", "image/jpeg"),
        ("transcoder_pin", ""),
        ("transcoder_pin", "   "),
    ] {
        let mut invalid = saved.clone();
        invalid["image_input_receipt"][key] = Value::from(value);
        cases.push((format!("invalid {key}={value:?}"), invalid));
    }
    for (reason, invalid) in cases {
        let mut rebound = header.clone();
        // Recompute BOTH digests so schema checks cannot pass merely because
        // the edited receipt no longer matches context/metadata checksums.
        bind_receipt_context(&mut rebound, &invalid);
        assert!(
            EmbeddingObject::from_bytes(&record(&rebound, &[1.0, 0.0], &invalid)).is_err(),
            "accepted {reason}: {invalid}"
        );
    }
}

#[test]
fn converted_embedding_record_rejects_unbound_source_context_or_target_type() {
    let (header, saved) = converted_image_record("image/gif");
    let mut cases = Vec::new();
    let mut wrong_source = saved.clone();
    wrong_source["image_input_receipt"]["source_hash"] = Value::from(hash_bytes(b"other source"));
    let mut rebound = header.clone();
    bind_receipt_context(&mut rebound, &wrong_source);
    cases.push((
        "receipt source differs from image target",
        rebound,
        wrong_source,
    ));
    let mut wrong_hash = header.clone();
    wrong_hash["context"]["image_input_hash"] = Value::from(hash_bytes(b"other receipt"));
    cases.push((
        "context digest differs from receipt",
        wrong_hash,
        saved.clone(),
    ));
    let mut invalid_hash = header.clone();
    invalid_hash["context"]["image_input_hash"] = Value::from("sha256:invalid");
    cases.push(("malformed context digest", invalid_hash, saved.clone()));
    let mut missing_context = header.clone();
    missing_context.as_object_mut().unwrap().remove("context");
    cases.push((
        "receipt without image context",
        missing_context,
        saved.clone(),
    ));
    let mut null_receipt = saved.clone();
    null_receipt["image_input_receipt"] = Value::Null;
    cases.push((
        "image context without receipt",
        header.clone(),
        null_receipt,
    ));
    let mut unknown_context = header.clone();
    unknown_context["context"]["unknown"] = Value::Bool(true);
    cases.push(("unknown typed context key", unknown_context, saved.clone()));
    let mut missing_context_hash = header.clone();
    missing_context_hash["context"] = serde_json::json!({});
    cases.push((
        "missing typed context hash",
        missing_context_hash,
        saved.clone(),
    ));
    let mut text_target = header.clone();
    text_target["target_type"] = Value::from("chunk");
    cases.push((
        "converted receipt on text target",
        text_target,
        saved.clone(),
    ));
    let mut string_context = header.clone();
    string_context["context"] = Value::from("filename context is chunk-only");
    cases.push(("string context on image target", string_context, saved));
    for (reason, invalid_header, invalid_metadata) in cases {
        assert!(
            EmbeddingObject::from_bytes(&record(&invalid_header, &[1.0, 0.0], &invalid_metadata))
                .is_err(),
            "accepted {reason}: {invalid_header} {invalid_metadata}"
        );
    }
}

#[test]
fn converted_embedding_record_input_change_changes_identity_but_producer_does_not() {
    let (header, saved) = converted_image_record("image/webp");
    let first = EmbeddingObject::from_bytes(&record(&header, &[1.0, 0.0], &saved)).unwrap();
    let mut other_producer = saved.clone();
    other_producer["producer"]["os"] = Value::from("macos");
    other_producer["producer"]["arch"] = Value::from("aarch64");
    let second =
        EmbeddingObject::from_bytes(&record(&header, &[1.0, 0.0], &other_producer)).unwrap();
    assert_eq!(
        first.identity_hash().unwrap(),
        second.identity_hash().unwrap()
    );
    assert_eq!(first.vector, second.vector);
    assert_ne!(first.to_bytes().unwrap(), second.to_bytes().unwrap());
    for field in ["transcoder_pin", "sent_hash"] {
        let mut changed = saved.clone();
        changed["image_input_receipt"][field] = Value::from(if field == "sent_hash" {
            hash_bytes(b"different converted PNG")
        } else {
            "kio_image_png_v1:different-codec-pin".to_owned()
        });
        let mut changed_header = header.clone();
        bind_receipt_context(&mut changed_header, &changed);
        let changed_object =
            EmbeddingObject::from_bytes(&record(&changed_header, &[1.0, 0.0], &changed)).unwrap();
        assert_eq!(changed_object.target_hash, first.target_hash);
        assert_ne!(
            changed_object.identity_hash().unwrap(),
            first.identity_hash().unwrap()
        );
    }
}

#[test]
fn original_png_jpeg_and_text_keep_first_three_lines_and_identity_without_receipts() {
    let cases = [
        (
            "chunk",
            "text",
            None,
            "sha256:42025bcc8131ba76cc0a9127e5cfb5a1a7edd35f554717351d20ff6e2f3eb08c",
        ),
        (
            "chunk",
            "text",
            Some("report name"),
            "sha256:818ca82587d3523afb072171a4f89147211e436c65a4699c5fd1f879650c92f6",
        ),
        (
            "image",
            "original png",
            None,
            "sha256:d88b6d91117588441ca5e54b45bc2618dfcd7d0cee5ae61c240876f9c76bc2d8",
        ),
        (
            "image",
            "original jpeg",
            None,
            "sha256:330a2632e0d8cd171911bcbf0fb949d653f2482a25065e207fe5f6d3543a9544",
        ),
    ];
    for (kind, label, context, expected_hash) in cases {
        let mut header = serde_json::json!({
            "spec_version": 1, "target_type": kind, "target_hash": hash_bytes(label.as_bytes()),
            "profile_hash": hash_bytes(b"profile"), "modality": "multimodal", "dimensions": 2, "distance": "cosine"
        });
        if let Some(context) = context {
            header["context"] = Value::from(context);
        }
        let bytes = record(&header, &[1.0, 0.0], &metadata());
        let object = EmbeddingObject::from_bytes(&bytes).unwrap();
        assert_eq!(object.identity_hash().unwrap(), expected_hash);
        let actual = String::from_utf8(object.to_bytes().unwrap()).unwrap();
        let lines = actual.lines().collect::<Vec<_>>();
        assert_eq!(lines[0].as_bytes(), canonical_json_bytes(&header).unwrap());
        assert_eq!(lines[1], "AACAPwAAAAA=");
        assert_eq!(
            lines[2],
            "434b26042aff3fb844a4c4c6be0d81a079b0ce84cfb8190679024404e5dc4822"
        );
        assert!(serde_json::from_str::<Value>(lines[3]).unwrap()["image_input_receipt"].is_null());
        let mut other = metadata();
        other["producer"]["os"] = Value::from("macos");
        other["producer"]["arch"] = Value::from("aarch64");
        let second = EmbeddingObject::from_bytes(&record(&header, &[1.0, 0.0], &other)).unwrap();
        assert_eq!(second.identity_hash().unwrap(), expected_hash);
        let changed = String::from_utf8(second.to_bytes().unwrap()).unwrap();
        assert_eq!(changed.lines().take(3).collect::<Vec<_>>(), lines[..3]);
    }
}

#[test]
fn original_image_record_rejects_string_or_unbound_object_context() {
    for context in [
        Value::from("chunk filename"),
        serde_json::json!({"image_input_hash": hash_bytes(b"unbound receipt")}),
    ] {
        let mut header = identity();
        header["target_type"] = Value::from("image");
        header["target_hash"] = Value::from(hash_bytes(b"original png"));
        header["context"] = context;
        assert!(
            EmbeddingObject::from_bytes(&record(&header, &[1.0, 0.0], &metadata())).is_err(),
            "original image with null receipt must not acquire context: {header}"
        );
    }
}

#[test]
fn text_record_rejects_converted_receipt_even_with_valid_chunk_context() {
    let (_, saved) = converted_image_record("image/webp");
    let mut header = identity();
    header["target_hash"] = saved["image_input_receipt"]["source_hash"].clone();
    // Matching source hash removes that independent rejection reason. A
    // perfectly valid chunk string context still cannot authorize a receipt.
    header["context"] = Value::from("report name");
    assert!(EmbeddingObject::from_bytes(&record(&header, &[1.0, 0.0], &saved)).is_err());
}
