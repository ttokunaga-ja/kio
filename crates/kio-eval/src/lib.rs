//! Deterministic primitives owned by Kio's internal evaluation binary.

pub mod acceptance;
pub mod acceptance_authenticated_local;
pub mod acceptance_distribution;
pub(crate) mod acceptance_environment;
pub mod acceptance_failure;
pub mod acceptance_fault;
pub mod acceptance_local_trust;
pub mod acceptance_native;
pub mod acceptance_office;
pub mod acceptance_policy;
pub mod acceptance_preflight;
pub mod acceptance_provider;
pub mod acceptance_service;
pub mod acceptance_tools;
pub mod artifact;
pub mod attestation;
pub mod boundary;
pub mod crossscope;
pub mod d1;
pub mod fixture_register;
pub mod generator;
pub mod manifest;
pub mod ocr_eval;
pub mod persona_artifact;
pub mod persona_attest;
pub mod persona_consumer;
pub mod persona_lease;
pub mod persona_manifest;
pub mod persona_materialize;
pub mod persona_plan;
pub mod persona_render;
pub mod persona_render_artifact;
pub mod persona_scaffold;
pub mod persona_schedule;
pub(crate) mod process_boundary;
pub mod provider_budget;
pub mod python_exceptions;
pub mod qhard;
pub mod release;
pub mod replay;
pub mod replay_boundary;
pub mod rerank;
pub mod resolver;
pub mod runner;
pub mod scale_attest;
pub mod scale_benchmark;
pub mod scale_fixture;
pub mod scale_prepare;
pub mod scale_spec;
pub mod u7;

use std::collections::HashSet;

use kio_index::chunking::slugify_heading;
use thiserror::Error;

