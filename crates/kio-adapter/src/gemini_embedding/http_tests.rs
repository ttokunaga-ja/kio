use super::*;
use crate::types::{EmbeddingInputType, EmbeddingMemberResult};
use std::io::{Read, Write};
use std::net::TcpListener;

fn capture_response(
    response: Value,
) -> (
    String,
    AuthenticatedHttpClient,
    std::thread::JoinHandle<(String, Value)>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let tls = crate::http_policy::test_tls::Fixture::new();
    let http = tls.client(HttpPolicy::default());
    let server = std::thread::spawn(move || {
        let stream = tls.accept(&listener);
        read_and_respond(stream, response)
    });
    (format!("https://{address}"), http, server)
}

fn read_and_respond(mut stream: impl Read + Write, response: Value) -> (String, Value) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let end = loop {
        if let Some(at) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break at + 4;
        }
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0, "request ended before its headers");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= 64 * 1024);
    };
    let head = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    assert!(length <= 64 * 1024);
    let mut body = bytes[end..].to_vec();
    while body.len() < length {
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0, "request ended before its JSON body");
        body.extend_from_slice(&chunk[..count]);
        assert!(body.len() <= 64 * 1024);
    }
    assert_eq!(body.len(), length);
    let response = serde_json::to_vec(&response).unwrap();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    )
    .unwrap();
    stream.write_all(&response).unwrap();
    (
        head,
        if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        },
    )
}

#[test]
fn sync_text_http_characterization_preserves_request_order_and_vector_bits() {
    crate::tool_lock::with_test_role_auth("embedding", |settings| {
        let mut first = vec![0.0_f32; 768];
        first[0] = 1.0;
        let mut second = vec![0.0_f32; 768];
        second[1] = 1.0;
        let (url, http, server) = capture_response(json!({
            "embeddings": [{"values": first}, {"values": second}],
            "usageMetadata": {
                "promptTokenCount": 7,
                "promptTokenDetails": [{"modality": "TEXT", "tokenCount": 7}]
            }
        }));
        let mut client =
            EnvGeminiEmbeddingClient::with_base_url(Arc::new(settings.clone()), url).unwrap();
        client.http = http;
        let adapter = GeminiEmbeddingAdapter::new(client, ADOPTED_MODEL_PIN, ADOPTED_DIMENSIONS);
        let profile = adapter.profile().tool_profile_hash;
        let outcome = adapter.embed(EmbeddingRequest {
            input_type: EmbeddingInputType::MarkdownChunk,
            items: vec![
                EmbeddingItem::text("member-z", "file: z.md\n世界"),
                EmbeddingItem::text("member-a", "file: a.md\nhello"),
            ],
            idempotency_token: None,
        });
        let (head, body) = server.join().unwrap();
        let completed = outcome.unwrap().into_completed().unwrap();
        assert!(
            head.starts_with(
                "POST /v1beta/models/gemini-embedding-2:batchEmbedContents HTTP/1.1\r\n"
            )
        );
        // docs07 §5.3 explicitly moves only this config nesting. Checkpoint3
        // recorded its real wire RuntimeRed; text, order/profile/vector bits stay pinned.
        assert_eq!(
            body,
            json!({"requests": [
                {"model": "models/gemini-embedding-2", "content": {"parts": [{"text": "file: z.md\n世界"}]}, "embedContentConfig": {"outputDimensionality": 768}},
                {"model": "models/gemini-embedding-2", "content": {"parts": [{"text": "file: a.md\nhello"}]}, "embedContentConfig": {"outputDimensionality": 768}}
            ]})
        );
        assert_eq!(completed.embedding_profile_hash, profile);
        assert_eq!(completed.dimensions, 768);
        assert_eq!(completed.members.len(), 2);
        for (member, id, expected) in [
            (&completed.members[0], "member-z", first),
            (&completed.members[1], "member-a", second),
        ] {
            assert_eq!(member.id, id);
            let EmbeddingMemberResult::Vector { vector } = &member.result else {
                panic!("successful member must retain its vector");
            };
            assert_eq!(
                vector.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                expected.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
            );
        }
        assert_eq!(
            completed.usage,
            EmbeddingUsage::Metered {
                usage: AdapterUsage::BillableUnits {
                    billable_units: vec![BillableUnit {
                        kind: BillableUnitKind::TokensIn,
                        count: 7
                    }]
                }
            }
        );
    });
}

pub(crate) struct ImageCapture {
    pub(crate) url: String,
    pub(crate) http: AuthenticatedHttpClient,
    address: std::net::SocketAddr,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    server: std::thread::JoinHandle<Option<(String, Value)>>,
}

