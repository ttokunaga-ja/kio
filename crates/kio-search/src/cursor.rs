//! Cursor token contracts for deterministic paging.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

use crate::query::{TimeTravelSelector, is_sha256_hash};
use crate::{Result, SearchError};

const CURSOR_VERSION: u64 = 3;
const MAX_CURSOR_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_CURSOR_TOKEN_BYTES: usize = 1_500_000;
const MAX_SCOPE_ID_BYTES: usize = 256;
const MAX_EXCLUSION_REASON_BYTES: usize = 128;
const MAX_SQLITE_ROWID: u64 = i64::MAX as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeMode {
    All,
    Scope,
    Descendants,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeCursor {
    pub scope_id: String,
    pub snapshot_commit: String,
    /// The current-policy observation which admitted this scope's bindings on
    /// page 1. A replay must reject a changed digest before it can reuse the
    /// frozen row and consumed boundaries.
    pub policy_digest: String,
    /// PC19/PC21 (05 §1.5): the scope's `index_metadata.index_generation` ULID at
    /// page-1 issuance. Replay re-reads the current value and rejects the cursor
    /// (`KIO-E-SEARCH-CURSOR-001`) on any mismatch — a rebuild/purge/enrichment
    /// finalize/tombstone-lifecycle update that changed this scope's index since
    /// invalidates the frozen `max_rowid`/`consumed` bookkeeping below.
    pub index_generation: String,
    pub max_rowid: u64,
    pub max_association_rowid: u64,
    pub chunking_config_hash: String,
    /// Hits already returned from this scope in the final, post-alias-expansion
    /// stream. This is deliberately not a semantic-chunk count.
    pub consumed: u64,
}

/// A page-1 exclusion retained inside the signed token. Excluded scopes never
/// become active participants if they later recover during the same stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CursorExcludedScope {
    pub scope_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CursorToken {
    #[serde(rename = "v")]
    pub version: u64,
    pub scope_mode: ScopeMode,
    pub query_hash: String,
    /// PC24 (05 §1.5/§1.8): the page-1 query vector's digest, a token-level field
    /// (not per-scope — one query embedding is shared by every participating
    /// scope, 05 §1.8 "送信は 1 回であり scope 別の再送信は発生しない"). Present
    /// only for a vector|hybrid page 1; omitted (not `null`) in text mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_vector_digest: Option<String>,
    /// R25 (05 §1.8 "replica 世代が cursor と一致する page N"): the
    /// `aggregator.sqlite` collection stamp that produced page 1's ranks, or
    /// absent when page 1 ranked without the replica.
    ///
    /// The per-scope `index_generation`s below pin each searched scope's ROWS,
    /// which is not enough: global BM25 also reads the collection's df/`N`/
    /// `avgdl`, so indexing a folder nobody searched moves the ranks of the
    /// ones they did while every per-scope stamp still matches. Both states
    /// have to be pinned — a page that ranked through the replica must replay
    /// against the same collection, and a page that did NOT must not suddenly
    /// start, or the two pages order the same stream differently and the
    /// boundary between them drops or repeats results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection_generation: Option<String>,
    pub time_travel: TimeTravelSelector,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_cutoff: Option<String>,
    pub excluded_scopes: Vec<CursorExcludedScope>,
    pub scopes: Vec<ScopeCursor>,
}

impl CursorToken {
    pub const VERSION: u64 = CURSOR_VERSION;

    /// Strictly validate the signed v3 structure after deserialization (and
    /// before emission). Array order is part of the deterministic paging state.
    pub fn validate(&self) -> Result<()> {
        self.validate_contract().map_err(SearchError::Cursor)
    }

    fn validate_contract(&self) -> std::result::Result<(), String> {
        if self.version != CURSOR_VERSION {
            return Err(format!(
                "unsupported cursor version {}; expected {CURSOR_VERSION}",
                self.version
            ));
        }
        if !is_sha256_hash(&self.query_hash) {
            return Err("query_hash must be sha256: plus 64 lowercase hex digits".to_owned());
        }
        if let Some(digest) = &self.query_vector_digest
            && !is_sha256_hash(digest)
        {
            return Err(
                "query_vector_digest must be sha256: plus 64 lowercase hex digits".to_owned(),
            );
        }
        if let Some(generation) = &self.collection_generation
            && !is_sha256_hash(generation)
        {
            return Err(
                "collection_generation must be sha256: plus 64 lowercase hex digits".to_owned(),
            );
        }
        self.time_travel
            .validate_contract()
            .map_err(|err| format!("invalid cursor selector: {err}"))?;
        match (&self.time_travel.since, &self.since_cutoff) {
            (Some(_), Some(cutoff)) => validate_utc_seconds(cutoff)?,
            (Some(_), None) => {
                return Err("since_cutoff is required when time_travel.since is set".to_owned());
            }
            (None, Some(_)) => {
                return Err("since_cutoff is only valid when time_travel.since is set".to_owned());
            }
            (None, None) => {}
        }

        if self.scopes.is_empty() {
            return Err("cursor must contain at least one active scope".to_owned());
        }
        let mut active_scope_ids = BTreeSet::new();
        let mut previous_scope_id: Option<&str> = None;
        for scope in &self.scopes {
            validate_cursor_string("scopes.scope_id", &scope.scope_id, MAX_SCOPE_ID_BYTES)?;
            if previous_scope_id.is_some_and(|previous| previous >= scope.scope_id.as_str()) {
                return Err("cursor scopes must be strictly sorted by scope_id".to_owned());
            }
            previous_scope_id = Some(&scope.scope_id);
            active_scope_ids.insert(scope.scope_id.as_str());
            if !is_sha256_hash(&scope.snapshot_commit) {
                return Err(
                    "snapshot_commit must be sha256: plus 64 lowercase hex digits".to_owned(),
                );
            }
            if !is_sha256_hash(&scope.policy_digest) {
                return Err("policy_digest must be sha256: plus 64 lowercase hex digits".to_owned());
            }
            if !is_sha256_hash(&scope.chunking_config_hash) {
                return Err(
                    "chunking_config_hash must be sha256: plus 64 lowercase hex digits".to_owned(),
                );
            }
            // PC19: `index_generation` is an opaque ULID string minted by the index
            // layer (`kio_index::fts::index_metadata`) — bounded/control-free like
            // every other cursor string, but not a sha256 digest.
            validate_cursor_string("scopes.index_generation", &scope.index_generation, 64)?;
            if scope.max_rowid > MAX_SQLITE_ROWID
                || scope.max_association_rowid > MAX_SQLITE_ROWID
                || scope.consumed > MAX_SQLITE_ROWID
            {
                return Err("cursor rowid/consumed value is out of range".to_owned());
            }
        }

        let mut previous_excluded_scope_id: Option<&str> = None;
        for excluded in &self.excluded_scopes {
            validate_cursor_string(
                "excluded_scopes.scope_id",
                &excluded.scope_id,
                MAX_SCOPE_ID_BYTES,
            )?;
            validate_cursor_string(
                "excluded_scopes.reason",
                &excluded.reason,
                MAX_EXCLUSION_REASON_BYTES,
            )?;
            if previous_excluded_scope_id
                .is_some_and(|previous| previous >= excluded.scope_id.as_str())
            {
                return Err("excluded_scopes must be strictly sorted by scope_id".to_owned());
            }
            previous_excluded_scope_id = Some(&excluded.scope_id);
            if active_scope_ids.contains(excluded.scope_id.as_str()) {
                return Err("a scope cannot be both active and excluded".to_owned());
            }
        }
        Ok(())
    }
}

/// Serialize + HMAC-sign a cursor into `<base64url(JCS)>.<base64url(HMAC-SHA256)>`
/// (O1(b)). The signature is computed over the canonical JCS bytes with a
/// device-local key so a caller cannot forge or tamper a token to jump scope or
/// page — `query_hash` alone binds only public inputs. The inner payload stays
/// exactly the previous `base64url(JCS(token))`, so the wire form is still
/// URL-safe and pad-free; only the `.signature` suffix is new.
pub fn encode_cursor_token(token: &CursorToken, key: &[u8]) -> Result<String> {
    token.validate()?;
    let value = serde_json::to_value(token).map_err(|err| SearchError::Cursor(err.to_string()))?;
    let bytes = serde_jcs::to_vec(&value).map_err(|err| SearchError::Cursor(err.to_string()))?;
    if bytes.len() > MAX_CURSOR_PAYLOAD_BYTES {
        return Err(SearchError::Cursor(
            "cursor payload exceeds size limit".to_owned(),
        ));
    }
    let signature = hmac_sha256(key, &bytes);
    Ok(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&bytes),
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

/// Verify the HMAC signature, then decode the cursor. A missing or mismatched
/// signature is `SearchError::Cursor` (the CLI maps it to
/// `KIO-E-SEARCH-CURSOR-001`), so a forged / tampered token is rejected before
/// its frozen scope set is ever trusted.
pub fn decode_cursor_token(token: &str, key: &[u8]) -> Result<CursorToken> {
    if token.len() > MAX_CURSOR_TOKEN_BYTES {
        return Err(SearchError::Cursor(
            "cursor token exceeds size limit".to_owned(),
        ));
    }
    let (payload_b64, signature_b64) = token
        .rsplit_once('.')
        .ok_or_else(|| SearchError::Cursor("cursor is missing its signature".to_owned()))?;
    if payload_b64.len() > MAX_CURSOR_TOKEN_BYTES || signature_b64.len() != 43 {
        return Err(SearchError::Cursor(
            "cursor payload or signature length is invalid".to_owned(),
        ));
    }
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|err| SearchError::Cursor(err.to_string()))?;
    if payload.len() > MAX_CURSOR_PAYLOAD_BYTES {
        return Err(SearchError::Cursor(
            "cursor payload exceeds size limit".to_owned(),
        ));
    }
    let provided = URL_SAFE_NO_PAD
        .decode(signature_b64)
        .map_err(|err| SearchError::Cursor(err.to_string()))?;
    let expected = hmac_sha256(key, &payload);
    if !constant_time_eq(&expected, &provided) {
        return Err(SearchError::Cursor("cursor signature mismatch".to_owned()));
    }
    let decoded: CursorToken =
        serde_json::from_slice(&payload).map_err(|err| SearchError::Cursor(err.to_string()))?;
    let value =
        serde_json::to_value(&decoded).map_err(|err| SearchError::Cursor(err.to_string()))?;
    let canonical =
        serde_jcs::to_vec(&value).map_err(|err| SearchError::Cursor(err.to_string()))?;
    if canonical != payload {
        return Err(SearchError::Cursor(
            "cursor payload is not canonical JCS".to_owned(),
        ));
    }
    decoded.validate()?;
    Ok(decoded)
}

fn validate_cursor_string(
    field: &str,
    value: &str,
    max_bytes: usize,
) -> std::result::Result<(), String> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(format!(
            "{field} must be non-empty, control-free, and at most {max_bytes} bytes"
        ));
    }
    Ok(())
}

