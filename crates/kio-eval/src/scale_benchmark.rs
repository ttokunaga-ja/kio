//! Descriptor-bound, paired-lane scale-v3 benchmark.

use std::{
    collections::{BTreeSet, HashSet},
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use crate::{
    RecallResult,
    attestation::PointerAttestor,
    d1::{D1_BENCHMARK_ID, D1_SCHEMA_VERSION, D1Report, Measurement},
    process_boundary::{
        DescriptorExecutable, ProcessBoundaryError, configure_descriptor_environment,
        configure_retained_cwd,
    },
    recall_at_k,
    runner::{BoundedProcessOptions, run_bounded_command},
    scale_attest::{
        AttestError, CorpusEvidence, attest_ready, publish_external_artifact,
        validate_benchmark_attestation,
    },
    scale_fixture::{ScaleFixtureError, ValidatedFixture, bind_ready},
    scale_prepare::{BenchmarkDevice, ScalePrepareError, validate_benchmark_prepare_report},
    scale_spec::{self, ScaleLane, ScaleProfile},
};
#[cfg(windows)]
use cap_primitives::fs::_WindowsByHandle;
#[cfg(unix)]
use cap_primitives::fs::MetadataExt;
use cap_primitives::{ambient_authority, fs as cap_fs};
use kio_core::cas::{canonical_json_bytes, hash_bytes};
use kio_index::chunking::slugify_heading;
use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;

const MAX_WARMUPS: usize = 5;
const MAX_SAMPLES: usize = 100;
const MAX_METRICS_LOG_BYTES: u64 = 64 * 1024 * 1024;
const MAX_METRICS_DELTA_BYTES: u64 = 64 * 1024;
const MAX_METRIC_LINE_BYTES: u64 = 32 * 1024;
const RESULT_LIMIT: usize = 10;
const DETERMINISTIC_EMBED_ENV: &str = "KIO_EVAL_DETERMINISTIC_EMBED";
const DETERMINISTIC_EMBED_VALUE: &str = "scale-v3";
#[derive(Debug, Error)]
pub enum ScaleBenchmarkError {
    #[error("invalid scale benchmark input: {0}")]
    Input(String),
    #[error(transparent)]
    Fixture(#[from] ScaleFixtureError),
    #[error(transparent)]
    Attest(#[from] AttestError),
    #[error(transparent)]
    Prepare(#[from] ScalePrepareError),
    #[error("unsafe scale benchmark process boundary: {0}")]
    Boundary(String),
    #[error(transparent)]
    Process(#[from] crate::runner::BoundedProcessError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
#[derive(Debug)]
pub struct BenchmarkSummary {
    pub report: PathBuf,
    pub acceptance_failed: bool,
}
#[derive(Clone, Copy)]
struct LaneSpec {
    name: &'static str,
    mode: &'static str,
    selector: Option<&'static str>,
    history: bool,
    formal_p95_threshold_ms: Option<f64>,
}
#[derive(Clone, Copy)]
struct LaneRun<'a> {
    warmups: usize,
    samples: usize,
    formal: bool,
    binary: &'a str,
}
const LANES: [LaneSpec; 5] = [
    LaneSpec {
        name: "current-text",
        mode: "text",
        selector: None,
        history: false,
        formal_p95_threshold_ms: Some(5_000.0),
    },
    LaneSpec {
        name: "vector",
        mode: "vector",
        selector: None,
        history: false,
        formal_p95_threshold_ms: None,
    },
    LaneSpec {
        name: "hybrid",
        mode: "hybrid",
        selector: None,
        history: false,
        formal_p95_threshold_ms: None,
    },
    LaneSpec {
        name: "history",
        mode: "text",
        selector: Some("--all-history"),
        history: true,
        formal_p95_threshold_ms: Some(7_000.0),
    },
    LaneSpec {
        name: "deleted",
        mode: "text",
        selector: Some("--include-deleted"),
        history: true,
        formal_p95_threshold_ms: Some(7_000.0),
    },
];
#[derive(Serialize)]
struct Report {
    schema_version: u64,
    benchmark: &'static str,
    profile: ScaleProfile,
    paired_fixture_sha256: String,
    paired_attestation_sha256: String,
    binary_sha256: String,
    binary_bytes: u64,
    warmups: usize,
    samples: usize,
    measurement_class: &'static str,
    full_formal_manual_gate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    passed_p95_thresholds: Option<bool>,
    acceptance_failed: bool,
    d1: D1Report,
    platform: Platform,
    lanes: Vec<LaneReport>,
}
#[derive(Serialize)]
struct LaneReport {
    name: &'static str,
    requested_mode: &'static str,
    resolved_mode: &'static str,
    population: u64,
    recall_at_10: f64,
    recall_at_10_passed: bool,
    pointer_attestations: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    restore_raw_verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    restore_working_tree_unchanged: Option<bool>,
    process_wall_statistics_ms: Statistics,
    product_metric_statistics_ms: Statistics,
    #[serde(skip_serializing_if = "Option::is_none")]
    p95_threshold_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    passed_p95_threshold: Option<bool>,
    raw_samples: Vec<Sample>,
    fixture_manifest_sha256: String,
    attestation_sha256: String,
    binary_sha256: String,
    binary_bytes: u64,
}
#[derive(Serialize)]
struct Sample {
    sequence: usize,
    query_index: usize,
    process_wall_duration_ms: f64,
    search_latency_ms: f64,
    recall_at_10: f64,
}
#[derive(Serialize)]
struct Statistics {
    p50: f64,
    p95: f64,
    p99: f64,
    min: f64,
    max: f64,
}
#[derive(Serialize)]
struct Platform {
    os: &'static str,
    arch: &'static str,
    family: &'static str,
}
fn boundary(e: ProcessBoundaryError) -> ScaleBenchmarkError {
    ScaleBenchmarkError::Boundary(e.to_string())
}
fn digest(b: &[u8]) -> String {
    hash_bytes(b)
}
fn stats(v: &[f64]) -> Result<Statistics, ScaleBenchmarkError> {
    if v.is_empty() || v.iter().any(|n| !n.is_finite() || *n < 0.) {
        return Err(ScaleBenchmarkError::Input(
            "latency samples must be nonempty finite values".into(),
        ));
    }
    let p = |f: f64| {
        let mut s = v.to_vec();
        s.sort_by(f64::total_cmp);
        s[((f * s.len() as f64).ceil() as usize).saturating_sub(1)]
    };
    Ok(Statistics {
        p50: p(0.5),
        p95: p(0.95),
        p99: p(0.99),
        min: v.iter().copied().fold(f64::INFINITY, f64::min),
        max: v.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    })
}
fn clean_absolute(p: &Path) -> Result<(), ScaleBenchmarkError> {
    if !p.is_absolute()
        || p.file_name().is_none()
        || p.components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        Err(ScaleBenchmarkError::Input(
            "benchmark output must be a clean absolute path".into(),
        ))
    } else {
        Ok(())
    }
}
fn absolute_bin(bin: &Path) -> Result<PathBuf, ScaleBenchmarkError> {
    if bin.is_absolute() {
        return Ok(bin.to_owned());
    }
    if bin
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(ScaleBenchmarkError::Input(
            "relative binary path must be lexical normal components".into(),
        ));
    }
    Ok(std::env::current_dir()
        .map_err(|e| ScaleBenchmarkError::Input(e.to_string()))?
        .join(bin))
}
fn object<'a>(
    value: &'a Value,
    label: &str,
) -> Result<&'a Map<String, Value>, ScaleBenchmarkError> {
    value
        .as_object()
        .ok_or_else(|| ScaleBenchmarkError::Input(format!("{label} must be an object")))
}
fn exact_keys(
    object: &Map<String, Value>,
    keys: &[&str],
    label: &str,
) -> Result<(), ScaleBenchmarkError> {
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = keys.iter().copied().collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(ScaleBenchmarkError::Input(format!(
            "{label} field set mismatch"
        )));
    }
    Ok(())
}
fn population(s: LaneSpec, e: &CorpusEvidence) -> Result<u64, ScaleBenchmarkError> {
    let n = match s.name {
        "current-text" => e.current_chunks,
        "vector" | "hybrid" => e.embedded_chunks,
        "history" => e.historical_only_chunks,
        "deleted" => e.deleted_chunks,
        _ => 0,
    };
    if n == 0 || (matches!(s.name, "vector" | "hybrid") && n != e.current_chunks) {
        Err(ScaleBenchmarkError::Input(
            "attested lane population is absent or inconsistent".into(),
        ))
    } else {
        Ok(n)
    }
}
fn pointer_key(p: &Value) -> Result<crate::ResultKey, ScaleBenchmarkError> {
    let o = p
        .as_object()
        .ok_or_else(|| ScaleBenchmarkError::Input("result lacks object evidence pointer".into()))?;
    let raw = o
        .get("raw_hash")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ScaleBenchmarkError::Input("pointer lacks raw_hash".into()))?
        .to_owned();
    let section = o
        .get("section_id")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(|v| v.rsplit('/').next().unwrap_or(v).to_owned());
    let path = o
        .get("path_at_commit")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    Ok((raw, section, path))
}
fn expected(
    f: &ValidatedFixture,
    s: LaneSpec,
    q: usize,
) -> Result<crate::ResultKey, ScaleBenchmarkError> {
    let query = f
        .manifest()
        .queries
        .get(q)
        .ok_or_else(|| ScaleBenchmarkError::Input("query schedule exceeded manifest".into()))?;
    let scope = f
        .manifest()
        .scopes
        .iter()
        .find(|x| x.name == query.scope)
        .ok_or_else(|| ScaleBenchmarkError::Input("query scope missing".into()))?;
    let scope_index = f
        .manifest()
        .scopes
        .iter()
        .position(|candidate| candidate.name == scope.name)
        .ok_or_else(|| ScaleBenchmarkError::Input("query scope order is invalid".into()))?;
    let (path, raw, heading) = if s.name == "history" || s.name == "deleted" {
        let file_index = if s.name == "history" { 0 } else { 2 };
        let source = scale_spec::document_path(file_index);
        let h = f
            .manifest()
            .history_operations
            .iter()
            .find(|op| op.scope == scope.name && op.source == source)
            .ok_or_else(|| ScaleBenchmarkError::Input("frozen history operation missing".into()))?;
        (
            source,
            h.before_raw_hash.clone(),
            scale_spec::section_heading(scope_index, file_index, 0),
        )
    } else {
        let file = scope
            .files
            .iter()
            .find(|x| x.path == query.file)
            .ok_or_else(|| ScaleBenchmarkError::Input("current query file missing".into()))?;
        (
            query.file.clone(),
            file.raw_hash.clone(),
            query.heading.clone(),
        )
    };
    Ok((raw, Some(slugify_heading(&heading)), Some(path)))
}

