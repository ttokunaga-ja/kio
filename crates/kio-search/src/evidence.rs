//! Evidence Pointer contracts.

use serde::{Deserialize, Deserializer, Serialize};

use crate::{Result, SearchError};

pub const EVIDENCE_POINTER_SCHEMA_VERSION: u64 = 1;
pub const SHA256_HASH_PREFIX: &str = "sha256:";
pub const SHA256_DIGEST_LENGTH: usize = 64;

const FULL_HASH_REQUIREMENT: &str =
    "hash must be `sha256:` followed by 64 lowercase hexadecimal characters";

fn deserialize_present_heading_path<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer).map(Some)
}

/// A full SHA-256 identifier that has passed the Kio object-hash grammar.
///
/// The borrowed representation prevents callers from accidentally substituting
/// unvalidated input after validation. Filesystem fanout should be derived from
/// [`Self::digest`] or [`Self::fanout`], never from the original input directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedHash<'a> {
    value: &'a str,
    digest: &'a str,
}

impl<'a> ValidatedHash<'a> {
    fn new(value: &'a str, field: &str) -> Result<Self> {
        let digest = validate_full_hash(value).map_err(|_| {
            SearchError::Evidence(format!(
                "invalid evidence pointer {field}: {FULL_HASH_REQUIREMENT}"
            ))
        })?;
        Ok(Self { value, digest })
    }

    pub fn as_str(self) -> &'a str {
        self.value
    }

    pub fn digest(self) -> &'a str {
        self.digest
    }

    pub fn fanout(self) -> (&'a str, &'a str) {
        (&self.digest[..2], &self.digest[2..4])
    }
}

/// A pointer whose hash-bearing fields all satisfy the Kio object-hash grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedEvidencePointer<'a> {
    pointer: &'a EvidencePointer,
    commit: ValidatedHash<'a>,
    tree: Option<ValidatedHash<'a>>,
    raw_hash: ValidatedHash<'a>,
    tool_profile_hash: ValidatedHash<'a>,
    chunk_hash: ValidatedHash<'a>,
}

impl<'a> ValidatedEvidencePointer<'a> {
    pub fn as_pointer(self) -> &'a EvidencePointer {
        self.pointer
    }

    pub fn commit(self) -> ValidatedHash<'a> {
        self.commit
    }

    pub fn tree(self) -> Option<ValidatedHash<'a>> {
        self.tree
    }

    pub fn raw_hash(self) -> ValidatedHash<'a> {
        self.raw_hash
    }

    pub fn tool_profile_hash(self) -> ValidatedHash<'a> {
        self.tool_profile_hash
    }

    pub fn chunk_hash(self) -> ValidatedHash<'a> {
        self.chunk_hash
    }
}

/// Validates the canonical Kio object-hash representation and returns its
/// lowercase 64-character digest without the `sha256:` prefix.
pub fn validate_full_hash(hash: &str) -> Result<&str> {
    let digest = hash
        .strip_prefix(SHA256_HASH_PREFIX)
        .ok_or_else(|| SearchError::Evidence(FULL_HASH_REQUIREMENT.to_owned()))?;
    if digest.len() != SHA256_DIGEST_LENGTH
        || !digest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(SearchError::Evidence(FULL_HASH_REQUIREMENT.to_owned()));
    }
    Ok(digest)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedEvidencePointer")]
pub struct EvidencePointer {
    pub schema_version: u64,
    pub commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    pub raw_hash: String,
    pub tool_profile_hash: String,
    pub chunk_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_at_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading_path: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_start: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_end: Option<u64>,
    pub scope_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedEvidencePointer {
    schema_version: u64,
    commit: String,
    tree: Option<String>,
    raw_hash: String,
    tool_profile_hash: String,
    chunk_hash: String,
    path_at_commit: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present_heading_path")]
    heading_path: Option<Vec<String>>,
    section_id: Option<String>,
    byte_start: Option<u64>,
    byte_end: Option<u64>,
    scope_id: String,
    scope_path: Option<String>,
}

impl TryFrom<UncheckedEvidencePointer> for EvidencePointer {
    type Error = SearchError;

