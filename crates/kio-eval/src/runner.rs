//! Process execution, scoring, and deterministic reporting for `kio-eval`.
//!
//! Manifest parsing and CAS attestation deliberately live in sibling modules.
//! This module only accepts already-resolved expected identities and typed
//! history references, so the CLI can keep all untrusted-input validation at
//! its boundary.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs,
    path::Path,
    process::Command,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    RecallResult, ResultKey,
    attestation::parse_pointer_wire,
    manifest::{HistoryOperation, frozen_history_plan},
    recall_at_k,
};

pub const RECALL_K: usize = 10;
pub const HISTORY_QUERY_COUNT: usize = 16;
pub const DEFAULT_RECALL_TARGET: f64 = 0.8;
pub use kio_process::{
    BoundedProcessError, BoundedProcessOptions, BoundedProcessOutput, BoundedStdin,
    DEFAULT_PROCESS_OUTPUT_LIMIT, DEFAULT_PROCESS_TIMEOUT, run_bounded_command,
};

#[must_use]
pub fn latency_target_ms(scenario: &str) -> Option<f64> {
    match scenario {
        "M3-1" => Some(5_000.0),
        "M3-2" | "M3-3" => Some(7_000.0),
        _ => None,
    }
}

/// Floating-point counterpart to the shared integer golden-vector primitive.
/// Durations originate from a monotonic clock; reject non-finite test/caller
/// values rather than allowing `total_cmp` to make an invalid metric pass.
fn percentile_duration_ms(values: &[f64], percentile: f64) -> Option<f64> {
    if values.is_empty() || !(0.0 < percentile && percentile <= 1.0) {
        return None;
    }
    let mut ordered = values.to_vec();
    if ordered
        .iter()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return None;
    }
    ordered.sort_by(|left, right| left.total_cmp(right));
    let rank = (percentile * ordered.len() as f64).ceil().max(1.0) as usize;
    ordered.get(rank.saturating_sub(1)).copied()
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error(transparent)]
    Process(#[from] BoundedProcessError),
    #[error("invalid evaluator input: {0}")]
    Input(String),
    #[error("could not write evaluation artifact: {0}")]
    Write(#[source] std::io::Error),
    #[error("could not serialize evaluation artifact: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// An unmodified, UTF-8 decoded subprocess outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchOutcome {
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: f64,
}

pub fn run_search(
    binary: impl AsRef<OsStr>,
    cwd: &Path,
    query: &str,
    flags: &[String],
) -> Result<SearchOutcome, RunnerError> {
    run_search_with_env(binary, cwd, query, flags, None)
}

pub fn run_search_with_env(
    binary: impl AsRef<OsStr>,
    cwd: &Path,
    query: &str,
    flags: &[String],
    environment: Option<&[(OsString, OsString)]>,
) -> Result<SearchOutcome, RunnerError> {
    let mut command = Command::new(binary);
    command
        .arg("--json")
        .arg("search")
        .arg(query)
        .arg("--all-scopes")
        .args(flags)
        .current_dir(cwd);
    if let Some(environment) = environment {
        command.env_clear().envs(environment.iter().cloned());
    }
    let output = run_bounded_command(&mut command, BoundedProcessOptions::default(), None)?;
    Ok(SearchOutcome {
        returncode: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: output.stderr,
        duration_ms: output.duration.as_secs_f64() * 1_000.0,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidencePointerRecord {
    pub raw_hash: String,
    pub section_id: Option<String>,
    pub heading_path: Option<Vec<String>>,
    pub path_at_commit: Option<String>,
    pub scope_id: String,
    pub commit: String,
    pub tree: Option<String>,
    pub tool_profile_hash: String,
    pub chunk_hash: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub pointer: EvidencePointerRecord,
    /// Exact returned pointer JSON used by CAS attestation and restore.
    pub pointer_value: Value,
    pub current_paths: Option<Vec<String>>,
    pub current_path: Option<String>,
    /// Product-provided display title. Fixture-B scores this separately from
    /// the normalized on-disk path.
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchResponse {
    pub results: Vec<SearchHit>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClassifiedOutcome {
    Scored {
        response: SearchResponse,
        error_code: Option<String>,
        detail: Option<String>,
    },
    Unimplemented {
        error_code: String,
    },
    Failed {
        error_code: Option<String>,
        detail: String,
    },
}

const MAX_JSON_ERROR_DETAIL_CHARS: usize = 256;

fn parse_json(text: &str) -> Result<Value, String> {
    let text = text.trim();
    if text.is_empty() {
        Err("empty output".to_owned())
    } else {
        serde_json::from_str(text).map_err(|error| {
            error
                .to_string()
                .chars()
                .take(MAX_JSON_ERROR_DETAIL_CHARS)
                .collect()
        })
    }
}

fn error_code(value: Option<&Value>) -> Option<String> {
    value?
        .as_object()?
        .get("error_code")?
        .as_str()
        .map(ToOwned::to_owned)
}

fn is_not_implemented(error_code: &str) -> bool {
    error_code.starts_with("KIO-E-") && error_code.contains("NOT-IMPLEMENTED")
}

fn nonempty_string(value: &Value, field: &str) -> Result<String, String> {
    value
        .as_str()
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("invalid Evidence field: {field}"))
}

fn optional_string(value: Option<&Value>, field: &str) -> Result<Option<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => nonempty_string(value, field).map(Some),
    }
}

fn parse_pointer(value: &Value) -> Result<EvidencePointerRecord, String> {
    let pointer =
        parse_pointer_wire(value).map_err(|error| format!("invalid evidence_pointer: {error}"))?;
    Ok(EvidencePointerRecord {
        commit: pointer.commit,
        tree: pointer.tree,
        raw_hash: pointer.raw_hash,
        tool_profile_hash: pointer.tool_profile_hash,
        chunk_hash: pointer.chunk_hash,
        path_at_commit: pointer.path_at_commit,
        section_id: pointer.section_id,
        heading_path: pointer.heading_path,
        scope_id: pointer.scope_id,
    })
}

enum ResponseParseError {
    Schema(String),
    Pointer { index: usize, reason: String },
}

impl ResponseParseError {
    fn detail(self, exit_code: i32) -> String {
        match self {
            Self::Schema(reason) => {
                format!("exit {exit_code}: JSON response schema invalid: {reason}")
            }
            Self::Pointer { index, reason } => {
                format!("exit {exit_code}: result[{index}] Evidence Pointer invalid: {reason}")
            }
        }
    }
}

fn parse_response(value: Value) -> Result<SearchResponse, ResponseParseError> {
    let object = value
        .as_object()
        .ok_or_else(|| ResponseParseError::Schema("response is not an object".to_owned()))?;
    let results = object
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| ResponseParseError::Schema("response has no results array".to_owned()))?;
    results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            let object = result.as_object().ok_or_else(|| {
                ResponseParseError::Schema(format!("result[{index}] is not an object"))
            })?;
            let pointer = parse_pointer(object.get("evidence_pointer").ok_or_else(|| {
                ResponseParseError::Schema(format!("result[{index}] has no evidence_pointer"))
            })?)
            .map_err(|error| ResponseParseError::Pointer {
                index,
                reason: error,
            })?;
            let current_paths = match object.get("current_paths") {
                None | Some(Value::Null) => None,
                Some(Value::Array(paths)) => Some(
                    paths
                        .iter()
                        .map(|path| nonempty_string(path, "current_paths"))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(ResponseParseError::Schema)?,
                ),
                Some(_) => {
                    return Err(ResponseParseError::Schema(format!(
                        "result[{index}] invalid current_paths"
                    )));
                }
            };
            Ok(SearchHit {
                pointer,
                pointer_value: object
                    .get("evidence_pointer")
                    .expect("pointer was checked above")
                    .clone(),
                current_paths,
                current_path: optional_string(object.get("current_path"), "current_path")
                    .map_err(ResponseParseError::Schema)?,
                title: optional_string(object.get("title"), "title")
                    .map_err(ResponseParseError::Schema)?,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|results| SearchResponse { results })
}

/// Apply the specified `0/3` result policy without conflating malformed output
/// with an honest `NOT-IMPLEMENTED` product response.
#[must_use]
pub fn classify_outcome(outcome: &SearchOutcome) -> ClassifiedOutcome {
    let stdout = parse_json(&outcome.stdout);
    let stderr = parse_json(&outcome.stderr);
    let code = error_code(stdout.as_ref().ok()).or_else(|| error_code(stderr.as_ref().ok()));
    if let Some(code) = code.as_deref().filter(|code| is_not_implemented(code)) {
        return ClassifiedOutcome::Unimplemented {
            error_code: code.to_owned(),
        };
    }
    if matches!(outcome.returncode, 0 | 3) {
        return match stdout {
            Ok(value) => match parse_response(value) {
                Ok(response) => ClassifiedOutcome::Scored {
                    response,
                    error_code: code,
                    detail: (outcome.returncode == 3).then(|| "partial(exit 3)".to_owned()),
                },
                Err(error) => ClassifiedOutcome::Failed {
                    error_code: code,
                    detail: error.detail(outcome.returncode),
                },
            },
            Err(reason) => ClassifiedOutcome::Failed {
                error_code: code,
                detail: format!("exit {}: stdout is not JSON: {reason}", outcome.returncode),
            },
        };
    }
    let detail = stderr
        .as_ref()
        .ok()
        .and_then(Value::as_object)
        .and_then(|object| object.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            if outcome.stderr.trim().is_empty() {
                outcome.stdout.trim()
            } else {
                outcome.stderr.trim()
            }
        });
    ClassifiedOutcome::Failed {
        error_code: code,
        detail: format!("exit={}: {detail}", outcome.returncode),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryResult {
    pub scenario: String,
    pub query: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recall_at_10: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub duration_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pointer_attested: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScenarioSummary {
    pub n_queries: usize,
    pub n_scored: usize,
    pub recall_at_10: Option<f64>,
    pub passes_target: bool,
    pub p95_ms: Option<f64>,
    pub latency_target_ms: f64,
    pub passes_latency: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RenameFailure {
    pub scope: String,
    pub raw_hash: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryCoverage {
    pub edited_old_required: usize,
    pub edited_old_missing: Vec<String>,
    pub rename_required: usize,
    pub rename_failures: Vec<RenameFailure>,
    pub deleted_required: usize,
    pub deleted_missing: Vec<String>,
    pub passes_m3_2: bool,
    pub passes_m3_3: bool,
    pub restore_problems: Vec<String>,
    pub passes_restore: bool,
    pub pointer_attested: usize,
    pub pointer_attestation_failures: usize,
    pub passes_pointer_attestation: bool,
}

impl Default for HistoryCoverage {
    fn default() -> Self {
        Self {
            edited_old_required: 0,
            edited_old_missing: Vec::new(),
            rename_required: 0,
            rename_failures: Vec::new(),
            deleted_required: 0,
            deleted_missing: Vec::new(),
            passes_m3_2: true,
            passes_m3_3: true,
            restore_problems: Vec::new(),
            passes_restore: true,
            pointer_attested: 0,
            pointer_attestation_failures: 0,
            passes_pointer_attestation: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Counts {
    pub n_queries: usize,
    pub n_unimplemented: usize,
    pub n_failed: usize,
    pub n_scored: usize,
    pub n_pointer_attested: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationResults {
    pub target_recall_at_10: f64,
    pub scenarios: BTreeMap<String, ScenarioSummary>,
    pub queries: Vec<QueryResult>,
    pub history_coverage: HistoryCoverage,
    pub counts: Counts,
}

#[derive(Debug, Clone)]
pub struct ResolvedQuery {
    pub scenario: String,
    pub query: String,
    pub expected: HashSet<ResultKey>,
    /// Fixture-resolution failures remain per-query evaluator results. Search
    /// is still run first so a NOT-IMPLEMENTED response keeps its higher-priority
    /// exit-2 classification.
    pub resolution_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ScoredRecord {
    pub scenario: String,
    pub expected: HashSet<ResultKey>,
    pub response: SearchResponse,
}

fn full_hash(hex: &str) -> String {
    format!("sha256:{hex}")
}

fn result_key(pointer: &EvidencePointerRecord) -> ResultKey {
    RecallResult {
        raw_hash: pointer.raw_hash.clone(),
        section_id: pointer.section_id.clone(),
        heading_path: pointer.heading_path.clone(),
        path_at_commit: pointer.path_at_commit.clone(),
    }
    .key()
}

impl SearchHit {
    #[must_use]
    pub fn result_key(&self) -> ResultKey {
        result_key(&self.pointer)
    }
}

fn recalled_hits<'a>(records: &'a [ScoredRecord], scenario: &str) -> Vec<&'a SearchHit> {
    records
        .iter()
        .filter(|record| record.scenario == scenario)
        .flat_map(|record| {
            record
                .response
                .results
                .iter()
                .take(RECALL_K)
                .filter(move |hit| record.expected.contains(&result_key(&hit.pointer)))
        })
        .collect()
}

/// History-specific structural gates. Restore validation is performed by the
/// caller, then recorded with [`HistoryCoverage::set_restore_problems`].
#[must_use]
pub fn assess_history_coverage(records: &[ScoredRecord]) -> HistoryCoverage {
    let plan = frozen_history_plan().expect("bundled history plan is validated at build time");
    let m32_hits = recalled_hits(records, "M3-2");
    let m33_hits = recalled_hits(records, "M3-3");
    let m32_raws: HashSet<_> = m32_hits
        .iter()
        .map(|hit| hit.pointer.raw_hash.as_str())
        .collect();
    let mut edited_old_missing = plan
        .operations
        .iter()
        .filter_map(|operation| match operation {
            HistoryOperation::Edit {
                before_raw_sha256, ..
            } => Some(full_hash(before_raw_sha256)),
            _ => None,
        })
        .filter(|hash| !m32_raws.contains(hash.as_str()))
        .collect::<Vec<_>>();
    edited_old_missing.sort();

    let mut rename_failures = Vec::new();
    for rename in plan
        .operations
        .iter()
        .filter_map(|operation| match operation {
            HistoryOperation::Rename {
                scope,
                old_file,
                new_file,
                before_raw_sha256,
                ..
            } => Some((scope, old_file, new_file, before_raw_sha256)),
            _ => None,
        })
    {
        let (scope, old_file, new_file, before_raw_sha256) = rename;
        let raw_hash = full_hash(before_raw_sha256);
        let mut paths = BTreeSet::new();
        let mut aliases_valid = true;
        let mut saw_old = false;
        for record in records.iter().filter(|record| record.scenario == "M3-2") {
            let twin_identities = record
                .expected
                .iter()
                .filter(|(raw, _, path)| {
                    raw == &raw_hash && path.as_deref() == Some(old_file.as_str())
                })
                .map(|(raw, section, _)| (raw.clone(), section.clone(), Some(new_file.clone())))
                .collect::<HashSet<_>>();
            if twin_identities.is_empty() {
                continue;
            }
            for hit in record.response.results.iter().take(RECALL_K) {
                if hit.pointer.raw_hash != raw_hash {
                    continue;
                }
                let identity = result_key(&hit.pointer);
                if record.expected.contains(&identity) || twin_identities.contains(&identity) {
                    if hit.pointer.path_at_commit.as_deref() == Some(old_file.as_str()) {
                        saw_old = true;
                    }
                    if let Some(path) = &hit.pointer.path_at_commit {
                        paths.insert(path.clone());
                    }
                    aliases_valid &= hit.current_paths.as_deref()
                        == Some(std::slice::from_ref(new_file))
                        && hit.current_path.as_deref() == Some(new_file.as_str());
                }
            }
        }
        if !saw_old || !paths.contains(old_file) || !paths.contains(new_file) || !aliases_valid {
            rename_failures.push(RenameFailure {
                scope: scope.clone(),
                raw_hash,
                paths: paths.into_iter().collect(),
            });
        }
    }
    let m33_raws: HashSet<_> = m33_hits
        .iter()
        .map(|hit| hit.pointer.raw_hash.as_str())
        .collect();
    let mut deleted_missing = plan
        .operations
        .iter()
        .filter_map(|operation| match operation {
            HistoryOperation::Delete {
                before_raw_sha256, ..
            } => Some(full_hash(before_raw_sha256)),
            _ => None,
        })
        .filter(|hash| !m33_raws.contains(hash.as_str()))
        .collect::<Vec<_>>();
    deleted_missing.sort();
    HistoryCoverage {
        edited_old_required: plan
            .operations
            .iter()
            .filter(|operation| matches!(operation, HistoryOperation::Edit { .. }))
            .count(),
        passes_m3_2: edited_old_missing.is_empty() && rename_failures.is_empty(),
        edited_old_missing,
        rename_required: plan
            .operations
            .iter()
            .filter(|operation| matches!(operation, HistoryOperation::Rename { .. }))
            .count(),
        rename_failures,
        deleted_required: plan
            .operations
            .iter()
            .filter(|operation| matches!(operation, HistoryOperation::Delete { .. }))
            .count(),
        passes_m3_3: deleted_missing.is_empty(),
        deleted_missing,
        ..HistoryCoverage::default()
    }
}

impl HistoryCoverage {
    pub fn set_restore_problems(&mut self, problems: Vec<String>) {
        self.passes_restore = problems.is_empty();
        self.restore_problems = problems;
    }
}

/// Score already-resolved queries and return records needed for history gates.
///
/// The validator runs after strict response decoding but before a response can
/// contribute recall. It is the integration point for M3-2 CAS attestation.
pub fn evaluate_queries_with_validator<F, V>(
    queries: &[ResolvedQuery],
    recall_target: f64,
    mut search: F,
    mut validate: V,
) -> Result<(EvaluationResults, Vec<ScoredRecord>), RunnerError>
where
    F: FnMut(&ResolvedQuery) -> Result<SearchOutcome, RunnerError>,
    V: FnMut(&ResolvedQuery, &SearchResponse) -> Result<Option<usize>, String>,
{
    if !(0.0..=1.0).contains(&recall_target) {
        return Err(RunnerError::Input(
            "recall target must be in [0, 1]".to_owned(),
        ));
    }
    let mut scores: HashMap<&str, Vec<f64>> = HashMap::new();
    let mut latencies: HashMap<&str, Vec<f64>> = HashMap::new();
    let mut results = EvaluationResults {
        target_recall_at_10: recall_target,
        scenarios: BTreeMap::new(),
        queries: Vec::new(),
        history_coverage: HistoryCoverage::default(),
        counts: Counts {
            n_queries: queries.len(),
            ..Counts::default()
        },
    };
    let mut records = Vec::new();
    for query in queries {
        let outcome = search(query)?;
        let duration_ms = outcome.duration_ms;
        match classify_outcome(&outcome) {
            ClassifiedOutcome::Unimplemented { error_code } => {
                results.counts.n_unimplemented += 1;
                results.queries.push(QueryResult {
                    scenario: query.scenario.clone(),
                    query: query.query.clone(),
                    status: "unimplemented".to_owned(),
                    recall_at_10: None,
                    error_code: Some(error_code),
                    detail: Some("search 未実装 (NOT-IMPLEMENTED)".to_owned()),
                    duration_ms,
                    pointer_attested: None,
                });
            }
            classified if query.resolution_error.is_some() => {
                let error_code = match classified {
                    ClassifiedOutcome::Failed { error_code, .. } => error_code,
                    ClassifiedOutcome::Scored { error_code, .. } => error_code,
                    ClassifiedOutcome::Unimplemented { .. } => unreachable!("handled above"),
                };
                results.counts.n_failed += 1;
                results.counts.n_scored += 1;
                scores.entry(&query.scenario).or_default().push(0.0);
                latencies
                    .entry(&query.scenario)
                    .or_default()
                    .push(duration_ms);
                results.queries.push(QueryResult {
                    scenario: query.scenario.clone(),
                    query: query.query.clone(),
                    status: "failed".to_owned(),
                    recall_at_10: Some(0.0),
                    error_code,
                    detail: query.resolution_error.clone(),
                    duration_ms,
                    pointer_attested: None,
                });
            }
            ClassifiedOutcome::Failed { error_code, detail } => {
                results.counts.n_failed += 1;
                results.counts.n_scored += 1;
                scores.entry(&query.scenario).or_default().push(0.0);
                latencies
                    .entry(&query.scenario)
                    .or_default()
                    .push(duration_ms);
                results.queries.push(QueryResult {
                    scenario: query.scenario.clone(),
                    query: query.query.clone(),
                    status: "failed".to_owned(),
                    recall_at_10: Some(0.0),
                    error_code,
                    detail: Some(detail),
                    duration_ms,
                    pointer_attested: None,
                });
            }
            ClassifiedOutcome::Scored {
                response, detail, ..
            } => {
                let pointer_attested = match validate(query, &response) {
                    Ok(pointer_attested) => pointer_attested,
                    Err(validation_error) => {
                        results.counts.n_failed += 1;
                        results.counts.n_scored += 1;
                        scores.entry(&query.scenario).or_default().push(0.0);
                        latencies
                            .entry(&query.scenario)
                            .or_default()
                            .push(duration_ms);
                        results.queries.push(QueryResult {
                            scenario: query.scenario.clone(),
                            query: query.query.clone(),
                            status: "failed".to_owned(),
                            recall_at_10: Some(0.0),
                            error_code: None,
                            detail: Some(validation_error),
                            duration_ms,
                            pointer_attested: None,
                        });
                        continue;
                    }
                };
                let recall = recall_at_k(
                    &response
                        .results
                        .iter()
                        .map(|hit| RecallResult {
                            raw_hash: hit.pointer.raw_hash.clone(),
                            section_id: hit.pointer.section_id.clone(),
                            heading_path: hit.pointer.heading_path.clone(),
                            path_at_commit: hit.pointer.path_at_commit.clone(),
                        })
                        .collect::<Vec<_>>(),
                    &query.expected,
                    RECALL_K,
                );
                scores.entry(&query.scenario).or_default().push(recall);
                latencies
                    .entry(&query.scenario)
                    .or_default()
                    .push(duration_ms);
                results.counts.n_scored += 1;
                results.counts.n_pointer_attested += pointer_attested.unwrap_or(0);
                results.queries.push(QueryResult {
                    scenario: query.scenario.clone(),
                    query: query.query.clone(),
                    status: "scored".to_owned(),
                    recall_at_10: Some(recall),
                    error_code: None,
                    detail,
                    duration_ms,
                    pointer_attested,
                });
                records.push(ScoredRecord {
                    scenario: query.scenario.clone(),
                    expected: query.expected.clone(),
                    response,
                });
            }
        }
    }
    let scenario_names = queries
        .iter()
        .map(|query| query.scenario.as_str())
        .collect::<BTreeSet<_>>();
    for scenario in scenario_names {
        let latency_target_ms = latency_target_ms(scenario)
            .ok_or_else(|| RunnerError::Input(format!("unknown scenario: {scenario}")))?;
        let scenario_scores = scores.get(scenario).cloned().unwrap_or_default();
        let scenario_latencies = latencies.get(scenario).cloned().unwrap_or_default();
        let average = (!scenario_scores.is_empty())
            .then(|| scenario_scores.iter().sum::<f64>() / scenario_scores.len() as f64);
        let p95_ms = percentile_duration_ms(&scenario_latencies, 0.95);
        results.scenarios.insert(
            scenario.to_owned(),
            ScenarioSummary {
                n_queries: queries
                    .iter()
                    .filter(|query| query.scenario == scenario)
                    .count(),
                n_scored: scenario_scores.len(),
                recall_at_10: average,
                passes_target: average.is_some_and(|value| value >= recall_target),
                p95_ms,
                latency_target_ms,
                passes_latency: p95_ms.is_some_and(|value| value < latency_target_ms),
            },
        );
    }
    Ok((results, records))
}

pub fn evaluate_queries<F>(
    queries: &[ResolvedQuery],
    recall_target: f64,
    search: F,
) -> Result<(EvaluationResults, Vec<ScoredRecord>), RunnerError>
where
    F: FnMut(&ResolvedQuery) -> Result<SearchOutcome, RunnerError>,
{
    evaluate_queries_with_validator(queries, recall_target, search, |_, _| Ok(None))
}

/// Final policy: 2 wins for any unfinished search, otherwise 1 for every
/// failed metric/coverage gate, otherwise 0.
#[must_use]
pub fn final_exit_code(results: &EvaluationResults, active: &[String]) -> i32 {
    if results.counts.n_unimplemented > 0 {
        return 2;
    }
    if results.counts.n_failed > 0 {
        return 1;
    }
    for scenario in active {
        let Some(summary) = results.scenarios.get(scenario) else {
            return 1;
        };
        if !summary.passes_target
            || !summary.passes_latency
            || summary.n_scored != summary.n_queries
        {
            return 1;
        }
        if matches!(scenario.as_str(), "M3-2" | "M3-3") && summary.n_queries != HISTORY_QUERY_COUNT
        {
            return 1;
        }
    }
    if active.iter().any(|scenario| scenario == "M3-2")
        && (!results.history_coverage.passes_m3_2
            || !results.history_coverage.passes_pointer_attestation)
    {
        return 1;
    }
    if active.iter().any(|scenario| scenario == "M3-3")
        && (!results.history_coverage.passes_m3_3 || !results.history_coverage.passes_restore)
    {
        return 1;
    }
    0
}

pub fn write_results(path: &Path, results: &EvaluationResults) -> Result<(), RunnerError> {
    // Serialize through `Value`: serde_json's map ordering is deterministic and
    // matches Python's `sort_keys=True` evaluator artifact contract.
    let value = serde_json::to_value(results)?;
    let mut bytes = serde_json::to_vec_pretty(&value)?;
    bytes.push(b'\n');
    fs::write(path, bytes).map_err(RunnerError::Write)
}

/// Stable Markdown report; its row order follows the caller's active scenario
/// order rather than hash-map iteration.
pub fn write_report(
    path: &Path,
    results: &EvaluationResults,
    active: &[String],
) -> Result<(), RunnerError> {
    let counts = &results.counts;
    let mut lines = vec![
        "# Kio 検索評価レポート (synthetic)".to_owned(),
        String::new(),
    ];
    lines.push(format!(
        "- 目標: 各シナリオ Recall@10 >= {} (docs/09 §4.3)",
        results.target_recall_at_10
    ));
    lines.push(format!(
        "- クエリ数: {} (scored={} / failed={} / unimplemented={})",
        counts.n_queries, counts.n_scored, counts.n_failed, counts.n_unimplemented
    ));
    if counts.n_unimplemented > 0 {
        lines.push("- 状態: **kio search 未実装のクエリあり (NOT-IMPLEMENTED)**。Recall 判定は無効 (exit 2)。".to_owned());
    }
    lines.extend([
        String::new(),
        "| シナリオ | クエリ数 | scored | Recall@10 | p95 ms | 目標 ms | 判定 |".to_owned(),
        "| --- | --- | --- | --- | --- | --- | --- |".to_owned(),
    ]);
    for scenario in active {
        let Some(summary) = results.scenarios.get(scenario) else {
            continue;
        };
        let recall = summary
            .recall_at_10
            .map_or_else(|| "-".to_owned(), |value| format!("{value:.3}"));
        let p95 = summary
            .p95_ms
            .map_or_else(|| "-".to_owned(), |value| format!("{value:.1}"));
        let verdict = if summary.n_scored == 0 {
            "n/a"
        } else if summary.passes_target && summary.passes_latency {
            "PASS"
        } else {
            "FAIL"
        };
        lines.push(format!(
            "| {scenario} | {} | {} | {recall} | {p95} | <{:.0} | {verdict} |",
            summary.n_queries, summary.n_scored, summary.latency_target_ms
        ));
    }
    if active.iter().any(|scenario| scenario == "M3-2") {
        lines.extend([
            String::new(),
            format!(
                "- M3-2 edited/rename structural coverage: {}",
                if results.history_coverage.passes_m3_2 {
                    "PASS"
                } else {
                    "FAIL"
                }
            ),
            format!(
                "- M3-2 pointer CAS attestation: {} ({} pointers)",
                if results.history_coverage.passes_pointer_attestation {
                    "PASS"
                } else {
                    "FAIL"
                },
                results.history_coverage.pointer_attested
            ),
        ]);
    }
    if active.iter().any(|scenario| scenario == "M3-3") {
        lines.extend([
            format!(
                "- M3-3 deleted coverage: {}",
                if results.history_coverage.passes_m3_3 {
                    "PASS"
                } else {
                    "FAIL"
                }
            ),
            format!(
                "- M3-3 restore verification: {}",
                if results.history_coverage.passes_restore {
                    "PASS"
                } else {
                    "FAIL"
                }
            ),
        ]);
    }
    lines.push(String::new());
    // Python writes `"\n".join(lines) + "\n"`; because `lines` ends with an
    // empty element this deliberately preserves two terminal LF bytes.
    fs::write(path, format!("{}\n", lines.join("\n"))).map_err(RunnerError::Write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        thread,
        time::{Duration, Instant},
    };

    fn pointer() -> Value {
        let hash = |byte: char| format!("sha256:{}", byte.to_string().repeat(64));
        serde_json::json!({"schema_version":1,"commit":hash('c'),"raw_hash":hash('a'),"tool_profile_hash":hash('b'),"chunk_hash":hash('d'),"scope_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","path_at_commit":"a.md","section_id":"heading"})
    }
    fn outcome(code: i32, stdout: Value) -> SearchOutcome {
        SearchOutcome {
            returncode: code,
            stdout: stdout.to_string(),
            stderr: String::new(),
            duration_ms: 12.5,
        }
    }

    #[cfg(unix)]
    fn shell(command: &str) -> Command {
        let mut process = Command::new("sh");
        process.arg("-c").arg(command);
        process
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_times_out_and_reports_the_configured_limit() {
        let mut command = shell("sleep 5");
        let error = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: Duration::from_millis(100),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BoundedProcessError::Timeout { timeout_ms: 100 }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_unread_large_stdin_returns_by_the_shared_deadline() {
        let mut command = shell("sleep 5");
        let started = Instant::now();
        let error = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: Duration::from_millis(100),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
            Some(BoundedStdin::new(
                vec![b'x'; 2 * 1024 * 1024],
                2 * 1024 * 1024,
            )),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BoundedProcessError::Timeout { timeout_ms: 100 }
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_drains_writable_stdin_before_waiting_for_output() {
        // `wc` consumes stdin but emits nothing until it observes EOF. A 32
        // MiB input is small enough for a local CI regression yet materially
        // exposes the former one-write-then-10-ms-recv loop across ordinary
        // Unix pipe capacities; the 3 s deadline leaves startup and I/O
        // headroom for the readiness-driven implementation.
        let input_len = 32 * 1024 * 1024;
        let mut command = shell("wc -c >/dev/null; printf done");
        let output = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: Duration::from_secs(3),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
            Some(BoundedStdin::new(vec![b'x'; input_len], input_len)),
        )
        .expect("the child consumes all stdin before producing output");
        assert_eq!(output.stdout, "done");
        assert!(output.stderr.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_closed_output_pipes_do_not_bypass_the_deadline() {
        let mut command = shell("exec 1>&- 2>&-; sleep 5");
        let started = Instant::now();
        let error = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: Duration::from_millis(100),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BoundedProcessError::Timeout { timeout_ms: 100 }
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn bounded_process_rejects_oversized_stdin_before_spawn() {
        let mut command = Command::new("definitely-not-an-evaluator");
        let error = run_bounded_command(
            &mut command,
            BoundedProcessOptions::default(),
            Some(BoundedStdin::new(vec![0; 2], 1)),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BoundedProcessError::InputLimit { limit: 1 }
        ));
    }

    #[cfg(windows)]
    struct WindowsPreAttachDelay(Option<Duration>);

    #[cfg(windows)]
    impl WindowsPreAttachDelay {
        fn set(delay: Duration) -> Self {
            let previous = kio_process::replace_windows_pre_attach_delay_for_test(Some(delay));
            Self(previous)
        }
    }

    #[cfg(windows)]
    impl Drop for WindowsPreAttachDelay {
        fn drop(&mut self) {
            let _ = kio_process::replace_windows_pre_attach_delay_for_test(self.0);
        }
    }

    #[cfg(windows)]
    fn assert_windows_job_timeout_terminates_descendant(
        delay_before_attach: Option<Duration>,
        timeout: Duration,
        elapsed_bound: Duration,
    ) {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0},
            Storage::FileSystem::SYNCHRONIZE,
            System::Threading::{OpenProcess, WaitForSingleObject},
        };

        let _delay = delay_before_attach.map(WindowsPreAttachDelay::set);
        let marker = tempfile::NamedTempFile::new()
            .expect("create descendant marker")
            .into_temp_path();
        let mut command = Command::new(std::env::current_exe().expect("current test binary"));
        command
            .arg("--exact")
            .arg("runner::tests::windows_job_descendant_helper")
            .arg("--nocapture")
            .env("KIO_EVAL_WINDOWS_JOB_MARKER", &marker);
        let started = Instant::now();
        let error = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout,
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BoundedProcessError::Timeout { timeout_ms }
                if timeout_ms == timeout.as_millis()
        ));
        assert!(started.elapsed() < elapsed_bound);
        let pid = (0..50)
            .find_map(|_| {
                let result = fs::read_to_string(&marker)
                    .ok()
                    .and_then(|value| value.trim().parse::<u32>().ok());
                if result.is_none() {
                    thread::sleep(Duration::from_millis(10));
                }
                result
            })
            .expect("job descendant recorded its PID before timeout");
        let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            // A Job Object may have already terminated and reaped the leaf
            // before this assertion observes it. `OpenProcess` reports that
            // documented no-such-process state as ERROR_INVALID_PARAMETER.
            assert_eq!(
                unsafe { GetLastError() },
                ERROR_INVALID_PARAMETER,
                "open recorded descendant PID"
            );
            return;
        }
        let waited = unsafe { WaitForSingleObject(handle, 1_000) };
        unsafe { CloseHandle(handle) };
        assert_eq!(
            waited, WAIT_OBJECT_0,
            "Job Object did not terminate descendant"
        );
    }

    #[cfg(windows)]
    #[test]
    fn bounded_process_timeout_terminates_a_windows_job_with_descendants() {
        assert_windows_job_timeout_terminates_descendant(
            None,
            Duration::from_millis(500),
            Duration::from_secs(2),
        );
    }

    #[cfg(windows)]
    #[test]
    fn bounded_process_pre_attach_delay_cannot_allow_a_windows_descendant_to_escape() {
        // The helper spawns its leaf immediately. Without CREATE_SUSPENDED,
        // this delay would let that leaf exist before its parent joined the
        // Job Object, so it would not inherit the kill-on-close boundary.
        assert_windows_job_timeout_terminates_descendant(
            Some(Duration::from_millis(500)),
            Duration::from_secs(2),
            Duration::from_secs(5),
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_job_descendant_helper() {
        let Some(marker) = std::env::var_os("KIO_EVAL_WINDOWS_JOB_MARKER") else {
            return;
        };
        let mut leaf = Command::new(std::env::current_exe().expect("current test binary"));
        leaf.arg("--exact")
            .arg("runner::tests::windows_job_leaf_helper")
            .arg("--nocapture")
            .env("KIO_EVAL_WINDOWS_JOB_LEAF_MARKER", marker);
        let _leaf = leaf.spawn().expect("spawn Job Object descendant");
        thread::sleep(Duration::from_secs(30));
    }

    #[cfg(windows)]
    #[test]
    fn windows_job_leaf_helper() {
        let Some(marker) = std::env::var_os("KIO_EVAL_WINDOWS_JOB_LEAF_MARKER") else {
            return;
        };
        fs::write(marker, std::process::id().to_string()).expect("write descendant PID");
        thread::sleep(Duration::from_secs(30));
    }

    /// This is invoked in a separate test binary by the escaped-pipe tests.
    /// The actual escaped holder is installed by their `pre_exec` hook, before
    /// this helper is executed. Keeping this body empty prevents a second,
    /// racy fork after the deterministic handshake has completed.
    #[cfg(unix)]
    #[test]
    fn escaped_pipe_holder_helper() {}

    #[cfg(unix)]
    struct EscapedPipeHolder {
        marker: tempfile::TempPath,
        marker_file: fs::File,
        release_reader: fs::File,
        release_writer: Option<fs::File>,
    }

    #[cfg(unix)]
    struct EscapedPipeProgram {
        shell: std::ffi::CString,
        shell_name: std::ffi::CString,
        command_flag: std::ffi::CString,
        script: std::ffi::CString,
    }

    #[cfg(unix)]
    impl EscapedPipeProgram {
        fn new(overflow: bool, shell: &str) -> Self {
            let script = if overflow {
                format!(
                    "printf A >&4; exec 4>&-; printf '{}'; read _ <&5; exec 5<&-",
                    "x".repeat(2_048)
                )
            } else {
                "printf A >&4; exec 4>&-; read _ <&5; exec 5<&-".to_owned()
            };
            Self {
                shell: std::ffi::CString::new(shell).expect("shell path has no NUL"),
                shell_name: std::ffi::CString::new("sh").expect("shell name has no NUL"),
                command_flag: std::ffi::CString::new("-c").expect("command flag has no NUL"),
                script: std::ffi::CString::new(script).expect("script has no NUL"),
            }
        }
    }

    #[cfg(unix)]
    impl EscapedPipeHolder {
        fn new() -> Self {
            use std::os::fd::FromRawFd;

            let marker = tempfile::NamedTempFile::new()
                .expect("create marker")
                .into_temp_path();
            let marker_file = fs::OpenOptions::new()
                .write(true)
                .open(&marker)
                .expect("open marker for pre-exec holder");
            let mut release = [-1; 2];
            assert_eq!(
                unsafe { libc::pipe(release.as_mut_ptr()) },
                0,
                "create release pipe"
            );
            for fd in release {
                assert_eq!(
                    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                    0,
                    "set release pipe CLOEXEC"
                );
            }
            Self {
                marker,
                marker_file,
                // SAFETY: `release` was created above and each ownership is
                // transferred into exactly one File for the test lifetime.
                release_reader: unsafe { fs::File::from_raw_fd(release[0]) },
                release_writer: Some(unsafe { fs::File::from_raw_fd(release[1]) }),
            }
        }

        fn install_pre_exec_holder(&self, command: &mut Command, overflow: bool) {
            self.install_pre_exec_holder_with_options(command, overflow, "/bin/sh", 30_000, false);
        }

        fn install_pre_exec_holder_with_shell(
            &self,
            command: &mut Command,
            overflow: bool,
            shell: &str,
        ) {
            self.install_pre_exec_holder_with_options(command, overflow, shell, 30_000, false);
        }

        fn install_pre_exec_holder_with_options(
            &self,
            command: &mut Command,
            overflow: bool,
            shell: &str,
            ack_timeout_millis: libc::c_int,
            stall_after_marker: bool,
        ) {
            use std::{os::fd::AsRawFd, os::unix::process::CommandExt};

            let marker_fd = self.marker_file.as_raw_fd();
            let release_fd = self.release_reader.as_raw_fd();
            let program = EscapedPipeProgram::new(overflow, shell);
            // SAFETY: after fork this closure calls only the async-signal-safe
            // libc routines in `spawn_escaped_pipe_holder_pre_exec`. It does
            // not allocate, lock, access Rust I/O, or return through a
            // fallible Rust operation. The finite ACK wait exits the child on
            // failure, so `Command::spawn` cannot wait indefinitely here.
            unsafe {
                command.pre_exec(move || {
                    let argv = [
                        program.shell_name.as_ptr(),
                        program.command_flag.as_ptr(),
                        program.script.as_ptr(),
                        std::ptr::null(),
                    ];
                    let envp = [std::ptr::null()];
                    spawn_escaped_pipe_holder_pre_exec(
                        marker_fd,
                        release_fd,
                        program.shell.as_ptr(),
                        argv.as_ptr(),
                        envp.as_ptr(),
                        ack_timeout_millis,
                        stall_after_marker,
                    );
                    Ok(())
                });
            }
        }

        fn wait_for_pid(&self) {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if fs::metadata(&self.marker)
                    .ok()
                    .is_some_and(|metadata| metadata.len() > 0)
                {
                    return;
                }
                if Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("escaped pipe holder did not record its pid within 30 seconds");
        }
    }

    #[cfg(unix)]
    unsafe fn close_or_exit(fd: libc::c_int) {
        if unsafe { libc::close(fd) } < 0 {
            unsafe { libc::_exit(127) };
        }
    }

    #[cfg(unix)]
    unsafe fn dup_to_or_exit(source: libc::c_int, target: libc::c_int) {
        if source != target {
            if unsafe { libc::dup2(source, target) } < 0 {
                unsafe { libc::_exit(127) };
            }
            unsafe { close_or_exit(source) };
        }
    }

    #[cfg(unix)]
    unsafe fn duplicate_cloexec_or_exit(fd: libc::c_int) -> libc::c_int {
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 16) };
        if duplicate < 0 {
            unsafe { libc::_exit(127) };
        }
        duplicate
    }

    #[cfg(unix)]
    unsafe fn write_all_or_exit(fd: libc::c_int, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() {
            let written =
                unsafe { libc::write(fd, bytes[offset..].as_ptr().cast(), bytes.len() - offset) };
            if written > 0 {
                offset += written as usize;
            } else if written < 0 && unsafe { errno() } == libc::EINTR {
                continue;
            } else {
                unsafe { libc::_exit(127) };
            }
        }
    }

    #[cfg(target_os = "linux")]
    unsafe fn errno() -> libc::c_int {
        unsafe { *libc::__errno_location() }
    }

    #[cfg(target_os = "macos")]
    unsafe fn errno() -> libc::c_int {
        unsafe { *libc::__error() }
    }

    #[cfg(unix)]
    unsafe fn write_pid_or_exit(marker_fd: libc::c_int) {
        if unsafe { libc::setsid() } < 0
            || unsafe { libc::ftruncate(marker_fd, 0) } < 0
            || unsafe { libc::lseek(marker_fd, 0, libc::SEEK_SET) } < 0
        {
            unsafe { libc::_exit(127) };
        }
        let mut digits = [0_u8; 20];
        let mut number = unsafe { libc::getpid() as u64 };
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (number % 10) as u8;
            number /= 10;
            if number == 0 {
                break;
            }
        }
        unsafe { write_all_or_exit(marker_fd, &digits[start..]) };
    }

    #[cfg(unix)]
    unsafe fn wait_for_ack(read_fd: libc::c_int, timeout_millis: libc::c_int) -> bool {
        let mut started = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut started) } < 0 {
            return false;
        }
        let mut deadline_secs = started.tv_sec + (timeout_millis / 1_000) as libc::time_t;
        let mut deadline_nsecs =
            started.tv_nsec + (timeout_millis % 1_000) as libc::c_long * 1_000_000;
        if deadline_nsecs >= 1_000_000_000 {
            deadline_secs += 1;
            deadline_nsecs -= 1_000_000_000;
        }
        loop {
            let mut now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } < 0 {
                return false;
            }
            if now.tv_sec > deadline_secs
                || (now.tv_sec == deadline_secs && now.tv_nsec >= deadline_nsecs)
            {
                return false;
            }
            let mut remaining_secs = deadline_secs - now.tv_sec;
            let mut remaining_nsecs = deadline_nsecs - now.tv_nsec;
            if remaining_nsecs < 0 {
                remaining_secs -= 1;
                remaining_nsecs += 1_000_000_000;
            }
            let remaining_ms = remaining_secs * 1_000 + (remaining_nsecs + 999_999) / 1_000_000;
            let mut pollfd = libc::pollfd {
                fd: read_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout_ms = if remaining_ms > libc::c_int::MAX as libc::time_t {
                libc::c_int::MAX
            } else {
                remaining_ms as libc::c_int
            };
            let polled = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
            if polled == 0 {
                return false;
            }
            if polled < 0 {
                if unsafe { errno() } == libc::EINTR {
                    continue;
                }
                return false;
            }
            let mut ack = 0_u8;
            let read = unsafe { libc::read(read_fd, (&mut ack as *mut u8).cast(), 1) };
            if read == 1 && ack == b'A' {
                return true;
            }
            if read < 0 && unsafe { errno() } == libc::EINTR {
                continue;
            }
            return false;
        }
    }

    #[cfg(unix)]
    unsafe fn reap_killed_holder_or_exit(child: libc::pid_t) {
        let mut started = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut started) } < 0 {
            unsafe { libc::_exit(127) };
        }
        let deadline_secs = started.tv_sec + 30;
        loop {
            let mut status = 0;
            let waited = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
            if waited == child || (waited < 0 && unsafe { errno() } == libc::ECHILD) {
                return;
            }
            if waited < 0 && unsafe { errno() } != libc::EINTR {
                unsafe { libc::_exit(127) };
            }
            let mut now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } < 0
                || now.tv_sec > deadline_secs
                || (now.tv_sec == deadline_secs && now.tv_nsec >= started.tv_nsec)
            {
                unsafe { libc::_exit(127) };
            }
            let slept = unsafe { libc::poll(std::ptr::null_mut(), 0, 10) };
            if slept < 0 && unsafe { errno() } != libc::EINTR {
                unsafe { libc::_exit(127) };
            }
        }
    }

    #[cfg(unix)]
    unsafe fn abort_unacknowledged_holder_or_exit(child: libc::pid_t, read_fd: libc::c_int) {
        let _ = unsafe { libc::close(read_fd) };
        let killed = unsafe { libc::kill(child, libc::SIGKILL) };
        if killed < 0 && unsafe { errno() } != libc::ESRCH {
            unsafe { libc::_exit(127) };
        }
        unsafe { reap_killed_holder_or_exit(child) };
        unsafe { libc::_exit(127) };
    }

    #[cfg(unix)]
    unsafe fn spawn_escaped_pipe_holder_pre_exec(
        marker_fd: libc::c_int,
        release_fd: libc::c_int,
        shell: *const libc::c_char,
        argv: *const *const libc::c_char,
        envp: *const *const libc::c_char,
        ack_timeout_millis: libc::c_int,
        stall_after_marker: bool,
    ) {
        let mut handshake = [-1; 2];
        if unsafe { libc::pipe(handshake.as_mut_ptr()) } < 0
            || unsafe { libc::fcntl(handshake[0], libc::F_SETFD, libc::FD_CLOEXEC) } < 0
            || unsafe { libc::fcntl(handshake[1], libc::F_SETFD, libc::FD_CLOEXEC) } < 0
        {
            unsafe { libc::_exit(127) };
        }
        let child = unsafe { libc::fork() };
        if child < 0 {
            unsafe { libc::_exit(127) };
        }
        if child == 0 {
            unsafe { close_or_exit(handshake[0]) };
            // Keep only stdio plus these fixed descriptors. The grandchild
            // must retain stdout/stderr. Executing the prepared shell below
            // closes Command::spawn's private CLOEXEC exec-status pipe before
            // the shell sends the readiness ACK on descriptor 4.
            let marker_copy = unsafe { duplicate_cloexec_or_exit(marker_fd) };
            let ack_copy = unsafe { duplicate_cloexec_or_exit(handshake[1]) };
            let release_copy = unsafe { duplicate_cloexec_or_exit(release_fd) };
            unsafe { dup_to_or_exit(marker_copy, 3) };
            unsafe { dup_to_or_exit(ack_copy, 4) };
            unsafe { dup_to_or_exit(release_copy, 5) };
            if unsafe { libc::fcntl(4, libc::F_SETFD, 0) } < 0 {
                unsafe { libc::_exit(127) };
            }
            if unsafe { libc::fcntl(5, libc::F_SETFD, 0) } < 0 {
                unsafe { libc::_exit(127) };
            }
            unsafe { write_pid_or_exit(3) };
            if stall_after_marker && unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) } < 0 {
                unsafe { libc::_exit(127) };
            }
            unsafe { close_or_exit(3) };
            unsafe { libc::execve(shell, argv, envp) };
            unsafe { libc::_exit(127) };
        }
        if unsafe { libc::close(handshake[1]) } < 0 {
            unsafe { abort_unacknowledged_holder_or_exit(child, handshake[0]) };
        }
        if !unsafe { wait_for_ack(handshake[0], ack_timeout_millis) }
            || unsafe { libc::close(handshake[0]) } < 0
        {
            unsafe { abort_unacknowledged_holder_or_exit(child, handshake[0]) };
        }
    }

    #[cfg(unix)]
    impl Drop for EscapedPipeHolder {
        fn drop(&mut self) {
            // EOF is an identity-safe release for the exact shell that owns
            // descriptor 5; unlike a recorded numeric PID it cannot target a
            // later, unrelated process after PID reuse.
            self.release_writer.take();
        }
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_timeout_does_not_wait_for_escaped_pipe_holders() {
        let _holder = EscapedPipeHolder::new();
        let mut command = Command::new(std::env::current_exe().expect("current test binary"));
        _holder.install_pre_exec_holder(&mut command, false);
        command
            .arg("--exact")
            .arg("runner::tests::escaped_pipe_holder_helper")
            .arg("--nocapture");
        let started = Instant::now();
        let error = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: Duration::from_millis(100),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BoundedProcessError::Timeout { timeout_ms: 100 }
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout waited for an escaped pipe holder"
        );
        _holder.wait_for_pid();
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_stops_on_escaped_stdout_overflow() {
        let _holder = EscapedPipeHolder::new();
        let mut command = Command::new(std::env::current_exe().expect("current test binary"));
        _holder.install_pre_exec_holder(&mut command, true);
        command
            .arg("--exact")
            .arg("runner::tests::escaped_pipe_holder_helper")
            .arg("--nocapture");
        let started = Instant::now();
        let error = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: Duration::from_secs(2),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BoundedProcessError::OutputLimit {
                stream: "stdout",
                limit: 1024
            }
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        _holder.wait_for_pid();
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_escaped_unread_stdin_returns_without_a_writer_join() {
        let _holder = EscapedPipeHolder::new();
        let mut command = Command::new(std::env::current_exe().expect("current test binary"));
        _holder.install_pre_exec_holder(&mut command, false);
        command
            .arg("--exact")
            .arg("runner::tests::escaped_pipe_holder_helper")
            .arg("--nocapture");
        let started = Instant::now();
        let error = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: Duration::from_millis(250),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
            Some(BoundedStdin::new(
                vec![b'x'; 2 * 1024 * 1024],
                2 * 1024 * 1024,
            )),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BoundedProcessError::Write(_) | BoundedProcessError::Timeout { .. }
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        _holder.wait_for_pid();
    }

    #[cfg(unix)]
    #[test]
    fn escaped_holder_pre_exec_failure_reaps_before_spawn_can_return() {
        let _holder = EscapedPipeHolder::new();
        let mut command = Command::new(std::env::current_exe().expect("current test binary"));
        _holder.install_pre_exec_holder_with_shell(&mut command, false, "/definitely/not/a/shell");
        let started = Instant::now();
        let output = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                // This checks the pre-exec failure path, not the product
                // timeout boundary. A loaded CI worker can spend more than
                // 100 ms scheduling the test binary after `spawn` has safely
                // returned. Keep the independent <2 s assertion below so a
                // stuck pre-exec cleanup remains a deterministic failure.
                timeout: Duration::from_secs(3),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
            None,
        )
        .expect("pre-exec cleanup must let spawn return");
        assert_eq!(output.status.code(), Some(127));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "unacknowledged escaped holder delayed spawn completion"
        );
        _holder.wait_for_pid();
    }

    #[cfg(unix)]
    #[test]
    fn stalled_pre_exec_holder_is_killed_and_reaped_before_spawn_can_return() {
        let _holder = EscapedPipeHolder::new();
        let mut command = Command::new(std::env::current_exe().expect("current test binary"));
        _holder.install_pre_exec_holder_with_options(&mut command, false, "/bin/sh", 250, true);
        let started = Instant::now();
        let output = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: Duration::from_secs(2),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
            None,
        )
        .expect("stalled pre-exec holder must be cleaned before spawn returns");
        assert_eq!(output.status.code(), Some(127));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "stalled pre-exec holder delayed spawn completion"
        );
        _holder.wait_for_pid();
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_stops_on_stdout_overflow() {
        let mut command = shell("while :; do printf x; done");
        let error = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: Duration::from_secs(2),
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BoundedProcessError::OutputLimit {
                stream: "stdout",
                limit: 1024
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_rejects_non_utf8_stdout() {
        let mut command = shell("printf '\\377'");
        let error =
            run_bounded_command(&mut command, BoundedProcessOptions::default(), None).unwrap_err();
        assert!(matches!(
            error,
            BoundedProcessError::NonUtf8 { stream: "stdout" }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_collects_both_terminal_stream_events_repeatedly() {
        for _ in 0..64 {
            let mut command = shell("printf stdout; printf stderr >&2");
            let output = run_bounded_command(&mut command, BoundedProcessOptions::default(), None)
                .expect("both reader threads must deliver their terminal event");
            assert_eq!(output.stdout, "stdout");
            assert_eq!(output.stderr, "stderr");
        }
    }

    #[test]
    fn classifies_zero_and_partial_json_as_scored() {
        for code in [0, 3] {
            let classified = classify_outcome(&outcome(
                code,
                serde_json::json!({"results":[{"evidence_pointer":pointer()}]}),
            ));
            assert!(matches!(classified, ClassifiedOutcome::Scored { .. }));
        }
    }
    #[test]
    fn not_implemented_wins_over_exit_code() {
        let classified = classify_outcome(&outcome(
            1,
            serde_json::json!({"error_code":"KIO-E-SEARCH-NOT-IMPLEMENTED"}),
        ));
        assert!(matches!(
            classified,
            ClassifiedOutcome::Unimplemented { .. }
        ));
    }
    #[test]
    fn malformed_or_incomplete_success_is_failed() {
        let zero = classify_outcome(&outcome(0, serde_json::json!({"results":"bad"})));
        assert!(matches!(
            zero,
            ClassifiedOutcome::Failed { ref detail, .. }
                if detail == "exit 0: JSON response schema invalid: response has no results array"
        ));
        let partial = classify_outcome(&outcome(
            3,
            serde_json::json!({"results":[{"evidence_pointer":{"schema_version":1}}]}),
        ));
        assert!(matches!(
            partial,
            ClassifiedOutcome::Failed { ref detail, .. }
                if detail.starts_with("exit 3: result[0] Evidence Pointer invalid:")
        ));
    }

    #[test]
    fn classifies_plaintext_fallback_with_diagnostic_error_code_as_scored() {
        let classified = classify_outcome(&outcome(
            0,
            serde_json::json!({
                "results": [{
                    "evidence_pointer": pointer(),
                    "title": "Plain text note",
                    "current_path": "note.txt"
                }],
                "fallback": true,
                "error_code": "KIO-E-VECTOR-UNAVAILABLE-001"
            }),
        ));
        assert!(matches!(
            classified,
            ClassifiedOutcome::Scored { error_code: Some(ref code), .. }
                if code == "KIO-E-VECTOR-UNAVAILABLE-001"
        ));
    }

    #[test]
    fn distinguishes_non_json_schema_and_pointer_failures() {
        let non_json = SearchOutcome {
            returncode: 0,
            stdout: "not json".into(),
            stderr: String::new(),
            duration_ms: 1.0,
        };
        assert!(matches!(
            classify_outcome(&non_json),
            ClassifiedOutcome::Failed { ref detail, .. }
                if detail.starts_with("exit 0: stdout is not JSON:")
                    && detail.contains("line 1 column")
        ));

        let truncated = SearchOutcome {
            returncode: 0,
            stdout: format!("{{\"{}\"", "field".repeat(100)),
            stderr: String::new(),
            duration_ms: 1.0,
        };
        assert!(matches!(
            classify_outcome(&truncated),
            ClassifiedOutcome::Failed { ref detail, .. }
                if detail.starts_with("exit 0: stdout is not JSON:")
                    && detail.contains("line 1 column")
                    && detail.chars().count() <= MAX_JSON_ERROR_DETAIL_CHARS + 28
        ));

        let pointer_failure = classify_outcome(&outcome(
            0,
            serde_json::json!({"results":[{"evidence_pointer": {
                "schema_version": 1,
                "commit": format!("sha256:{}", "c".repeat(64)),
                "raw_hash": format!("sha256:{}", "a".repeat(64)),
                "tool_profile_hash": format!("sha256:{}", "b".repeat(64)),
                "chunk_hash": format!("sha256:{}", "d".repeat(64)),
                "scope_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "heading_path": []
            }}]}),
        ));
        assert!(matches!(
            pointer_failure,
            ClassifiedOutcome::Failed { ref detail, .. }
                if detail == "exit 0: result[0] Evidence Pointer invalid: invalid evidence_pointer: pointer heading_path is invalid"
        ));
    }

    #[test]
    fn failure_artifact_omits_missing_error_code_and_preserves_detail() {
        let query = ResolvedQuery {
            scenario: "M3-1".to_owned(),
            query: "fixture query".to_owned(),
            expected: HashSet::new(),
            resolution_error: None,
        };
        let (results, _) = evaluate_queries(std::slice::from_ref(&query), 0.8, |_| {
            Ok(SearchOutcome {
                returncode: 1,
                stdout: String::new(),
                stderr: "plain failure".to_owned(),
                duration_ms: 1.0,
            })
        })
        .unwrap();
        let value = serde_json::to_value(&results).unwrap();
        let result = &value["queries"][0];
        assert!(result.get("error_code").is_none());
        assert_eq!(result["detail"], "exit=1: plain failure");

        let mut unresolved = query.clone();
        unresolved.resolution_error = Some("fixture cannot resolve".to_owned());
        let (results, _) = evaluate_queries(&[unresolved], 0.8, |_| {
            Ok(outcome(
                0,
                serde_json::json!({
                    "results": [],
                    "error_code": "KIO-E-SEARCH-BOOM-001"
                }),
            ))
        })
        .unwrap();
        let value = serde_json::to_value(&results).unwrap();
        assert_eq!(value["queries"][0]["error_code"], "KIO-E-SEARCH-BOOM-001");

        let (results, _) = evaluate_queries_with_validator(
            &[query],
            0.8,
            |_| Ok(outcome(0, serde_json::json!({"results":[]}))),
            |_, _| Err("pointer validation failed".to_owned()),
        )
        .unwrap();
        let value = serde_json::to_value(&results).unwrap();
        assert!(value["queries"][0].get("error_code").is_none());
    }
    #[test]
    fn report_is_deterministic_and_exit_policy_prioritizes_unimplemented() {
        let mut results = EvaluationResults {
            target_recall_at_10: 0.8,
            scenarios: BTreeMap::new(),
            queries: Vec::new(),
            history_coverage: HistoryCoverage::default(),
            counts: Counts {
                n_queries: 1,
                n_unimplemented: 1,
                ..Counts::default()
            },
        };
        results.scenarios.insert(
            "M3-1".to_owned(),
            ScenarioSummary {
                n_queries: 1,
                n_scored: 0,
                recall_at_10: None,
                passes_target: false,
                p95_ms: None,
                latency_target_ms: 5000.0,
                passes_latency: false,
            },
        );
        assert_eq!(final_exit_code(&results, &["M3-1".to_owned()]), 2);
        let path = std::env::temp_dir().join(format!("kio-eval-report-{}", std::process::id()));
        write_report(&path, &results, &["M3-1".to_owned()]).unwrap();
        let first = fs::read(&path).unwrap();
        write_report(&path, &results, &["M3-1".to_owned()]).unwrap();
        assert_eq!(first, fs::read(&path).unwrap());
        assert!(first.ends_with(b"\n\n"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn resolution_error_is_a_failed_zero_after_search_but_not_after_not_implemented() {
        let query = ResolvedQuery {
            scenario: "M3-1".to_owned(),
            query: "fixture query".to_owned(),
            expected: HashSet::new(),
            resolution_error: Some("missing fixture identity".to_owned()),
        };
        let (results, records) = evaluate_queries(std::slice::from_ref(&query), 0.8, |_| {
            Ok(outcome(0, serde_json::json!({"results":[]})))
        })
        .unwrap();
        assert!(records.is_empty());
        assert_eq!(results.counts.n_failed, 1);
        assert_eq!(results.queries[0].recall_at_10, Some(0.0));
        assert_eq!(
            results.queries[0].detail.as_deref(),
            Some("missing fixture identity")
        );
        assert_eq!(final_exit_code(&results, &["M3-1".to_owned()]), 1);

        let (results, _) = evaluate_queries(&[query], 0.8, |_| {
            Ok(outcome(
                1,
                serde_json::json!({"error_code":"KIO-E-SEARCH-NOT-IMPLEMENTED"}),
            ))
        })
        .unwrap();
        assert_eq!(results.counts.n_unimplemented, 1);
        assert_eq!(results.counts.n_failed, 0);
        assert_eq!(final_exit_code(&results, &["M3-1".to_owned()]), 2);
    }

    #[test]
    fn results_json_has_a_single_terminal_newline() {
        let results = EvaluationResults {
            target_recall_at_10: 0.8,
            scenarios: BTreeMap::new(),
            queries: Vec::new(),
            history_coverage: HistoryCoverage::default(),
            counts: Counts::default(),
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("results.json");
        write_results(&path, &results).unwrap();
        let bytes = fs::read(path).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert!(!bytes.ends_with(b"\n\n"));
    }
}
