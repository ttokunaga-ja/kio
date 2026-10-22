use super::*;
use crate::gemini_embedding::http_tests::ImageCapture;
use crate::types::{AdapterUsage, BillableUnit, BillableUnitKind};
use base64::Engine as _;

#[test]
fn batch_http_original_png_has_one_inline_member_and_exact_key() {
    crate::tool_lock::with_test_role_auth("embedding", |settings| {
        let bytes = base64::engine::general_purpose::STANDARD.decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGP4DwQACfsD/fteaysAAAAASUVORK5CYII="
        ).unwrap();
        let capture = ImageCapture::new(json!({
            "name": "batches/image-wire",
            "metadata": {"name": "batches/image-wire", "state": "BATCH_STATE_PENDING", "displayName": "kio-image-wire"}
        }));
        let mut client =
            EnvGeminiBatchClient::with_base_url(Arc::new(settings.clone()), capture.url.clone());
        client.http = capture.http.clone();
        let result = client.submit_job(
            "gemini-embedding-2",
            768,
            "kio-image-wire",
            &[GeminiBatchEmbedInput {
                key: "image-key-unchanged".into(),
                content: EmbeddingContent::Image {
                    bytes: bytes.clone(),
                    mime: "image/png".into(),
                },
            }],
        );
        let captured = capture.finish(result.is_err());
        assert!(
            matches!(result.expect("valid original image must reach Batch HTTP"),
            EmbeddingOutcome::Pending { remote_handle } if remote_handle.as_str() == "batches/image-wire")
        );
        let (head, body) = captured.expect("one image POST must be captured");
        assert!(head.starts_with(
            "POST /v1beta/models/gemini-embedding-2:asyncBatchEmbedContent HTTP/1.1\r\n"
        ));
        assert_eq!(
            body,
            json!({"batch": {
                "model": "models/gemini-embedding-2",
                "displayName": "kio-image-wire",
                "inputConfig": {"requests": {"requests": [{
                    "metadata": {"key": "image-key-unchanged"},
                    "request": {
                        "model": "models/gemini-embedding-2",
                        "content": {"parts": [{"inlineData": {
                            "mimeType": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(&bytes)
                        }}]},
                        "outputDimensionality": 768
                    }
                }]}}
            }})
        );
    });
}

#[test]
fn batch_http_image_results_keep_separate_image_units_and_vector_bits() {
    crate::tool_lock::with_test_role_auth("embedding", |settings| {
        for metadata_output in [false, true] {
            let mut vector = vec![0.0_f32; 768];
            vector[0] = 1.0;
            let lines = json!({"inlinedResponses": [{
                "metadata": {"key": "image-key-unchanged"},
                "response": {
                    "embedding": {"values": vector},
                    "usageMetadata": {"promptTokenCount": 7,
                        "promptTokenDetails": [{"modality": "IMAGE", "tokenCount": 7}]}
                }
            }]});
            let mut response = json!({"name": "batches/image-wire", "done": true,
                "metadata": {"name": "batches/image-wire", "state": "BATCH_STATE_SUCCEEDED", "displayName": "kio-image-wire"}
            });
            if metadata_output {
                response["metadata"]["output"] = json!({"inlinedResponses": lines});
            } else {
                response["response"] = json!({"inlinedResponses": lines});
            }
            let capture = ImageCapture::new(response);
            let mut client = EnvGeminiBatchClient::with_base_url(
                Arc::new(settings.clone()),
                capture.url.clone(),
            );
            client.http = capture.http.clone();
            let result = client.poll_job(&EmbeddingRemoteHandle("batches/image-wire".into()));
            let captured = capture.finish(result.is_err());
            let lines = result.unwrap().results.unwrap();
            let (head, body) = captured.unwrap();
            assert!(head.starts_with("GET /v1beta/batches/image-wire HTTP/1.1\r\n"));
            assert!(body.is_null());
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].key, "image-key-unchanged");
            assert_eq!(
                lines[0].usage,
                EmbeddingUsage::Metered {
                    usage: AdapterUsage::BillableUnits {
                        billable_units: vec![BillableUnit {
                            kind: BillableUnitKind::ImageTokensIn,
                            count: 7
                        }]
                    }
                }
            );
            assert!(lines[0].error.is_none());
            assert_eq!(
                lines[0]
                    .values
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>(),
                vector.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
            );
        }
    });
}