    fn try_from(value: UncheckedEvidencePointer) -> Result<Self> {
        let pointer = Self {
            schema_version: value.schema_version,
            commit: value.commit,
            tree: value.tree,
            raw_hash: value.raw_hash,
            tool_profile_hash: value.tool_profile_hash,
            chunk_hash: value.chunk_hash,
            path_at_commit: value.path_at_commit,
            heading_path: value.heading_path,
            section_id: value.section_id,
            byte_start: value.byte_start,
            byte_end: value.byte_end,
            scope_id: value.scope_id,
            scope_path: value.scope_path,
        };
        pointer.validate()?;
        Ok(pointer)
    }
}

impl EvidencePointer {
    /// Validates the schema version and every hash-bearing pointer field.
    pub fn validate(&self) -> Result<ValidatedEvidencePointer<'_>> {
        if self.schema_version != EVIDENCE_POINTER_SCHEMA_VERSION {
            return Err(SearchError::Evidence(
                "unsupported evidence schema version".to_owned(),
            ));
        }

        if self
            .path_at_commit
            .as_deref()
            .is_some_and(|path| !kio_core::dag::is_logical_direct_child(path))
        {
            return Err(SearchError::Evidence(
                "path_at_commit must be a logical direct child name".to_owned(),
            ));
        }

        if let Some(heading_path) = &self.heading_path {
            if heading_path.is_empty() {
                return Err(SearchError::Evidence(
                    "heading_path must be omitted when no headings are present".to_owned(),
                ));
            }
            if heading_path.len() > 64 {
                return Err(SearchError::Evidence(
                    "heading_path must contain at most 64 headings".to_owned(),
                ));
            }
            if heading_path.iter().any(|heading| heading.is_empty()) {
                return Err(SearchError::Evidence(
                    "heading_path entries must be non-empty strings".to_owned(),
                ));
            }
        }