/// A resolved result identity: raw object, section leaf, and path at the commit.
pub type ResultKey = (String, Option<String>, Option<String>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallResult {
    pub raw_hash: String,
    pub section_id: Option<String>,
    pub heading_path: Option<Vec<String>>,
    pub path_at_commit: Option<String>,
}

impl RecallResult {
    #[must_use]
    pub fn key(&self) -> ResultKey {
        let section = self
            .section_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .and_then(|value| value.rsplit('/').next())
            .map(ToOwned::to_owned)
            .or_else(|| {
                self.heading_path
                    .as_ref()
                    .and_then(|path| path.last())
                    .map(|heading| slugify_heading(heading))
            });
        (self.raw_hash.clone(), section, self.path_at_commit.clone())
    }
}

/// Recall@k over distinct `(raw_hash, section leaf, path_at_commit)` results.
#[must_use]
pub fn recall_at_k(results: &[RecallResult], expected: &HashSet<ResultKey>, k: usize) -> f64 {
    if expected.is_empty() {
        return 0.0;
    }
    let got: HashSet<_> = results
        .iter()
        .take(k)
        .filter(|result| !result.raw_hash.is_empty())
        .map(RecallResult::key)
        .collect();
    (got.intersection(expected).count() as f64) / (expected.len() as f64)
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("percentile must be in the interval (0, 1]")]
pub struct PercentileError;

/// Nearest-rank percentile. Empty samples have no percentile.
///
/// Percentiles outside `(0, 1]` are invalid rather than indistinguishable
/// from an empty sample.
pub fn percentile_nearest_rank<T: Ord + Copy>(
    values: &[T],
    percentile: f64,
) -> Result<Option<T>, PercentileError> {
    if !(0.0 < percentile && percentile <= 1.0) {
        return Err(PercentileError);
    }
    if values.is_empty() {
        return Ok(None);
    }
    let mut ordered = values.to_vec();
    ordered.sort_unstable();
    let rank = (percentile * ordered.len() as f64).ceil().max(1.0) as usize;
    Ok(ordered.get(rank.saturating_sub(1)).copied())
}

#[cfg(test)]
mod golden_vectors {
    use std::{collections::HashSet, fs, path::PathBuf};

    use kio_core::cas::{ChunkObject, canonical_json_bytes, hash_bytes};
    use serde::Deserialize;
    use serde_json::Value;

    use super::{RecallResult, ResultKey, percentile_nearest_rank, recall_at_k};

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Vectors {
        schema_version: u64,
        canonical_json: Vec<CanonicalCase>,
        slugs: Vec<SlugCase>,
        chunk_identity: Vec<ChunkCase>,
        recall: Vec<RecallCase>,
        percentiles: Vec<PercentileCase>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CanonicalCase {
        name: String,
        value: Value,
        canonical_utf8: String,
        sha256: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SlugCase {
        name: String,
        input: String,
        expected: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ChunkCase {
        name: String,
        chunk: ChunkObject,
        expected_hash: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RecallCase {
        name: String,
        expected: Vec<ResultKey>,
        results: Vec<RecallVectorResult>,
        k: usize,
        expected_recall: f64,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RecallVectorResult {
        raw_hash: String,
        section_id: Option<String>,
        heading_path: Option<Vec<String>>,
        path_at_commit: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PercentileCase {
        name: String,
        values: Vec<u64>,
        percentile: f64,
        expected: Option<u64>,
    }

    fn vectors() -> Vectors {
        parse_frozen_vectors(&frozen_vector_bytes()).unwrap()
    }

    fn frozen_vector_bytes() -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../eval/golden-vectors.json");
        fs::read(path).unwrap()
    }

    fn parse_frozen_vectors(bytes: &[u8]) -> Result<Vectors, String> {
        let vectors: Vectors = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if vectors.schema_version != 1 {
            return Err("unsupported golden vector schema_version".to_owned());
        }
        Ok(vectors)
    }

    fn replace_once(bytes: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
        let offset = bytes
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("frozen vector fixture contains replacement target");
        let mut replaced = Vec::with_capacity(bytes.len() - needle.len() + replacement.len());
        replaced.extend_from_slice(&bytes[..offset]);
        replaced.extend_from_slice(replacement);
        replaced.extend_from_slice(&bytes[offset + needle.len()..]);
        replaced
    }

    #[test]
    fn golden_vectors_match_rust_contracts() {
        let vectors = vectors();
        assert_eq!(vectors.schema_version, 1);
        for case in vectors.canonical_json {
            let actual = canonical_json_bytes(&case.value).unwrap();
            assert_eq!(actual, case.canonical_utf8.as_bytes(), "{}", case.name);
            assert_eq!(hash_bytes(&actual), case.sha256, "{}", case.name);
        }
        for case in vectors.slugs {
            assert_eq!(
                kio_index::chunking::slugify_heading(&case.input),
                case.expected,
                "{}",
                case.name
            );
        }
        for case in vectors.chunk_identity {
            assert_eq!(
                case.chunk.identity_hash().unwrap(),
                case.expected_hash,
                "{}",
                case.name
            );
        }
        for case in vectors.recall {
            let expected: HashSet<_> = case.expected.into_iter().collect();
            let results = case
                .results
                .into_iter()
                .map(|result| RecallResult {
                    raw_hash: result.raw_hash,
                    section_id: result.section_id,
                    heading_path: result.heading_path,
                    path_at_commit: result.path_at_commit,
                })
                .collect::<Vec<_>>();
            assert!(
                (recall_at_k(&results, &expected, case.k) - case.expected_recall).abs()
                    < f64::EPSILON,
                "{}",
                case.name
            );
        }
        for case in vectors.percentiles {
            assert_eq!(
                percentile_nearest_rank(&case.values, case.percentile).unwrap(),
                case.expected,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn percentile_rejects_out_of_range_values() {
        for percentile in [0.0, -0.1, 1.1, f64::NAN] {
            assert!(percentile_nearest_rank(&[1_u64], percentile).is_err());
        }
    }

    #[test]
    fn golden_vector_wire_schema_rejects_unknown_fields_and_wrong_version() {
        let unknown_nested = br#"{
            "schema_version": 1,
            "canonical_json": [], "slugs": [], "chunk_identity": [], "recall": [],
            "percentiles": [{"name": "bad", "values": [], "percentile": 0.95,
                "expected": null, "unexpected": true}]
        }"#;
        assert!(parse_frozen_vectors(unknown_nested).is_err());

        let wrong_version = replace_once(
            &frozen_vector_bytes(),
            br#""schema_version": 1"#,
            br#""schema_version": 2"#,
        );
        assert!(parse_frozen_vectors(&wrong_version).is_err());
    }

    #[test]
    fn golden_vector_wire_schema_rejects_nonfinite_and_invalid_chunk_contracts() {
        let nonfinite = replace_once(&frozen_vector_bytes(), b"1e-7", b"NaN");
        assert!(parse_frozen_vectors(&nonfinite).is_err());

        for (field, replacement) in [
            (
                br#""spec_version": 1, "raw_hash"# as &[u8],
                br#""spec_version": 2, "raw_hash"# as &[u8],
            ),
            (
                br#""raw_hash": "sha256:1111111111111111111111111111111111111111111111111111111111111111"#,
                br#""raw_hash": "sha256:not-a-hash"#,
            ),
            (br#""unit_key": "section:one"# as &[u8], br#""unit_key": ""# as &[u8]),
            (
                br#""byte_start": 0, "byte_end": 5"# as &[u8],
                br#""byte_start": 6, "byte_end": 5"# as &[u8],
            ),
            (
                br#""text_hash": "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"#,
                br#""text_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000"#,
            ),
        ] {
            let wire = replace_once(&frozen_vector_bytes(), field, replacement);
            let parsed: Vectors = serde_json::from_slice(&wire).unwrap();
            assert!(parsed.chunk_identity[0].chunk.validate().is_err());
        }
    }
}