impl ImageCapture {
    pub(crate) fn new(response: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let tls = crate::http_policy::test_tls::Fixture::new();
        let http = tls.client(HttpPolicy::default());
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_cancel = cancel.clone();
        let server = std::thread::spawn(move || {
            let stream = tls.accept(&listener);
            if server_cancel.load(std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            Some(read_and_respond(stream, response))
        });
        Self {
            url: format!("https://{address}"),
            http,
            address,
            cancel,
            server,
        }
    }

    pub(crate) fn finish(self, returned_before_http: bool) -> Option<(String, Value)> {
        if returned_before_http {
            self.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            // Wake the fixture's accept without generating an HTTP exchange.
            // The server observes cancellation before reading TLS or payload.
            let _ = std::net::TcpStream::connect(self.address);
        }
        self.server.join().unwrap()
    }
}

fn original_image_wire(bytes: Vec<u8>, mime: &str) {
    use base64::Engine as _;
    crate::tool_lock::with_test_role_auth("embedding", |settings| {
        let mut vector = vec![0.0_f32; 768];
        vector[0] = 1.0;
        let capture = ImageCapture::new(json!({
            "embeddings": [{"values": vector}],
            "usageMetadata": {"promptTokenCount": 7,
                "promptTokenDetails": [{"modality": "IMAGE", "tokenCount": 7}]}
        }));
        let mut client = EnvGeminiEmbeddingClient::with_base_url(
            Arc::new(settings.clone()),
            capture.url.clone(),
        )
        .unwrap();
        client.http = capture.http.clone();
        let result = client.embed(
            &[EmbeddingItem {
                id: "original-image".into(),
                content: EmbeddingContent::Image {
                    bytes: bytes.clone(),
                    mime: mime.into(),
                },
            }],
            ADOPTED_MODEL_PIN,
            ADOPTED_DIMENSIONS,
            None,
        );
        let captured = capture.finish(result.is_err());
        let output = result.expect("valid original image must reach actual rooted HTTP client");
        let (head, body) = captured.expect("one image HTTP exchange must be captured");
        assert!(
            head.starts_with(
                "POST /v1beta/models/gemini-embedding-2:batchEmbedContents HTTP/1.1\r\n"
            )
        );
        assert_eq!(
            body,
            json!({"requests": [{
                "model": "models/gemini-embedding-2",
                "content": {"parts": [{"inlineData": {
                    "mimeType": mime, "data": base64::engine::general_purpose::STANDARD.encode(&bytes)
                }}]},
                "embedContentConfig": {"outputDimensionality": 768}
            }]})
        );
        assert_eq!(output.vectors.len(), 1);
        assert_eq!(output.vectors[0].id, "original-image");
        assert_eq!(
            output.usage,
            EmbeddingUsage::Metered {
                usage: AdapterUsage::BillableUnits {
                    billable_units: vec![BillableUnit {
                        kind: BillableUnitKind::ImageTokensIn,
                        count: 7
                    }]
                }
            }
        );
        assert_eq!(
            output.vectors[0]
                .vector
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
            vector.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
    });
}

#[test]
fn sync_http_original_png_uses_one_inline_image_and_nested_768_config() {
    use base64::Engine as _;
    let png = base64::engine::general_purpose::STANDARD.decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGP4DwQACfsD/fteaysAAAAASUVORK5CYII="
    ).unwrap();
    original_image_wire(png, "image/png");
}

#[test]
fn sync_http_text_uses_documented_nested_768_config() {
    crate::tool_lock::with_test_role_auth("embedding", |settings| {
        let mut vector = vec![0.0_f32; 768];
        vector[0] = 1.0;
        let (url, http, server) = capture_response(json!({
            "embeddings": [{"values": vector}],
            "usageMetadata": {"promptTokenCount": 7,
                "promptTokenDetails": [{"modality": "TEXT", "tokenCount": 7}]}
        }));
        let mut client =
            EnvGeminiEmbeddingClient::with_base_url(Arc::new(settings.clone()), url).unwrap();
        client.http = http;
        let output = client.embed(
            &[EmbeddingItem::text("text", "unchanged text")],
            ADOPTED_MODEL_PIN,
            ADOPTED_DIMENSIONS,
            None,
        );
        let (_, body) = server.join().unwrap();
        assert_eq!(output.unwrap().vectors[0].vector.len(), 768);
        assert_eq!(
            body,
            json!({"requests": [{
                "model": "models/gemini-embedding-2",
                "content": {"parts": [{"text": "unchanged text"}]},
                "embedContentConfig": {"outputDimensionality": 768}
            }]})
        );
    });
}

// Newly authored 2x1 JPEG control, image0.25.10 encoder quality100.
// Provenance: B1/O2-golden-fixture-prep; SHA256 7f41ab0a3a72445155cad9af8389fcc442274796fffcb14268cd09a94559197f.
#[test]
fn sync_http_original_jpeg_uses_one_inline_image_and_nested_768_config() {
    use base64::Engine as _;
    let jpeg = base64::engine::general_purpose::STANDARD.decode("/9j/4AAQSkZJRgABAgAAAQABAAD/wAARCAABAAIDAREAAhEBAxEB/9sAQwABAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB/9sAQwEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwD2b4M/8kf+FH/ZNfAv/qL6XX/MRxx/yWvGH/ZU8Qf+rbFn/Mj+1E/5WY/tE/8AtOr6W/8A6/7xAP/Z").unwrap();
    original_image_wire(jpeg, "image/jpeg");
}