        Ok(ValidatedEvidencePointer {
            pointer: self,
            commit: ValidatedHash::new(&self.commit, "commit")?,
            tree: self
                .tree
                .as_deref()
                .map(|tree| ValidatedHash::new(tree, "tree"))
                .transpose()?,
            raw_hash: ValidatedHash::new(&self.raw_hash, "raw_hash")?,
            tool_profile_hash: ValidatedHash::new(&self.tool_profile_hash, "tool_profile_hash")?,
            chunk_hash: ValidatedHash::new(&self.chunk_hash, "chunk_hash")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidencePointerIssueRequest {
    pub scope_id: String,
    pub scope_path: Option<String>,
    pub commit: String,
    pub tree: Option<String>,
    pub raw_hash: String,
    pub tool_profile_hash: String,
    pub chunk_hash: String,
    pub path_at_commit: Option<String>,
    pub heading_path: Option<Vec<String>>,
    pub section_id: Option<String>,
    pub byte_start: Option<u64>,
    pub byte_end: Option<u64>,
}

pub fn issue_evidence_pointer(request: EvidencePointerIssueRequest) -> Result<EvidencePointer> {
    let heading_path = request.heading_path.filter(|headings| !headings.is_empty());
    let pointer = EvidencePointer {
        schema_version: EVIDENCE_POINTER_SCHEMA_VERSION,
        commit: request.commit,
        tree: request.tree,
        raw_hash: request.raw_hash,
        tool_profile_hash: request.tool_profile_hash,
        chunk_hash: request.chunk_hash,
        path_at_commit: request.path_at_commit,
        heading_path,
        section_id: request.section_id,
        byte_start: request.byte_start,
        byte_end: request.byte_end,
        scope_id: request.scope_id,
        scope_path: request.scope_path,
    };
    pointer.validate()?;
    Ok(pointer)
}

pub fn evidence_pointer_to_uri(pointer: &EvidencePointer) -> Result<String> {
    pointer.validate()?;
    Ok(format!(
        "kio://{}/{}/{}/{}/{}",
        pointer.scope_id,
        pointer.commit,
        pointer.raw_hash,
        pointer.tool_profile_hash,
        pointer.chunk_hash
    ))
}

pub fn parse_evidence_pointer_uri(uri: &str) -> Result<EvidencePointer> {
    let rest = uri
        .strip_prefix("kio://")
        .ok_or_else(|| SearchError::Evidence("evidence URI must start with kio://".to_owned()))?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let schema_version = match query {
        "" => EVIDENCE_POINTER_SCHEMA_VERSION,
        "sv=1" => EVIDENCE_POINTER_SCHEMA_VERSION,
        _ => {
            return Err(SearchError::Evidence(
                "evidence URI query must be empty or exactly sv=1".to_owned(),
            ));
        }
    };
    if schema_version != EVIDENCE_POINTER_SCHEMA_VERSION {
        return Err(SearchError::Evidence(
            "KIO-E-CONFIG-SCHEMA-001: unsupported evidence schema version".to_owned(),
        ));
    }
    let parts = path.split('/').collect::<Vec<_>>();
    // `kio://<scope_id>/object/<type>/<hash>` — four segments, not three
    // (08 §2.3). The earlier `len() == 3` guard never fired for a real object
    // URI, so those fell through to the generic segment-count error below and
    // reported a misleading reason. Grammar lives in `crate::object_uri`.
    if parts.get(1) == Some(&crate::object_uri::OBJECT_SEGMENT) {
        return Err(SearchError::Evidence(
            "object reference URI is not an evidence pointer".to_owned(),
        ));
    }
    if parts.len() != 5 {
        return Err(SearchError::Evidence(
            "evidence URI must have five path segments".to_owned(),
        ));
    }
    let pointer = EvidencePointer {
        schema_version,
        scope_id: parts[0].to_owned(),
        commit: parts[1].to_owned(),
        raw_hash: parts[2].to_owned(),
        tool_profile_hash: parts[3].to_owned(),
        chunk_hash: parts[4].to_owned(),
        tree: None,
        path_at_commit: None,
        heading_path: None,
        section_id: None,
        byte_start: None,
        byte_end: None,
        scope_path: None,
    };
    pointer.validate()?;
    Ok(pointer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer() -> EvidencePointer {
        EvidencePointer {
            schema_version: 1,
            commit: "sha256:9f2c1a7b04dee5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e"
                .to_owned(),
            tree: Some(
                "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                    .to_owned(),
            ),
            raw_hash: "sha256:74bcb92d8088c950e45e4c43563332da2ca1e04b25d6d4016aa43f830d4cca8a"
                .to_owned(),
            tool_profile_hash:
                "sha256:e067e42e6634b8043f46a4b7f55257ab10ca6266be80cc47b6a68a5aacd2c8f0".to_owned(),
            chunk_hash: "sha256:c5e31f10da04b722769bdbbd60a55b94c177b5f3bf9c64e5341be7281d115c3d"
                .to_owned(),
            path_at_commit: Some("report.pdf".to_owned()),
            heading_path: Some(vec!["認証仕様".to_owned(), "API Token".to_owned()]),
            section_id: Some("認証仕様/api-token".to_owned()),
            byte_start: Some(1200),
            byte_end: Some(1500),
            scope_id: "scope_01J8ZQABCDEFGHJKMNPQRS".to_owned(),
            scope_path: Some("/tmp/scope".to_owned()),
        }
    }

    fn issue_request(heading_path: Option<Vec<String>>) -> EvidencePointerIssueRequest {
        let pointer = pointer();
        EvidencePointerIssueRequest {
            scope_id: pointer.scope_id,
            scope_path: pointer.scope_path,
            commit: pointer.commit,
            tree: pointer.tree,
            raw_hash: pointer.raw_hash,
            tool_profile_hash: pointer.tool_profile_hash,
            chunk_hash: pointer.chunk_hash,
            path_at_commit: pointer.path_at_commit,
            heading_path,
            section_id: pointer.section_id,
            byte_start: pointer.byte_start,
            byte_end: pointer.byte_end,
        }
    }

    #[test]
    fn path_at_commit_obeys_logical_name_schema() {
        for name in ["", ".", "..", "a/b", "a\0b"] {
            let mut value = serde_json::to_value(pointer()).unwrap();
            value["path_at_commit"] = serde_json::json!(name);
            assert!(
                serde_json::from_value::<EvidencePointer>(value).is_err(),
                "{name:?}"
            );
        }
        for name in ["a\\b", "CON", "a\nb"] {
            let mut value = serde_json::to_value(pointer()).unwrap();
            value["path_at_commit"] = serde_json::json!(name);
            assert!(
                serde_json::from_value::<EvidencePointer>(value).is_ok(),
                "{name:?}"
            );
        }
    }

    #[test]
    fn ct3_evidence_001_issue_pointer_has_required_fields_and_uri() {
        let pointer = pointer();
        let uri = evidence_pointer_to_uri(&pointer).unwrap();
        assert!(uri.starts_with("kio://scope_01J8ZQ"));
        assert_eq!(pointer.schema_version, 1);
        assert!(pointer.heading_path.is_some());
        assert!(pointer.byte_start.is_some());
    }

    #[test]
    fn ct3_uri_001_json_uri_json_roundtrip_drops_only_optional_fields() {
        let uri = evidence_pointer_to_uri(&pointer()).unwrap();
        assert_eq!(
            uri,
            "kio://scope_01J8ZQABCDEFGHJKMNPQRS/sha256:9f2c1a7b04dee5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e/sha256:74bcb92d8088c950e45e4c43563332da2ca1e04b25d6d4016aa43f830d4cca8a/sha256:e067e42e6634b8043f46a4b7f55257ab10ca6266be80cc47b6a68a5aacd2c8f0/sha256:c5e31f10da04b722769bdbbd60a55b94c177b5f3bf9c64e5341be7281d115c3d"
        );
        let parsed = parse_evidence_pointer_uri(&uri).unwrap();
        assert_eq!(parsed.scope_id, pointer().scope_id);
        assert!(parsed.heading_path.is_none());
    }

    #[test]
    fn ct3_uri_002_object_reference_is_distinct_from_evidence_pointer() {
        assert!(parse_evidence_pointer_uri("kio://scope/object/image/sha256:abc").is_err());
    }

    #[test]
    fn object_reference_is_rejected_for_being_an_object_reference() {
        // The assertion above only demanded *an* error, which let a stale
        // `len() == 3` guard sit dead while real four-segment object URIs fell
        // through to the generic segment-count message. Pin the reason.
        let digest = "a".repeat(SHA256_DIGEST_LENGTH);
        for uri in [
            format!("kio://scope/object/image/sha256:{digest}"),
            "kio://scope/object/image/sha256:abc".to_owned(),
            "kio://scope/object/raw/sha256:abc".to_owned(),
        ] {
            let error = parse_evidence_pointer_uri(&uri).unwrap_err().to_string();
            assert!(
                error.contains("object reference URI is not an evidence pointer"),
                "{uri} reported: {error}"
            );
        }
    }

    #[test]
    fn ct3_uri_004_sv_query_grammar_is_exact() {
        let uri = evidence_pointer_to_uri(&pointer()).unwrap();
        assert_eq!(parse_evidence_pointer_uri(&uri).unwrap().schema_version, 1);
        assert_eq!(
            parse_evidence_pointer_uri(&(uri.clone() + "?sv=1"))
                .unwrap()
                .schema_version,
            1
        );
        for query in [
            "sv=99",
            "sv=",
            "foo=1",
            "sv=1&sv=1",
            "sv=1&",
            "&sv=1",
            "sv=1&foo=1",
        ] {
            let error = parse_evidence_pointer_uri(&format!("{uri}?{query}"))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("query must be empty or exactly sv=1"),
                "{query}: {error}"
            );
        }
    }

    fn inline_pointer_with_raw_hash(raw_hash: &str) -> serde_json::Value {
        let mut value = serde_json::to_value(pointer()).unwrap();
        value["raw_hash"] = serde_json::Value::String(raw_hash.to_owned());
        value
    }

    #[test]
    fn r23_cand_069_inline_json_rejects_absolute_raw_hash() {
        let error = serde_json::from_value::<EvidencePointer>(inline_pointer_with_raw_hash(
            "/tmp/synthetic-marker.json",
        ))
        .unwrap_err()
        .to_string();

        assert!(error.contains("raw_hash"));
        assert!(error.contains("64 lowercase hexadecimal"));
    }

    #[test]
    fn r23_cand_069_inline_json_rejects_parent_traversal_raw_hash() {
        for raw_hash in [
            "../../synthetic-marker.json",
            "sha256:../../synthetic-marker.json",
            r"sha256:..\..\synthetic-marker.json",
        ] {
            let error =
                serde_json::from_value::<EvidencePointer>(inline_pointer_with_raw_hash(raw_hash))
                    .unwrap_err()
                    .to_string();
            assert!(error.contains("raw_hash"), "unexpected error: {error}");
        }
    }

    #[test]
    fn r23_cand_069_inline_json_rejects_malformed_hashes() {
        let malformed = [
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sha256:abcd",
            "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "sha256:gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg",
            "sha512:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ];

        for raw_hash in malformed {
            assert!(
                serde_json::from_value::<EvidencePointer>(inline_pointer_with_raw_hash(raw_hash))
                    .is_err(),
                "malformed hash was accepted: {raw_hash}"
            );
        }
    }

    #[test]
    fn r23_cand_069_inline_json_valid_control_exposes_only_validated_digest() {
        let encoded = serde_json::to_string(&pointer()).unwrap();
        let decoded: EvidencePointer = serde_json::from_str(&encoded).unwrap();

        let validated = decoded.validate().unwrap();

        assert_eq!(
            validated.raw_hash().digest(),
            "74bcb92d8088c950e45e4c43563332da2ca1e04b25d6d4016aa43f830d4cca8a"
        );
        assert_eq!(validated.raw_hash().fanout(), ("74", "bc"));
        assert_eq!(validated.raw_hash().as_str(), decoded.raw_hash.as_str());
    }

    #[test]
    fn r23_cand_069_inline_json_preserves_optional_field_compatibility() {
        let value = serde_json::json!({
            "schema_version": EVIDENCE_POINTER_SCHEMA_VERSION,
            "commit": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "raw_hash": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "tool_profile_hash": "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "chunk_hash": "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            "scope_id": "scope_01J8ZQABCDEFGHJKMNPQRS"
        });

        let decoded: EvidencePointer = serde_json::from_value(value).unwrap();

        assert!(decoded.tree.is_none());
        assert!(decoded.path_at_commit.is_none());
        assert!(decoded.scope_path.is_none());
    }

    #[test]
    fn issuer_normalizes_empty_heading_path_to_omission() {
        let issued = issue_evidence_pointer(issue_request(Some(Vec::new()))).unwrap();

        assert!(issued.heading_path.is_none());
        let encoded = serde_json::to_value(issued).unwrap();
        assert!(encoded.get("heading_path").is_none());
    }

    #[test]
    fn inline_json_rejects_explicit_empty_heading_path() {
        let mut value = serde_json::to_value(pointer()).unwrap();
        value["heading_path"] = serde_json::json!([]);

        let error = serde_json::from_value::<EvidencePointer>(value)
            .unwrap_err()
            .to_string();
        assert!(error.contains("heading_path must be omitted"));
    }

    #[test]
    fn inline_json_rejects_explicit_null_heading_path() {
        let mut value = serde_json::to_value(pointer()).unwrap();
        value["heading_path"] = serde_json::Value::Null;

        assert!(serde_json::from_value::<EvidencePointer>(value).is_err());
    }

    #[test]
    fn inline_json_rejects_empty_heading_path_entry() {
        let mut value = serde_json::to_value(pointer()).unwrap();
        value["heading_path"] = serde_json::json!([""]);

        let error = serde_json::from_value::<EvidencePointer>(value)
            .unwrap_err()
            .to_string();
        assert!(error.contains("heading_path entries must be non-empty"));
    }

    #[test]
    fn inline_json_rejects_more_than_64_heading_path_entries() {
        let mut value = serde_json::to_value(pointer()).unwrap();
        value["heading_path"] = serde_json::json!(vec!["heading"; 65]);

        let error = serde_json::from_value::<EvidencePointer>(value)
            .unwrap_err()
            .to_string();
        assert!(error.contains("at most 64 headings"));
    }

    #[test]
    fn inline_json_accepts_64_non_empty_heading_path_entries() {
        let mut value = serde_json::to_value(pointer()).unwrap();
        value["heading_path"] = serde_json::json!(vec!["heading"; 64]);

        let decoded: EvidencePointer = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.heading_path.as_ref().map(Vec::len), Some(64));
    }

    #[test]
    fn evidence_pointer_rejects_unknown_json_fields() {
        let mut value = serde_json::to_value(pointer()).unwrap();
        value["unexpected"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<EvidencePointer>(value).is_err());
    }

    #[test]
    fn r23_cand_069_all_pointer_hash_fields_share_the_validation_boundary() {
        for field in [
            "commit",
            "tree",
            "raw_hash",
            "tool_profile_hash",
            "chunk_hash",
        ] {
            let mut value = serde_json::to_value(pointer()).unwrap();
            value[field] = serde_json::Value::String("sha256:not-a-digest".to_owned());
            let error = serde_json::from_value::<EvidencePointer>(value)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(field),
                "unexpected error for {field}: {error}"
            );
        }
    }

    #[test]
    fn r23_cand_069_uri_parser_rejects_malformed_hash_segments() {
        let uri = evidence_pointer_to_uri(&pointer()).unwrap();
        let invalid = uri.replace(&pointer().raw_hash, "sha256:../../synthetic-marker.json");

        assert!(parse_evidence_pointer_uri(&invalid).is_err());
    }
}