fn validate_utc_seconds(value: &str) -> std::result::Result<(), String> {
    let bytes = value.as_bytes();
    let separators_are_valid = bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z';
    if !separators_are_valid
        || bytes.iter().enumerate().any(|(index, byte)| {
            !matches!(index, 4 | 7 | 10 | 13 | 16 | 19) && !byte.is_ascii_digit()
        })
    {
        return Err("since_cutoff must be canonical UTC seconds (YYYY-MM-DDTHH:MM:SSZ)".to_owned());
    }
    let number = |start: usize, end: usize| -> u32 {
        bytes[start..end]
            .iter()
            .fold(0, |value, byte| value * 10 + u32::from(byte - b'0'))
    };
    let year = number(0, 4);
    let month = number(5, 7);
    let day = number(8, 10);
    let hour = number(11, 13);
    let minute = number(14, 16);
    let second = number(17, 19);
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => 0,
    };
    if year == 0 || day == 0 || day > max_day || hour > 23 || minute > 59 || second > 59 {
        return Err("since_cutoff is not a valid canonical UTC timestamp".to_owned());
    }
    Ok(())
}

/// HMAC-SHA256 (RFC 2104) over `sha2`, avoiding a new dependency. Block size is
/// 64 bytes (SHA-256).
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block_key = [0u8; BLOCK];
    if key.len() > BLOCK {
        block_key[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= block_key[i];
        opad[i] ^= block_key[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

/// Length-independent, branch-flat equality so signature verification does not
/// leak timing information about the expected MAC.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn hash(fill: char) -> String {
        format!("sha256:{}", fill.to_string().repeat(64))
    }

    fn sample_token() -> CursorToken {
        CursorToken {
            version: CursorToken::VERSION,
            scope_mode: ScopeMode::All,
            query_hash: hash('e'),
            query_vector_digest: None,
            collection_generation: None,
            time_travel: TimeTravelSelector::default(),
            since_cutoff: None,
            excluded_scopes: vec![CursorExcludedScope {
                scope_id: "scope_00".to_owned(),
                reason: "unreachable".to_owned(),
            }],
            scopes: vec![ScopeCursor {
                scope_id: "scope_01".to_owned(),
                snapshot_commit: hash('a'),
                policy_digest: hash('b'),
                index_generation: "01J8ZQEXAMPLEGENERATION0".to_owned(),
                max_rowid: 42,
                max_association_rowid: 48,
                chunking_config_hash: hash('c'),
                consumed: 20,
            }],
        }
    }

    fn signed_payload(payload: &[u8], key: &[u8]) -> String {
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(hmac_sha256(key, payload))
        )
    }

    fn signed_json(value: &Value, key: &[u8]) -> String {
        signed_payload(&serde_jcs::to_vec(value).unwrap(), key)
    }

    #[test]
    fn ct3_cursor_004_cursor_is_base64url_jcs_json() {
        let token = sample_token();
        let key = b"cursor-round-trip-key";
        let encoded = encode_cursor_token(&token, key).unwrap();
        assert_eq!(decode_cursor_token(&encoded, key).unwrap(), token);
        assert!(!encoded.contains('='));
    }

    #[test]
    fn o1_tampered_or_mis_keyed_cursor_is_rejected() {
        let token = sample_token();
        let key = b"device-key-a";
        let encoded = encode_cursor_token(&token, key).unwrap();
        // A different device key fails signature verification.
        assert!(decode_cursor_token(&encoded, b"device-key-b").is_err());
        // Flipping a payload character (forging a scope jump) also fails.
        let (payload, signature) = encoded.rsplit_once('.').unwrap();
        let mut forged = payload.to_owned();
        let last = forged.pop().unwrap();
        forged.push(if last == 'A' { 'B' } else { 'A' });
        let forged = format!("{forged}.{signature}");
        assert!(decode_cursor_token(&forged, key).is_err());
    }

    #[test]
    fn excluded_scopes_are_covered_by_the_signature() {
        let token = sample_token();
        let key = b"cursor-exclusions-key";
        let encoded = encode_cursor_token(&token, key).unwrap();
        let (payload_b64, original_signature) = encoded.rsplit_once('.').unwrap();
        let payload = URL_SAFE_NO_PAD.decode(payload_b64).unwrap();
        let mut value: Value = serde_json::from_slice(&payload).unwrap();
        value["excluded_scopes"][0]["reason"] = json!("recovered");
        let forged_payload = serde_jcs::to_vec(&value).unwrap();
        let forged = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(forged_payload),
            original_signature
        );
        assert!(decode_cursor_token(&forged, key).is_err());
    }

    #[test]
    fn strict_decode_rejects_legacy_unknown_or_incomplete_v3_payloads() {
        let key = b"strict-v3-key";
        let mut value = serde_json::to_value(sample_token()).unwrap();

        value["v"] = json!(2);
        assert!(decode_cursor_token(&signed_json(&value, key), key).is_err());

        value["v"] = json!(4);
        assert!(decode_cursor_token(&signed_json(&value, key), key).is_err());

        value["v"] = json!(3);
        value.as_object_mut().unwrap().remove("time_travel");
        assert!(decode_cursor_token(&signed_json(&value, key), key).is_err());

        for required_scope_field in [
            "policy_digest",
            "max_association_rowid",
            "chunking_config_hash",
        ] {
            value = serde_json::to_value(sample_token()).unwrap();
            value["scopes"][0]
                .as_object_mut()
                .unwrap()
                .remove(required_scope_field);
            assert!(decode_cursor_token(&signed_json(&value, key), key).is_err());
        }

        value = serde_json::to_value(sample_token()).unwrap();
        value["unexpected"] = json!(true);
        assert!(decode_cursor_token(&signed_json(&value, key), key).is_err());
    }

    #[test]
    fn strict_decode_rejects_noncanonical_json_even_with_a_valid_signature() {
        let key = b"canonical-key";
        let value = serde_json::to_value(sample_token()).unwrap();
        let mut payload = vec![b' '];
        payload.extend(serde_jcs::to_vec(&value).unwrap());
        assert!(decode_cursor_token(&signed_payload(&payload, key), key).is_err());
    }

    #[test]
    fn cursor_requires_canonical_since_cutoff_and_selector_consistency() {
        let mut token = sample_token();
        token.time_travel = TimeTravelSelector {
            all_history: true,
            since: Some("604800s".to_owned()),
            ..TimeTravelSelector::default()
        };
        assert!(token.validate().is_err());

        token.since_cutoff = Some("2026-07-13T00:00:00Z".to_owned());
        token.validate().unwrap();

        token.since_cutoff = Some("2026-02-29T00:00:00Z".to_owned());
        assert!(token.validate().is_err());

        token.time_travel = TimeTravelSelector::default();
        assert!(token.validate().is_err());
    }

    #[test]
    fn cursor_rejects_unsorted_overlapping_or_out_of_range_scope_state() {
        let mut token = sample_token();
        token.scopes.push(ScopeCursor {
            scope_id: "scope_00".to_owned(),
            snapshot_commit: hash('b'),
            policy_digest: hash('c'),
            index_generation: "01J8ZQEXAMPLEGENERATION1".to_owned(),
            max_rowid: 1,
            max_association_rowid: 1,
            chunking_config_hash: hash('d'),
            consumed: 0,
        });
        assert!(token.validate().is_err());

        let mut token = sample_token();
        token.excluded_scopes[0].scope_id = "scope_01".to_owned();
        assert!(token.validate().is_err());

        let mut token = sample_token();
        token.scopes[0].max_association_rowid = i64::MAX as u64 + 1;
        assert!(token.validate().is_err());
    }

    /// `policy_digest` and `index_generation` are required per-scope fields —
    /// a v3 cursor without either (e.g. a hand-forged payload, or a prior token
    /// shape) is rejected the same way a missing `chunking_config_hash` already
    /// is (`strict_decode_rejects_legacy_unknown_or_incomplete_v3_payloads`).
    #[test]
    fn policy_digest_and_index_generation_are_required_and_nonempty() {
        let mut token = sample_token();
        token.scopes[0].policy_digest = String::new();
        assert!(token.validate().is_err());

        let mut token = sample_token();
        token.scopes[0].index_generation = String::new();
        assert!(token.validate().is_err());

        let key = b"pc19-key";
        let mut value = serde_json::to_value(sample_token()).unwrap();
        value["scopes"][0]
            .as_object_mut()
            .unwrap()
            .remove("index_generation");
        assert!(decode_cursor_token(&signed_json(&value, key), key).is_err());

        let mut value = serde_json::to_value(sample_token()).unwrap();
        value["scopes"][0]
            .as_object_mut()
            .unwrap()
            .remove("policy_digest");
        assert!(decode_cursor_token(&signed_json(&value, key), key).is_err());
    }

    /// PC24/PC27: a top-level `query_vector_digest` round-trips for a vector page
    /// 1 and is rejected if malformed; a text-mode token (the pre-PC24 shape,
    /// field omitted) still round-trips unchanged.
    #[test]
    fn pc24_query_vector_digest_round_trips_and_validates() {
        let key = b"pc24-key";
        let mut token = sample_token();
        token.query_vector_digest = Some(hash('f'));
        let encoded = encode_cursor_token(&token, key).unwrap();
        assert_eq!(decode_cursor_token(&encoded, key).unwrap(), token);

        token.query_vector_digest = Some("not-a-digest".to_owned());
        assert!(token.validate().is_err());

        // text mode (field omitted): unchanged round trip, matching every
        // pre-PC24 cursor already in the field.
        let text_mode = sample_token();
        assert_eq!(text_mode.query_vector_digest, None);
        let encoded_text = encode_cursor_token(&text_mode, key).unwrap();
        assert_eq!(decode_cursor_token(&encoded_text, key).unwrap(), text_mode);
    }
}
