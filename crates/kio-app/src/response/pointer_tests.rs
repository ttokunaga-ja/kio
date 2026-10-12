use super::pointer::{
    EvidenceOpenResponse, EvidenceViewResponse, ObjectChunkResponse, ObjectFileResponse,
    OpenResponse, RawPointerResponse, ViewResponse,
};
use super::{CommandResponse, Completion, ResponseBody, project_legacy};
use serde_json::{Value, json};
use std::path::PathBuf;

fn assert_projection(body: ResponseBody, legacy: Value) {
    let actual = project_legacy(CommandResponse {
        body,
        completion: Completion::Complete,
    })
    .expect("project typed pointer response");
    assert_eq!(actual, legacy);
    assert_eq!(
        serde_json::to_vec(&actual).expect("typed pointer response bytes"),
        serde_json::to_vec(&legacy).expect("legacy pointer response bytes")
    );
}

#[test]
fn evidence_open_projection_preserves_nullable_path_and_resolution_flags() {
    for (path, temporary, commit_shallow, manifest_missing) in [
        (Some(PathBuf::from("scope/日本語.md")), false, false, false),
        (Some(PathBuf::from("cache/日本語.md")), true, true, false),
        (None, false, false, true),
    ] {
        let raw_hash = "a".repeat(64);
        let chunk_hash = "b".repeat(64);
        let legacy = json!({
            "status": "opened",
            "path": path,
            "raw_hash": raw_hash,
            "chunk_hash": chunk_hash,
            "temporary": temporary,
            "commit_shallow": commit_shallow,
            "manifest_missing": manifest_missing,
        });
        assert_projection(
            ResponseBody::Open(OpenResponse::Evidence(EvidenceOpenResponse {
                status: "opened".to_owned(),
                path,
                raw_hash,
                chunk_hash,
                temporary,
                commit_shallow,
                manifest_missing,
            })),
            legacy,
        );
    }
}

#[test]
fn evidence_view_projection_preserves_full_and_short_hash_field_presence() {
    for temporary in [Some(false), Some(true), None] {
        for (path, view_path, view_byte_start, view_byte_end) in [
            (
                Some(PathBuf::from("scope/日本語.md")),
                Some(PathBuf::from("cache/日本語.view.md")),
                Some(0),
                Some(u64::MAX),
            ),
            (None, None, None, None),
        ] {
            let raw_hash = "a".repeat(64);
            let chunk_hash = "b".repeat(64);
            let commit_shallow = path.is_none();
            let manifest_missing = false;
            let legacy = match temporary {
                Some(temporary) => json!({
                    "status": "viewed",
                    "raw_hash": raw_hash,
                    "chunk_hash": chunk_hash,
                    "view_path": view_path,
                    "view_byte_start": view_byte_start,
                    "view_byte_end": view_byte_end,
                    "path": path,
                    "temporary": temporary,
                    "commit_shallow": commit_shallow,
                    "manifest_missing": manifest_missing,
                }),
                None => json!({
                    "status": "viewed",
                    "raw_hash": raw_hash,
                    "chunk_hash": chunk_hash,
                    "view_path": view_path,
                    "view_byte_start": view_byte_start,
                    "view_byte_end": view_byte_end,
                    "path": path,
                    "commit_shallow": commit_shallow,
                    "manifest_missing": manifest_missing,
                }),
            };
            let object = legacy.as_object().expect("view response object");
            assert_eq!(object.contains_key("temporary"), temporary.is_some());
            assert!(!object.contains_key("text"));
            assert_projection(
                ResponseBody::View(ViewResponse::Evidence(EvidenceViewResponse {
                    status: "viewed".to_owned(),
                    raw_hash,
                    chunk_hash,
                    view_path,
                    view_byte_start,
                    view_byte_end,
                    path,
                    temporary,
                    commit_shallow,
                    manifest_missing,
                })),
                legacy,
            );
        }
    }
}

#[test]
fn short_hash_raw_projection_preserves_open_and_view_shapes() {
    for as_view in [false, true] {
        for temporary in [false, true] {
            let status = if as_view { "viewed" } else { "opened" };
            let raw_hash = "a".repeat(64);
            let path = PathBuf::from("cache/日本語.raw");
            let legacy = json!({
                "status": status,
                "object_type": "raw",
                "raw_hash": raw_hash,
                "path": path,
                "temporary": temporary,
            });
            let payload = RawPointerResponse {
                status: status.to_owned(),
                object_type: "raw".to_owned(),
                raw_hash,
                path,
                temporary,
            };
            let body = if as_view {
                ResponseBody::View(ViewResponse::Raw(payload))
            } else {
                ResponseBody::Open(OpenResponse::Raw(payload))
            };
            assert_projection(body, legacy);
        }
    }
}

#[test]
fn object_file_projection_preserves_hash_path_and_temporary_fields() {
    for as_view in [false, true] {
        for object_type in ["image", "raw", "prepared"] {
            for temporary in [false, true] {
                let status = if as_view { "viewed" } else { "opened" };
                let hash = "c".repeat(64);
                let path = PathBuf::from("cache/日本語.object");
                let legacy = json!({
                    "status": status,
                    "object_type": object_type,
                    "hash": hash,
                    "path": path,
                    "temporary": temporary,
                });
                let payload = ObjectFileResponse {
                    status: status.to_owned(),
                    object_type: object_type.to_owned(),
                    hash,
                    path,
                    temporary,
                };
                let body = if as_view {
                    ResponseBody::View(ViewResponse::ObjectFile(payload))
                } else {
                    ResponseBody::Open(OpenResponse::ObjectFile(payload))
                };
                assert_projection(body, legacy);
            }
        }
    }
}

#[test]
fn object_chunk_projection_preserves_empty_and_multiline_text_without_path() {
    for as_view in [false, true] {
        for text in ["", "日本語の本文\nsecond line\n"] {
            let status = if as_view { "viewed" } else { "opened" };
            let hash = "d".repeat(64);
            let legacy = json!({
                "status": status,
                "object_type": "chunk",
                "hash": hash,
                "text": text,
            });
            let object = legacy.as_object().expect("chunk response object");
            assert!(!object.contains_key("path"));
            assert!(!object.contains_key("view_path"));
            let payload = ObjectChunkResponse {
                status: status.to_owned(),
                object_type: "chunk".to_owned(),
                hash,
                text: text.to_owned(),
            };
            let body = if as_view {
                ResponseBody::View(ViewResponse::ObjectChunk(payload))
            } else {
                ResponseBody::Open(OpenResponse::ObjectChunk(payload))
            };
            assert_projection(body, legacy);
        }
    }
}