fn query_text(
    f: &ValidatedFixture,
    s: LaneSpec,
    query: &scale_spec::ScaleQuery,
) -> Result<String, ScaleBenchmarkError> {
    if s.name != "deleted" {
        return Ok(query.query.clone());
    }
    let scope_index = f
        .manifest()
        .scopes
        .iter()
        .position(|scope| scope.name == query.scope)
        .ok_or_else(|| ScaleBenchmarkError::Input("query scope order is invalid".into()))?;
    Ok(scale_spec::section_query(scope_index, 2, 0))
}
fn response(
    stdout: &str,
    s: LaneSpec,
    query: &str,
    ids: &BTreeSet<String>,
    attestor: &mut PointerAttestor,
    want: &crate::ResultKey,
) -> Result<(usize, f64, Option<Value>), ScaleBenchmarkError> {
    let v: Value = serde_json::from_str(stdout)
        .map_err(|_| ScaleBenchmarkError::Input("search returned invalid JSON".into()))?;
    let o = v
        .as_object()
        .ok_or_else(|| ScaleBenchmarkError::Input("search response must be object".into()))?;
    if o.get("query").and_then(Value::as_str) != Some(query)
        || o.get("requested_mode").and_then(Value::as_str) != Some(s.mode)
        || o.get("resolved_mode").and_then(Value::as_str) != Some(s.mode)
        || o.get("fallback") != Some(&Value::Bool(false))
        || !o.get("fallback_reason").is_some_and(Value::is_null)
    {
        return Err(ScaleBenchmarkError::Input(
            "explicit search mode resolved through fallback".into(),
        ));
    }
    if o.get("excluded_scopes") != Some(&Value::Array(Vec::new())) {
        return Err(ScaleBenchmarkError::Input(
            "search excluded part of the attested corpus".into(),
        ));
    }
    let got = o
        .get("searched_scopes")
        .and_then(Value::as_array)
        .ok_or_else(|| ScaleBenchmarkError::Input("search response has no searched scopes".into()))?
        .iter()
        .map(|x| {
            x.get("scope_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| ScaleBenchmarkError::Input("searched scope lacks scope_id".into()))
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if got != *ids || got.len() != scale_spec::SCOPE_COUNT {
        return Err(ScaleBenchmarkError::Input(
            "search scope identities differ from attestation".into(),
        ));
    }
    let r = o
        .get("results")
        .and_then(Value::as_array)
        .filter(|x| !x.is_empty() && x.len() <= RESULT_LIMIT)
        .ok_or_else(|| ScaleBenchmarkError::Input("search returned invalid results".into()))?;
    let mut hits = Vec::new();
    let mut matched_pointer = None;
    for result in r {
        let p = result.get("evidence_pointer").ok_or_else(|| {
            ScaleBenchmarkError::Input("search result lacks evidence pointer".into())
        })?;
        attestor.attest(p).map_err(|e| {
            ScaleBenchmarkError::Input(format!("independent pointer attestation failed: {e}"))
        })?;
        let k = pointer_key(p)?;
        if &k == want && matched_pointer.is_none() {
            matched_pointer = Some(p.clone());
        }
        hits.push(RecallResult {
            raw_hash: k.0,
            section_id: k.1,
            heading_path: None,
            path_at_commit: k.2,
        })
    }
    Ok((
        r.len(),
        recall_at_k(&hits, &HashSet::from([want.clone()]), RESULT_LIMIT),
        matched_pointer,
    ))
}

fn verify_deleted_restore(
    fixture: &ValidatedFixture,
    executable: &DescriptorExecutable,
    device: &BenchmarkDevice,
    cwd: &fs::File,
    pointer: &Value,
    expected_raw: &str,
) -> Result<(), ScaleBenchmarkError> {
    fixture.recheck()?;
    executable.recheck_original().map_err(boundary)?;
    let destination = tempfile::tempdir()
        .map_err(|error| ScaleBenchmarkError::Input(format!("restore destination: {error}")))?;
    if destination.path().starts_with(fixture.root())
        || fixture.root().starts_with(destination.path())
    {
        return Err(ScaleBenchmarkError::Input(
            "restore destination overlaps the fixture".into(),
        ));
    }
    let destination_handle = cap_fs::open_ambient_dir(destination.path(), ambient_authority())
        .map_err(|error| ScaleBenchmarkError::Input(format!("restore destination: {error}")))?;
    let destination_before = cap_fs::Metadata::from_file(&destination_handle)
        .map_err(|error| ScaleBenchmarkError::Input(format!("restore destination: {error}")))?;
    let pointer_path = pointer
        .get("path_at_commit")
        .and_then(Value::as_str)
        .filter(|path| {
            !path.is_empty()
                && Path::new(path).components().count() == 1
                && Path::new(path)
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
        })
        .ok_or_else(|| {
            ScaleBenchmarkError::Input("deleted pointer lacks a direct restore path".into())
        })?;
    let mut command = executable.command().map_err(boundary)?;
    command
        .arg("--json")
        .arg("export")
        .arg(serde_json::to_string(pointer)?)
        .arg("--to")
        .arg(destination.path());
    configure_descriptor_environment(&mut command, &device.environment()?).map_err(boundary)?;
    command.env(DETERMINISTIC_EMBED_ENV, DETERMINISTIC_EMBED_VALUE);
    configure_retained_cwd(&mut command, cwd).map_err(boundary)?;
    let output = run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: Duration::from_secs(30),
            max_stdout_bytes: 64 * 1024,
            max_stderr_bytes: 32 * 1024,
        },
        None,
    )?;
    if !output.status.success() {
        return Err(ScaleBenchmarkError::Input(format!(
            "deleted evidence restore failed: {}",
            output.stderr.trim()
        )));
    }
    let receipt: Value = serde_json::from_str(&output.stdout)
        .map_err(|_| ScaleBenchmarkError::Input("restore returned invalid JSON".into()))?;
    if receipt.get("source_kind").and_then(Value::as_str) != Some("evidence")
        || receipt.get("exported_count").and_then(Value::as_u64) != Some(1)
    {
        return Err(ScaleBenchmarkError::Input(
            "restore receipt does not bind one evidence object".into(),
        ));
    }
    let names = cap_fs::read_base_dir(&destination_handle)
        .map_err(|error| ScaleBenchmarkError::Input(format!("restore destination: {error}")))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(|error| {
                    ScaleBenchmarkError::Input(format!("restore destination: {error}"))
                })
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if names != BTreeSet::from([pointer_path.to_owned()]) {
        return Err(ScaleBenchmarkError::Input(
            "restore destination does not contain exactly the pointer path".into(),
        ));
    }
    let metadata = cap_fs::stat(
        &destination_handle,
        Path::new(pointer_path),
        cap_fs::FollowSymlinks::No,
    )
    .map_err(|error| ScaleBenchmarkError::Input(format!("restored raw: {error}")))?;
    #[cfg(unix)]
    let links_are_safe = metadata.nlink() == 1;
    #[cfg(windows)]
    let links_are_safe = metadata.number_of_links() == Some(1);
    #[cfg(not(any(unix, windows)))]
    let links_are_safe = false;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || !links_are_safe
        || metadata.len() > scale_spec::MAX_SOURCE_BYTES as u64
    {
        return Err(ScaleBenchmarkError::Input(
            "restored raw is not a bounded private regular file".into(),
        ));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut restored = cap_fs::open(&destination_handle, Path::new(pointer_path), &options)
        .map_err(|error| ScaleBenchmarkError::Input(format!("restored raw: {error}")))?;
    let opened = cap_fs::Metadata::from_file(&restored)
        .map_err(|error| ScaleBenchmarkError::Input(format!("restored raw: {error}")))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    restored
        .by_ref()
        .take(scale_spec::MAX_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ScaleBenchmarkError::Input(format!("restored raw: {error}")))?;
    let after = cap_fs::stat(
        &destination_handle,
        Path::new(pointer_path),
        cap_fs::FollowSymlinks::No,
    )
    .map_err(|error| ScaleBenchmarkError::Input(format!("restored raw: {error}")))?;
    #[cfg(unix)]
    let opened_links_are_safe = opened.nlink() == 1 && after.nlink() == 1;
    #[cfg(windows)]
    let opened_links_are_safe =
        opened.number_of_links() == Some(1) && after.number_of_links() == Some(1);
    #[cfg(not(any(unix, windows)))]
    let opened_links_are_safe = false;
    if !same_file(&metadata, &opened)
        || !same_file(&opened, &after)
        || !opened_links_are_safe
        || opened.len() != bytes.len() as u64
        || hash_bytes(&bytes) != expected_raw
    {
        return Err(ScaleBenchmarkError::Input(
            "restored raw differs from the independently attested pointer".into(),
        ));
    }
    let destination_after = cap_fs::Metadata::from_file(&destination_handle)
        .map_err(|error| ScaleBenchmarkError::Input(format!("restore destination: {error}")))?;
    if !same_file(&destination_before, &destination_after) {
        return Err(ScaleBenchmarkError::Input(
            "restore destination was replaced".into(),
        ));
    }
    fixture.recheck()?;
    executable.recheck_original().map_err(boundary)?;
    Ok(())
}

struct MetricsSnapshot {
    parent: fs::File,
    before: Option<cap_fs::Metadata>,
    offset: u64,
}
#[cfg(unix)]
fn same_file(a: &cap_fs::Metadata, b: &cap_fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(windows)]
fn same_file(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    matches!(
        (
            left.volume_serial_number(),
            left.file_index(),
            right.volume_serial_number(),
            right.file_index(),
        ),
        (Some(left_volume), Some(left_index), Some(right_volume), Some(right_index))
            if left_volume == right_volume && left_index == right_index
    )
}
#[cfg(not(any(unix, windows)))]
fn same_file(_: &cap_fs::Metadata, _: &cap_fs::Metadata) -> bool {
    false
}
fn open_metrics(parent: &fs::File) -> Result<(fs::File, cap_fs::Metadata), ScaleBenchmarkError> {
    let before = cap_fs::stat(
        parent,
        Path::new("metrics.jsonl"),
        cap_fs::FollowSymlinks::No,
    )
    .map_err(|e| ScaleBenchmarkError::Input(format!("cannot inspect metrics log: {e}")))?;
    #[cfg(unix)]
    let safe_links = before.nlink() == 1;
    #[cfg(windows)]
    let safe_links = before.number_of_links() == Some(1);
    #[cfg(not(any(unix, windows)))]
    let safe_links = false;
    if !before.is_file()
        || before.file_type().is_symlink()
        || !safe_links
        || before.len() > MAX_METRICS_LOG_BYTES
    {
        return Err(ScaleBenchmarkError::Input(
            "metrics log must be a bounded single-link regular file".into(),
        ));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(parent, Path::new("metrics.jsonl"), &options).map_err(|error| {
        ScaleBenchmarkError::Input(format!(
            "cannot open metrics log without following links: {error}"
        ))
    })?;
    let opened = cap_fs::Metadata::from_file(&file).map_err(|error| {
        ScaleBenchmarkError::Input(format!("cannot inspect metrics log: {error}"))
    })?;
    let after = cap_fs::stat(
        parent,
        Path::new("metrics.jsonl"),
        cap_fs::FollowSymlinks::No,
    )
    .map_err(|error| ScaleBenchmarkError::Input(format!("cannot recheck metrics log: {error}")))?;
    #[cfg(unix)]
    let opened_links_safe = opened.nlink() == 1 && after.nlink() == 1;
    #[cfg(windows)]
    let opened_links_safe =
        opened.number_of_links() == Some(1) && after.number_of_links() == Some(1);
    #[cfg(not(any(unix, windows)))]
    let opened_links_safe = false;
    if !opened.is_file()
        || !opened_links_safe
        || !same_file(&before, &opened)
        || !same_file(&opened, &after)
        || before.len() != opened.len()
        || opened.len() != after.len()
    {
        return Err(ScaleBenchmarkError::Input(
            "metrics log changed while opening".into(),
        ));
    }
    Ok((file, opened))
}
fn snapshot_metrics(parent: &fs::File) -> Result<MetricsSnapshot, ScaleBenchmarkError> {
    if matches!(cap_fs::stat(parent, Path::new("metrics.jsonl"), cap_fs::FollowSymlinks::No), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(MetricsSnapshot {
            parent: parent.try_clone()?,
            before: None,
            offset: 0,
        });
    }
    let (mut file, metadata) = open_metrics(parent)?;
    if metadata.len() > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut byte = [0];
        file.read_exact(&mut byte)?;
        if byte != *b"\n" {
            return Err(ScaleBenchmarkError::Input(
                "existing metrics log lacks a final newline".into(),
            ));
        }
    }
    Ok(MetricsSnapshot {
        parent: parent.try_clone()?,
        offset: metadata.len(),
        before: Some(metadata),
    })
}
fn appended_metric(
    snapshot: MetricsSnapshot,
    mode: &str,
    result_count: usize,
) -> Result<f64, ScaleBenchmarkError> {
    let (mut f, current) = open_metrics(&snapshot.parent)?;
    let start = match snapshot.before.as_ref() {
        Some(before) if same_file(before, &current) => snapshot.offset,
        Some(_) => {
            return Err(ScaleBenchmarkError::Input(
                "metrics log replaced during search".into(),
            ));
        }
        None => 0,
    };
    let delta = current
        .len()
        .checked_sub(start)
        .ok_or_else(|| ScaleBenchmarkError::Input("metrics log shrank during search".into()))?;
    if delta == 0 || delta > MAX_METRICS_DELTA_BYTES || delta > MAX_METRIC_LINE_BYTES {
        return Err(ScaleBenchmarkError::Input(
            "search must append one bounded metrics line".into(),
        ));
    }
    f.seek(SeekFrom::Start(start))?;
    let mut raw = vec![0; delta as usize];
    f.read_exact(&mut raw)?;
    let opened_after = cap_fs::Metadata::from_file(&f)?;
    let (_, named_after) = open_metrics(&snapshot.parent)?;
    if !same_file(&current, &opened_after)
        || !same_file(&current, &named_after)
        || opened_after.len() != current.len()
        || named_after.len() != current.len()
    {
        return Err(ScaleBenchmarkError::Input(
            "metrics log changed while reading its append".into(),
        ));
    }
    if !raw.ends_with(b"\n") || raw.iter().filter(|b| **b == b'\n').count() != 1 {
        return Err(ScaleBenchmarkError::Input(
            "search must append exactly one LF-terminated metrics line".into(),
        ));
    }
    let value: Value = serde_json::from_slice(&raw)
        .map_err(|_| ScaleBenchmarkError::Input("search metric is invalid JSON".into()))?;
    let metric = object(&value, "search metric")?;
    exact_keys(
        metric,
        &[
            "ts",
            "level",
            "code",
            "component",
            "message",
            "metric",
            "value",
            "context",
        ],
        "search metric",
    )?;
    if metric
        .get("ts")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        || metric.get("level").and_then(Value::as_str) != Some("info")
        || metric.get("code").and_then(Value::as_str) != Some("KIO-M-SEARCH-001")
        || metric.get("component").and_then(Value::as_str) != Some("search")
        || metric.get("message").and_then(Value::as_str) != Some("search completed")
        || metric.get("metric").and_then(Value::as_str) != Some("search.latency_ms")
    {
        return Err(ScaleBenchmarkError::Input(
            "KIO-M-SEARCH-001 envelope is invalid".into(),
        ));
    }
    let context = object(
        metric
            .get("context")
            .ok_or_else(|| ScaleBenchmarkError::Input("search metric context missing".into()))?,
        "search metric context",
    )?;
    exact_keys(
        context,
        &["mode", "scope_count", "result_count"],
        "search metric context",
    )?;
    if context.get("mode").and_then(Value::as_str) != Some(mode)
        || context.get("scope_count").and_then(Value::as_u64)
            != Some(scale_spec::SCOPE_COUNT as u64)
        || context.get("result_count").and_then(Value::as_u64) != Some(result_count as u64)
    {
        return Err(ScaleBenchmarkError::Input(
            "search metric context disagrees with explicit mode response".into(),
        ));
    }
    metric
        .get("value")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| ScaleBenchmarkError::Input("KIO-M-SEARCH-001 has invalid latency".into()))
}
fn run_lane(
    s: LaneSpec,
    f: &ValidatedFixture,
    e: &CorpusEvidence,
    exe: &DescriptorExecutable,
    run: LaneRun<'_>,
) -> Result<LaneReport, ScaleBenchmarkError> {
    let device = BenchmarkDevice::bind(f)?;
    let metrics = device.metrics_directory(f.root())?;
    let cwd = f.try_clone_scope(&f.manifest().scopes[0].name)?;
    let names = f
        .manifest()
        .scopes
        .iter()
        .map(|x| x.name.clone())
        .collect::<Vec<_>>();
    let mut attestor = PointerAttestor::new(f.root(), &names).map_err(|x| {
        ScaleBenchmarkError::Input(format!("cannot bind independent pointer attestor: {x}"))
    })?;
    let ids = e
        .scopes
        .iter()
        .map(|x| x.scope_id.clone())
        .collect::<BTreeSet<_>>();
    let (mut raw, mut wall, mut metric) = (
        Vec::with_capacity(run.samples),
        Vec::with_capacity(run.samples),
        Vec::with_capacity(run.samples),
    );
    let mut pointer_attestations = 0usize;
    let mut restore_verified = None;
    for i in 0..run.warmups + run.samples {
        f.recheck()?;
        device.recheck(f.root())?;
        exe.recheck_original().map_err(boundary)?;
        let qi = scheduled_query_index(i, run.warmups, f.manifest().queries.len());
        let q = &f.manifest().queries[qi];
        let query = query_text(f, s, q)?;
        let mut c = exe.command().map_err(boundary)?;
        c.arg("--json")
            .arg("search")
            .arg(&query)
            .arg("--all-scopes")
            .arg("--limit")
            .arg("10")
            .arg("--mode")
            .arg(s.mode);
        if let Some(x) = s.selector {
            c.arg(x);
        }
        configure_descriptor_environment(&mut c, &device.environment()?).map_err(boundary)?;
        c.env(DETERMINISTIC_EMBED_ENV, DETERMINISTIC_EMBED_VALUE);
        configure_retained_cwd(&mut c, &cwd).map_err(boundary)?;
        let metric_snapshot = snapshot_metrics(&metrics)?;
        let out = run_bounded_command(
            &mut c,
            BoundedProcessOptions {
                timeout: Duration::from_secs(30),
                max_stdout_bytes: 1024 * 1024,
                max_stderr_bytes: 32 * 1024,
            },
            None,
        )?;
        if !out.status.success() {
            return Err(ScaleBenchmarkError::Input(format!(
                "{} search failed: {}",
                s.name,
                out.stderr.trim()
            )));
        }
        let want = expected(f, s, qi)?;
        let (count, recall, matched_pointer) =
            response(&out.stdout, s, &query, &ids, &mut attestor, &want)?;
        let product_metric = appended_metric(metric_snapshot, s.mode, count)?;
        if i >= run.warmups {
            pointer_attestations = pointer_attestations
                .checked_add(count)
                .ok_or_else(|| ScaleBenchmarkError::Input("pointer count overflow".into()))?;
            if s.name == "deleted" && restore_verified.is_none() {
                let pointer = matched_pointer.ok_or_else(|| {
                    ScaleBenchmarkError::Input(
                        "deleted Recall hit has no restorable evidence pointer".into(),
                    )
                })?;
                verify_deleted_restore(f, exe, &device, &cwd, &pointer, &want.0)?;
                restore_verified = Some(true);
            }
            let ms = out.duration.as_secs_f64() * 1000.;
            wall.push(ms);
            metric.push(product_metric);
            raw.push(Sample {
                sequence: i - run.warmups,
                query_index: qi,
                process_wall_duration_ms: ms,
                search_latency_ms: product_metric,
                recall_at_10: recall,
            })
        }
    }
    if attest_ready(f)? != *e {
        return Err(ScaleBenchmarkError::Input(
            "fixture evidence changed during lane benchmark".into(),
        ));
    }
    let recall_at_10 = raw.iter().map(|x| x.recall_at_10).sum::<f64>() / (run.samples as f64);
    let process_wall_statistics_ms = stats(&wall)?;
    let product_metric_statistics_ms = stats(&metric)?;
    let p95_threshold_ms = if run.formal {
        s.formal_p95_threshold_ms
    } else {
        None
    };
    let passed_p95_threshold = p95_threshold_ms.map(|threshold| {
        process_wall_statistics_ms.p95 < threshold && product_metric_statistics_ms.p95 < threshold
    });
    Ok(LaneReport {
        name: s.name,
        requested_mode: s.mode,
        resolved_mode: s.mode,
        population: population(s, e)?,
        recall_at_10,
        recall_at_10_passed: recall_at_10 == 1.0,
        pointer_attestations,
        restore_raw_verified: restore_verified,
        restore_working_tree_unchanged: restore_verified,
        process_wall_statistics_ms,
        product_metric_statistics_ms,
        p95_threshold_ms,
        passed_p95_threshold,
        raw_samples: raw,
        fixture_manifest_sha256: scale_spec::manifest_hash(f.manifest())
            .map_err(|x| ScaleBenchmarkError::Input(x.to_string()))?,
        attestation_sha256: digest(&validate_benchmark_attestation(f, e)?),
        binary_sha256: run.binary.into(),
        binary_bytes: exe.immutable_binding().bytes,
    })
}

fn scheduled_query_index(iteration: usize, warmups: usize, query_count: usize) -> usize {
    debug_assert!(query_count > 0);
    if iteration < warmups {
        iteration % query_count
    } else {
        (iteration - warmups) % query_count
    }
}
/// Benchmark the exact current and history destinations without automatic lane adoption.
pub fn benchmark(
    current_corpus: &Path,
    history_corpus: &Path,
    bin: &Path,
    warmups: usize,
    samples: usize,
    out: &Path,
) -> Result<BenchmarkSummary, ScaleBenchmarkError> {
    if !(1..=MAX_WARMUPS).contains(&warmups) || !(1..=MAX_SAMPLES).contains(&samples) {
        return Err(ScaleBenchmarkError::Input(
            "warmups must be 1..=5 and samples must be 1..=100".into(),
        ));
    }
    if current_corpus == history_corpus {
        return Err(ScaleBenchmarkError::Input(
            "paired destinations must be distinct".into(),
        ));
    }
    DescriptorExecutable::preflight_platform().map_err(boundary)?;
    let exe = DescriptorExecutable::bind_build_artifact(&absolute_bin(bin)?).map_err(boundary)?;
    let current = bind_ready(current_corpus)?;
    let history = bind_ready(history_corpus)?;
    let current_identity = crate::boundary::directory_identity_from_file(
        &current.try_clone_root()?,
    )
    .map_err(|error| ScaleBenchmarkError::Input(format!("current corpus identity: {error}")))?;
    let history_identity = crate::boundary::directory_identity_from_file(
        &history.try_clone_root()?,
    )
    .map_err(|error| ScaleBenchmarkError::Input(format!("history corpus identity: {error}")))?;
    if current.profile() != history.profile()
        || current.lane() != ScaleLane::CurrentText
        || history.lane() != ScaleLane::HistoryOverlay
        || current_identity.is_none()
        || history_identity.is_none()
        || current_identity == history_identity
    {
        return Err(ScaleBenchmarkError::Input(
            "paired fixtures must be distinct same-profile current-text and history-overlay roots"
                .into(),
        ));
    }
    let _a = current.lock()?;
    let _b = history.lock()?;
    let ce = attest_ready(&current)?;
    let he = attest_ready(&history)?;
    let cp = validate_benchmark_prepare_report(&current, &exe, &ce.scopes, ce.registry_rows)?;
    let hp = validate_benchmark_prepare_report(&history, &exe, &he.scopes, he.registry_rows)?;
    let ca = validate_benchmark_attestation(&current, &ce)?;
    let ha = validate_benchmark_attestation(&history, &he)?;
    let binary = format!("sha256:{}", exe.immutable_binding().sha256);
    let formal = current.profile() == ScaleProfile::Full && warmups == 5 && samples == 100;
    let run = LaneRun {
        warmups,
        samples,
        formal,
        binary: &binary,
    };
    let mut lanes = Vec::new();
    for s in LANES {
        lanes.push(if s.history {
            run_lane(s, &history, &he, &exe, run)?
        } else {
            run_lane(s, &current, &ce, &exe, run)?
        })
    }
    let passed_p95_thresholds = formal.then(|| {
        lanes
            .iter()
            .filter(|lane| lane.p95_threshold_ms.is_some())
            .all(|lane| lane.passed_p95_threshold == Some(true))
    });
    let acceptance_failed = lanes.iter().any(|lane| !lane.recall_at_10_passed)
        || (formal && passed_p95_thresholds != Some(true));
    if attest_ready(&current)? != ce
        || attest_ready(&history)? != he
        || validate_benchmark_prepare_report(&current, &exe, &ce.scopes, ce.registry_rows)? != cp
        || validate_benchmark_prepare_report(&history, &exe, &he.scopes, he.registry_rows)? != hp
        || validate_benchmark_attestation(&current, &ce)? != ca
        || validate_benchmark_attestation(&history, &he)? != ha
    {
        return Err(ScaleBenchmarkError::Input(
            "paired fixture receipts changed during benchmark".into(),
        ));
    }
    let fixture = digest(
        &canonical_json_bytes(&serde_json::json!([
            scale_spec::manifest_hash(current.manifest())
                .map_err(|e| ScaleBenchmarkError::Input(e.to_string()))?,
            scale_spec::manifest_hash(history.manifest())
                .map_err(|e| ScaleBenchmarkError::Input(e.to_string()))?
        ]))
        .map_err(|e| ScaleBenchmarkError::Input(e.to_string()))?,
    );
    let attestation = digest(
        &canonical_json_bytes(&serde_json::json!([digest(&ca), digest(&ha)]))
            .map_err(|e| ScaleBenchmarkError::Input(e.to_string()))?,
    );
    let why = "P4 manual D1 gate was not executed by the scale benchmark".to_owned();
    let d1 = D1Report {
        schema_version: D1_SCHEMA_VERSION,
        benchmark: D1_BENCHMARK_ID.into(),
        binary_sha256: binary.clone(),
        fixture_sha256: fixture.clone(),
        attestation_sha256: attestation.clone(),
        baseline_ttfv_ms: Measurement::NotMeasured {
            reason: why.clone(),
        },
        enriched_ttfv_ms: Measurement::NotMeasured {
            reason: why.clone(),
        },
        preview_cost_micro_usd: Measurement::NotMeasured {
            reason: why.clone(),
        },
        actual_cost_micro_usd: Measurement::NotMeasured { reason: why },
    };
    d1.validate()
        .map_err(|e| ScaleBenchmarkError::Input(e.to_string()))?;
    let report = Report {
        schema_version: scale_spec::SCHEMA_VERSION,
        benchmark: "kio-eval scale benchmark/v3",
        profile: current.profile(),
        paired_fixture_sha256: fixture,
        paired_attestation_sha256: attestation,
        binary_sha256: binary,
        binary_bytes: exe.immutable_binding().bytes,
        warmups,
        samples,
        measurement_class: if formal {
            "full_manual_p4_gate"
        } else if current.profile() == ScaleProfile::Tiny {
            "tiny_smoke"
        } else {
            "full_non_acceptance"
        },
        full_formal_manual_gate: formal,
        passed_p95_thresholds,
        acceptance_failed,
        d1,
        platform: Platform {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            family: std::env::consts::FAMILY,
        },
        lanes,
    };
    let mut bytes = canonical_json_bytes(&serde_json::to_value(report)?)
        .map_err(|e| ScaleBenchmarkError::Input(e.to_string()))?;
    bytes.push(b'\n');
    clean_absolute(out)?;
    publish_external_artifact(out, &[&current, &history], &bytes)?;
    Ok(BenchmarkSummary {
        report: out.to_owned(),
        acceptance_failed,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_are_closed() {
        assert!(
            benchmark(
                Path::new("/a"),
                Path::new("/b"),
                Path::new("/x"),
                0,
                1,
                Path::new("/out")
            )
            .is_err()
        )
    }
    #[test]
    fn stats_use_nearest_rank() {
        let s = stats(&[1., 2., 3., 4.]).unwrap();
        assert_eq!(s.p50, 2.);
        assert_eq!(s.p95, 4.)
    }

    #[test]
    fn measured_query_schedule_starts_at_zero_after_warmups() {
        assert_eq!(scheduled_query_index(0, 1, 20), 0);
        assert_eq!(scheduled_query_index(1, 1, 20), 0);
        assert_eq!(scheduled_query_index(2, 1, 20), 1);
        assert_eq!(scheduled_query_index(5, 5, 20), 0);
        assert_eq!(scheduled_query_index(24, 5, 20), 19);
        assert_eq!(scheduled_query_index(25, 5, 20), 0);
    }
}
