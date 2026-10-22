//! Search request and AI Agent response contracts.

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::{Result, SearchError};

const SHA256_PREFIX: &str = "sha256:";
const SHA256_HEX_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Auto,
    Text,
    Image,
    Vector,
    Hybrid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeSelectionMode {
    All,
    Scope,
    Descendants,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiversifyStrategy {
    Mmr,
    GroupByRawHash,
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiversifyRequest {
    pub strategy: DiversifyStrategy,
    pub mmr_lambda: Option<f64>,
    pub max_per_raw_hash: Option<u64>,
    pub mmr_depth: Option<u64>,
}

/// The effective chunking generation for one participating scope.
///
/// Query identity binds the complete per-scope mapping. This prevents a cursor
/// from being replayed after only one scope changes its effective generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkingConfigBinding {
    pub scope_id: String,
    pub chunking_config_hash: String,
}

/// Canonical time-travel selector shared by query hashing and cursor payloads.
///
/// Default-valued fields are absent on the wire, so the default selector is
/// exactly `{}`. Callers canonicalize durations to positive whole seconds
/// (`604800s`, for example) and set `all_history` whenever `since` is present.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeTravelSelector {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub all_history: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub include_deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

impl TimeTravelSelector {
    /// Validate that this is an already-canonical effective selector.
    pub fn validate(&self) -> Result<()> {
        self.validate_contract().map_err(SearchError::Contract)
    }

    pub(crate) fn validate_contract(&self) -> std::result::Result<(), String> {
        if let Some(at) = &self.at {
            validate_bounded_nonempty("time_travel.at", at, 1_024)?;
            if self.all_history || self.include_deleted || self.since.is_some() {
                return Err("time_travel.at conflicts with history selectors".to_owned());
            }
        }
        if self.include_deleted && (self.all_history || self.since.is_some()) {
            return Err("time_travel.include_deleted conflicts with all_history/since".to_owned());
        }
        if let Some(since) = &self.since {
            if !self.all_history {
                return Err("time_travel.since requires all_history=true".to_owned());
            }
            validate_canonical_seconds(since)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryHashInput {
    pub query: String,
    pub mode: SearchMode,
    pub scope_mode: ScopeSelectionMode,
    pub scopes: Vec<String>,
    pub diversify: DiversifyRequest,
    pub rrf_k: u64,
    pub rrf_candidate_depth: u64,
    pub rrf_w_text: f64,
    pub rrf_w_vector: f64,
    pub chunking_configs: Vec<ChunkingConfigBinding>,
    pub time_travel: TimeTravelSelector,
    /// PC24 (05 §1.5/§1.8): the page-1 query vector's digest, included in the hash
    /// preimage only when the effective mode is vector|hybrid. `None` in text mode
    /// (and for every pre-PC24 caller), which omits the JSON key entirely so the
    /// hash of an unrelated (text-mode) query is byte-for-byte unchanged from
    /// before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_vector_digest: Option<String>,
}

pub fn query_hash(input: &QueryHashInput) -> Result<String> {
    input.time_travel.validate()?;
    let mut scopes = input.scopes.clone();
    scopes.sort();
    validate_unique_scopes(&scopes)?;

    let mut chunking_configs = input.chunking_configs.clone();
    chunking_configs.sort_by(|a, b| a.scope_id.cmp(&b.scope_id));
    validate_chunking_configs(&scopes, &chunking_configs)?;

    if let Some(digest) = &input.query_vector_digest
        && !is_sha256_hash(digest)
    {
        return Err(SearchError::Contract(
            "query_vector_digest must be sha256: plus 64 lowercase hex digits".to_owned(),
        ));
    }

    let time_travel = serde_json::to_value(&input.time_travel)
        .map_err(|err| SearchError::Contract(err.to_string()))?;
    let mut value = json!({
        "chunking_configs": chunking_configs,
        "diversify": {
            "enabled": input.diversify.strategy != DiversifyStrategy::Off,
            "max_per_raw_hash": input.diversify.max_per_raw_hash.unwrap_or(3),
            "mmr_depth": input.diversify.mmr_depth.unwrap_or(100),
            "mmr_lambda": input.diversify.mmr_lambda.unwrap_or(0.7),
            "strategy": strategy_name(input.diversify.strategy),
        },
        "mode": search_mode_name(input.mode),
        "query": input.query.nfc().collect::<String>(),
        "rrf": {
            "candidate_depth": input.rrf_candidate_depth,
            "k": input.rrf_k,
            "w_text": input.rrf_w_text,
            "w_vector": input.rrf_w_vector,
        },
        "scope_mode": scope_mode_name(input.scope_mode),
        "scopes": scopes,
        "time_travel": time_travel,
    });
    // PC24: the key itself is absent (not `null`) in text mode, so a text-mode
    // hash preimage is byte-identical to before this field was added.
    if let Some(digest) = &input.query_vector_digest {
        value
            .as_object_mut()
            .expect("value is always a JSON object")
            .insert("query_vector_digest".to_owned(), json!(digest));
    }
    let bytes = serde_jcs::to_vec(&value).map_err(|err| SearchError::Contract(err.to_string()))?;
    Ok(hash_bytes(&bytes))
}

fn validate_unique_scopes(scopes: &[String]) -> Result<()> {
    for scope_id in scopes {
        validate_bounded_nonempty("scope_id", scope_id, 256).map_err(SearchError::Contract)?;
    }
    if scopes.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(SearchError::Contract(
            "scopes must contain unique scope_ids".to_owned(),
        ));
    }
    Ok(())
}

fn validate_chunking_configs(
    scopes: &[String],
    chunking_configs: &[ChunkingConfigBinding],
) -> Result<()> {
    for binding in chunking_configs {
        validate_bounded_nonempty("chunking_configs.scope_id", &binding.scope_id, 256)
            .map_err(SearchError::Contract)?;
        if !is_sha256_hash(&binding.chunking_config_hash) {
            return Err(SearchError::Contract(
                "chunking_config_hash must be sha256: plus 64 lowercase hex digits".to_owned(),
            ));
        }
    }
    if chunking_configs
        .windows(2)
        .any(|pair| pair[0].scope_id == pair[1].scope_id)
    {
        return Err(SearchError::Contract(
            "chunking_configs must contain exactly one binding per scope".to_owned(),
        ));
    }
    let binding_scopes: Vec<&str> = chunking_configs
        .iter()
        .map(|binding| binding.scope_id.as_str())
        .collect();
    if !scopes
        .iter()
        .map(String::as_str)
        .eq(binding_scopes.iter().copied())
    {
        return Err(SearchError::Contract(
            "chunking_configs must bind every participating scope exactly once".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn is_sha256_hash(value: &str) -> bool {
    let Some(digest) = value.strip_prefix(SHA256_PREFIX) else {
        return false;
    };
    digest.len() == SHA256_HEX_LEN
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_bounded_nonempty(
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

fn validate_canonical_seconds(value: &str) -> std::result::Result<(), String> {
    let Some(digits) = value.strip_suffix('s') else {
        return Err(
            "time_travel.since must be canonical positive seconds (for example 604800s)".to_owned(),
        );
    };
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
        || digits
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds > 0)
            .is_none()
    {
        return Err(
            "time_travel.since must be canonical positive seconds (for example 604800s)".to_owned(),
        );
    }
    Ok(())
}

fn is_false(value: &bool) -> bool {
    !*value
}

pub fn search_mode_name(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::Auto => "auto",
        SearchMode::Text => "text",
        SearchMode::Image => "image",
        SearchMode::Vector => "vector",
        SearchMode::Hybrid => "hybrid",
    }
}

pub fn scope_mode_name(mode: ScopeSelectionMode) -> &'static str {
    match mode {
        ScopeSelectionMode::All => "all",
        ScopeSelectionMode::Scope => "scope",
        ScopeSelectionMode::Descendants => "descendants",
    }
}

pub fn strategy_name(strategy: DiversifyStrategy) -> &'static str {
    match strategy {
        DiversifyStrategy::Mmr => "mmr",
        DiversifyStrategy::GroupByRawHash => "group_by_raw_hash",
        DiversifyStrategy::Off => "off",
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", lower_hex(&digest))
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(scope_id: &str, fill: char) -> ChunkingConfigBinding {
        ChunkingConfigBinding {
            scope_id: scope_id.to_owned(),
            chunking_config_hash: format!("sha256:{}", fill.to_string().repeat(64)),
        }
    }

    fn input(
        scope_mode: ScopeSelectionMode,
        scopes: Vec<&str>,
        time_travel: TimeTravelSelector,
    ) -> QueryHashInput {
        QueryHashInput {
            query: "認証仕様".to_owned(),
            mode: SearchMode::Text,
            scope_mode,
            chunking_configs: scopes
                .iter()
                .map(|scope_id| config(scope_id, 'c'))
                .collect(),
            scopes: scopes.into_iter().map(str::to_owned).collect(),
            diversify: DiversifyRequest {
                strategy: DiversifyStrategy::Mmr,
                mmr_lambda: Some(0.7),
                max_per_raw_hash: Some(3),
                mmr_depth: Some(100),
            },
            rrf_k: 60,
            rrf_candidate_depth: 200,
            rrf_w_text: 1.0,
            rrf_w_vector: 1.0,
            time_travel,
            query_vector_digest: None,
        }
    }

    #[test]
    fn ct3_cursor_003_query_hash_vector() {
        let hash = query_hash(&QueryHashInput {
            query: "認証仕様".to_owned(),
            mode: SearchMode::Hybrid,
            scope_mode: ScopeSelectionMode::All,
            scopes: vec![
                "scope_01K3ABCDEFGHJKMNPQRSTV".to_owned(),
                "scope_01J8ZQABCDEFGHJKMNPQRS".to_owned(),
            ],
            diversify: DiversifyRequest {
                strategy: DiversifyStrategy::Mmr,
                mmr_lambda: Some(0.7),
                max_per_raw_hash: Some(3),
                mmr_depth: Some(100),
            },
            rrf_k: 60,
            rrf_candidate_depth: 200,
            rrf_w_text: 1.0,
            rrf_w_vector: 1.0,
            chunking_configs: vec![
                config("scope_01K3ABCDEFGHJKMNPQRSTV", 'c'),
                config("scope_01J8ZQABCDEFGHJKMNPQRS", 'c'),
            ],
            time_travel: TimeTravelSelector::default(),
            query_vector_digest: None,
        })
        .unwrap();
        assert_eq!(
            hash,
            "sha256:bfd9387844c90e9d7f58ac8c9b0775c8fd20d4a2ee01936785795375dd2a93aa"
        );
    }

    /// PC24/PC27: `query_vector_digest` changes the hash only when present (a
    /// vector|hybrid page 1), and text-mode callers that never set it get the
    /// exact pre-PC24 hash (the pinned vector above stays green with the field
    /// defaulted to `None`).
    #[test]
    fn pc24_query_vector_digest_changes_hash_only_when_present() {
        let base = input(
            ScopeSelectionMode::All,
            vec!["scope_a", "scope_b"],
            TimeTravelSelector::default(),
        );
        let mut with_digest = base.clone();
        with_digest.mode = SearchMode::Hybrid;
        with_digest.query_vector_digest = Some(format!("sha256:{}", "a".repeat(64)));
        assert_ne!(
            query_hash(&base).unwrap(),
            query_hash(&with_digest).unwrap()
        );

        // Two different digests (e.g. two different query embeddings) hash
        // differently — the digest is a real component of the preimage, not a
        // decorative flag.
        let mut other_digest = with_digest.clone();
        other_digest.query_vector_digest = Some(format!("sha256:{}", "b".repeat(64)));
        assert_ne!(
            query_hash(&with_digest).unwrap(),
            query_hash(&other_digest).unwrap()
        );

        // Malformed digests are rejected rather than silently hashed as opaque
        // strings (keeps the cursor's replay-time re-derivation trustworthy).
        let mut malformed = with_digest.clone();
        malformed.query_vector_digest = Some("not-a-digest".to_owned());
        assert!(query_hash(&malformed).is_err());
    }

    #[test]
    fn ct4_timetravel_query_hash_vectors() {
        let all_history = query_hash(&input(
            ScopeSelectionMode::All,
            vec!["scope_b", "scope_a"],
            TimeTravelSelector {
                all_history: true,
                ..TimeTravelSelector::default()
            },
        ))
        .unwrap();
        assert_eq!(
            all_history,
            "sha256:8b3f6fedd0376e1dd0fb02efb0b9ea1f34f1088465a2d9ad4f83e1307b7053f1"
        );

        let at = query_hash(&input(
            ScopeSelectionMode::Scope,
            vec!["scope_a"],
            TimeTravelSelector {
                at: Some(format!("sha256:{}", "a".repeat(64))),
                ..TimeTravelSelector::default()
            },
        ))
        .unwrap();
        assert_eq!(
            at,
            "sha256:8895f616f97776f376cd26d3210f5fa2e00b1f57dc748b115b8e0e0d3670d962"
        );

        let since = query_hash(&input(
            ScopeSelectionMode::All,
            vec!["scope_a", "scope_b"],
            TimeTravelSelector {
                all_history: true,
                since: Some("604800s".to_owned()),
                ..TimeTravelSelector::default()
            },
        ))
        .unwrap();
        assert_eq!(
            since,
            "sha256:df768d2bc941daab1d43321b3739052905ca148b1bf884b7dc9cd6b5a88144e4"
        );
    }

    #[test]
    fn query_hash_sorts_scope_and_config_bindings() {
        let mut unsorted = input(
            ScopeSelectionMode::All,
            vec!["scope_b", "scope_a"],
            TimeTravelSelector::default(),
        );
        unsorted.chunking_configs.reverse();
        let sorted = input(
            ScopeSelectionMode::All,
            vec!["scope_a", "scope_b"],
            TimeTravelSelector::default(),
        );
        assert_eq!(query_hash(&unsorted).unwrap(), query_hash(&sorted).unwrap());
    }

    #[test]
    fn query_hash_changes_when_one_scopes_effective_config_changes() {
        let baseline = input(
            ScopeSelectionMode::All,
            vec!["scope_a", "scope_b"],
            TimeTravelSelector::default(),
        );
        let mut changed = baseline.clone();
        changed.chunking_configs[1].chunking_config_hash = format!("sha256:{}", "d".repeat(64));
        assert_ne!(
            query_hash(&baseline).unwrap(),
            query_hash(&changed).unwrap()
        );
    }

    #[test]
    fn query_hash_rejects_incomplete_or_duplicate_config_mapping() {
        let mut missing = input(
            ScopeSelectionMode::All,
            vec!["scope_a", "scope_b"],
            TimeTravelSelector::default(),
        );
        missing.chunking_configs.pop();
        assert!(query_hash(&missing).is_err());

        let mut duplicate = input(
            ScopeSelectionMode::All,
            vec!["scope_a", "scope_b"],
            TimeTravelSelector::default(),
        );
        duplicate.chunking_configs[1].scope_id = "scope_a".to_owned();
        assert!(query_hash(&duplicate).is_err());
    }

    #[test]
    fn selector_serializes_canonically_and_rejects_noncanonical_combinations() {
        assert_eq!(
            serde_jcs::to_string(&TimeTravelSelector::default()).unwrap(),
            "{}"
        );
        let since = TimeTravelSelector {
            all_history: true,
            since: Some("604800s".to_owned()),
            ..TimeTravelSelector::default()
        };
        assert_eq!(
            serde_jcs::to_string(&since).unwrap(),
            r#"{"all_history":true,"since":"604800s"}"#
        );
        since.validate().unwrap();

        for selector in [
            TimeTravelSelector {
                at: Some("HEAD".to_owned()),
                all_history: true,
                ..TimeTravelSelector::default()
            },
            TimeTravelSelector {
                include_deleted: true,
                all_history: true,
                ..TimeTravelSelector::default()
            },
            TimeTravelSelector {
                since: Some("7d".to_owned()),
                all_history: true,
                ..TimeTravelSelector::default()
            },
            TimeTravelSelector {
                since: Some("604800s".to_owned()),
                ..TimeTravelSelector::default()
            },
        ] {
            assert!(selector.validate().is_err());
        }
    }
}
