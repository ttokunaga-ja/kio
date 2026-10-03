//! Command-line boundary for the internal evaluator.
//!
//! All fixture parsing and scoring remains in the library.  This module owns
//! only CLI defaults, the fixed synthetic-device process environment, and the
//! small amount of live-CAS orchestration that cannot be expressed as a pure
//! metric.

use std::{
    collections::{BTreeMap, HashMap},
    env,
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use cap_primitives::fs as cap_fs;
use clap::{Parser, Subcommand};
use kio_core::{
    ExitCode,
    cas::{MAX_RAW_OBJECT_BYTES, hash_bytes, read_bounded_regular_file},
};
use kio_eval::{
    acceptance::{
        A01Options, A05Options, A06Options, A09Options, AcceptanceCase, AcceptanceLane,
        AcceptanceRequirement, AcceptanceSubcase, NativeOs, assemble_expected_cases,
        read_acceptance_binding, read_expected_manifest, read_expected_receipt, read_receipt,
        run_a01, run_a05, run_a06, run_a09, write_expected_manifest_create_only,
    },
    acceptance_authenticated_local::{AuthenticatedLocalOptions, run_embedding, run_ocr},
    acceptance_distribution::{ExpectedCaseOptions, write_expected_case},
    acceptance_failure::{FailureOptions, run_a08 as run_a08_failure},
    acceptance_fault::{FaultOptions, run_a10},
    acceptance_local_trust::{LocalTrustOptions, run_a04 as run_local_trust_a04},
    acceptance_native::{NativeOptions, run_a02, run_a03},
    acceptance_office::{A07OfficeOptions, run_a07},
    acceptance_policy::{PolicyOptions, run_a04 as run_policy_a04},
    acceptance_provider::{A08ProviderOptions, Provider, run_a08_provider},
    acceptance_service::{ServiceOptions, run_a03 as run_service_a03, run_a11},
    artifact::CreateOnlyArtifact,
    attestation::{MAX_POINTER_ATTESTATIONS_PER_QUERY, PointerAttestor},
    boundary::{BoundCorpus, BoundScope},
    crossscope::CrossscopeOptions,
    fixture_register::{FixtureMode, FixtureRegisterOptions},
    manifest::{
        HistoryOperation, SCOPES, Scenario, frozen_history_plan, load_corpus_manifest,
        load_golden_queries, load_history_manifest,
    },
    persona_plan::PersonaProfile,
    provider_budget::{initialize_campaign, reserve as reserve_provider_budget},
    qhard::{self, BaselineAttestOptions, BaselineOptions, QhardOptions},
    rerank::{
        FixtureRerankDumpOptions, RerankApplyOptions, RerankApplySummary, RerankDataset,
        RerankDumpOptions, RerankDumpSummary,
    },
    resolver::{CorpusModel, Resolver, validate_query},
    runner::{
        BoundedProcessOptions, ResolvedQuery, ScoredRecord, assess_history_coverage,
        evaluate_queries_with_validator, final_exit_code, run_bounded_command, write_report,
        write_results,
    },
    scale_spec::{ScaleLane, ScaleProfile},
    u7::{self, AdapterCommand, AdapterLimits, Modality},
};
use thiserror::Error;

const MAX_U7_TEXTS: usize = 32;
const MAX_U7_IMAGES: usize = 32;
const MAX_U7_TEXT_BYTES: usize = 16 * 1024;
const MAX_U7_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_U7_TOTAL_INPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_U7_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const U7_HTTP_TIMEOUT: Duration = Duration::from_secs(120);
const U7_REPORT_MAX_BYTES: usize = 64 * 1024;

const DEFAULT_BIN: &str = "target/release/kio";

#[derive(Debug, Parser)]
#[command(name = "kio-eval", about = "Kio synthetic search evaluator")]
pub struct Args {
    #[command(subcommand)]
    command: Option<Commands>,
    #[arg(long)]
    golden: Option<PathBuf>,
    #[arg(long)]
    corpus: Option<PathBuf>,
    #[arg(long)]
    corpus_manifest: Option<PathBuf>,
    #[arg(long)]
    history_manifest: Option<PathBuf>,
    /// Trusted local Kio executable under test. Resource bounds contain
    /// accidental hangs/output floods; they do not sandbox hostile code.
    #[arg(long, default_value = DEFAULT_BIN)]
    bin: PathBuf,
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long)]
    report: Option<PathBuf>,
    #[arg(long, value_parser = parse_scenario)]
    scenario: Vec<String>,
    #[arg(long, default_value_t = 0.8, value_parser = parse_recall)]
    min_recall: f64,
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Rust-owned, pure persona planning artifacts.
    Persona {
        #[command(subcommand)]
        command: PersonaCommands,
    },
    /// Evaluate the frozen cross-scope query supplement without full-suite gates.
    Crossscope {
        #[arg(long, default_value = "eval/golden-queries-crossscope.jsonl")]
        golden: PathBuf,
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long, default_value = DEFAULT_BIN)]
        bin: PathBuf,
        /// New output file. Existing measurements are never overwritten.
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    /// Register an external fixture with a Rust-owned control plane.
    Fixture {
        #[command(subcommand)]
        command: FixtureCommands,
    },
    /// Freeze or score reranker measurements.
    Rerank {
        #[command(subcommand)]
        command: RerankCommands,
    },
    /// Compare a served multimodal embedding endpoint to an explicit reference adapter.
    U7 {
        /// OpenAI-compatible serving endpoint root, for example http://127.0.0.1:8000.
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        model: String,
        /// Python-native reference adapter path. Rust owns all HTTP and verdict logic.
        #[arg(long)]
        reference_adapter: PathBuf,
        /// Absolute Python interpreter containing the reference runtime.
        #[arg(long)]
        reference_python: PathBuf,
        /// Canonical local model directory passed as the adapter's sole argument.
        #[arg(long)]
        reference_model: PathBuf,
        /// Text control. Repeat for additional bounded controls.
        #[arg(long)]
        text: Vec<String>,
        /// Image fixture. Repeat for additional bounded observations.
        #[arg(long)]
        image: Vec<PathBuf>,
        #[arg(long, default_value_t = u7::DEFAULT_THRESHOLD, value_parser = parse_u7_threshold)]
        threshold: f64,
        /// New report file. Existing reports are never overwritten.
        #[arg(long)]
        out: PathBuf,
    },
    /// Rust-owned OCR metric evaluation over a normalized provider response.
    Ocr {
        #[command(subcommand)]
        command: OcrCommands,
    },
    /// Materialize the frozen synthetic evaluation corpus.
    GenerateCorpus {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Rebuild the frozen synthetic history corpus using the Rust-only plan.
    ReplayHistory {
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long)]
        bin: PathBuf,
    },
    /// Scale-fixture lifecycle and deterministic search measurement.
    Scale {
        #[command(subcommand)]
        command: ScaleCommands,
    },
    /// Fixture-B baseline and external Q_hard benchmark operations.
    Benchmark {
        #[command(subcommand)]
        command: BenchmarkCommands,
    },
    /// Deterministically build, package, and verify an unpublished RC candidate.
    Release {
        #[command(subcommand)]
        command: ReleaseCommands,
    },
    /// Verify immutable acceptance bindings and execute one implemented case.
    Acceptance {
        #[command(subcommand)]
        command: AcceptanceCommands,
    },
}

#[derive(Debug, Subcommand)]
enum AcceptanceCommands {
    /// Execute one phase of A08 against managed HTTPS local model peers.
    /// OCR writes a checkpoint; embedding verifies it and emits the final receipt.
    AuthenticatedLocal {
        #[arg(long, value_enum)]
        phase: AuthenticatedLocalPhase,
        #[arg(long)]
        expected_case: PathBuf,
        #[arg(long, value_parser = parse_native_os)]
        os: NativeOs,
        #[arg(long)]
        bin: PathBuf,
        #[arg(long)]
        fixture: PathBuf,
        #[arg(long)]
        work_dir: PathBuf,
        #[arg(long)]
        receipt: PathBuf,
        #[arg(long)]
        ocr_endpoint: String,
        #[arg(long)]
        embedding_endpoint: String,
        #[arg(long)]
        ca_pem: PathBuf,
        #[arg(long)]
        service_identity: PathBuf,
        #[arg(long)]
        service_identity_sha256: String,
    },
    TargetBinding {
        #[arg(long)]
        candidate_sha: String,
        #[arg(long, value_parser = parse_native_os)]
        os: NativeOs,
        #[arg(long)]
        target: String,
        #[arg(long)]
        archive_sha256: String,
        #[arg(long)]
        binary_sha256: String,
        #[arg(long)]
        version: String,
        #[arg(long)]
        out: PathBuf,
    },
    Distribution {
        #[arg(
            long,
            required_unless_present = "expected_case",
            conflicts_with = "expected_case"
        )]
        expected: Option<PathBuf>,
        /// Canonical binding for the requested A12 distribution case.
        #[arg(long, conflicts_with = "expected")]
        expected_case: Option<PathBuf>,
        #[arg(long, value_parser = parse_native_os)]
        os: NativeOs,
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        checksums: PathBuf,
        #[arg(long)]
        repro_archive: PathBuf,
        #[arg(long)]
        repro_checksums: PathBuf,
        #[arg(long)]
        source_repo: PathBuf,
        #[arg(long)]
        expected_lock_sha256: String,
        #[arg(long)]
        work_dir: PathBuf,
        #[arg(long)]
        receipt: PathBuf,
    },
    /// Check native runner prerequisites and write a create-only report.
    Preflight {
        #[arg(long, value_parser = parse_native_os)]
        runner_os: NativeOs,
        #[arg(long)]
        office_converter: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Create or reserve immutable provider-acceptance campaign budget state.
    Budget {
        #[command(subcommand)]
        command: ProviderBudgetCommands,
    },
    /// Generate the complete fixed v1 expected matrix from reviewed,
    /// pre-execution bindings for Linux, macOS, and Windows.
    Expected {
        #[arg(long)]
        binding: PathBuf,
        /// New canonical manifest path; existing output is rejected.
        #[arg(long)]
        out: PathBuf,
    },
    /// Construct one canonical expected receipt from this workflow's reviewed
    /// target binding and immutable fixture/workflow inputs.
    ExpectedCase {
        #[arg(long)]
        target_binding: PathBuf,
        #[arg(long, value_parser = parse_acceptance_case)]
        case: AcceptanceCase,
        #[arg(long, value_parser = parse_acceptance_lane)]
        lane: AcceptanceLane,
        #[arg(long, value_parser = parse_acceptance_subcase)]
        subcase: AcceptanceSubcase,
        #[arg(long, value_parser = parse_native_os)]
        os: NativeOs,
        #[arg(long)]
        fixture_id: String,
        #[arg(long)]
        fixture_sha256: String,
        #[arg(long)]
        workflow_path: String,
        #[arg(long)]
        workflow_sha256: String,
        #[arg(long)]
        workflow_commit: String,
        #[arg(long)]
        run_id: u64,
        #[arg(long)]
        attempt: u32,
        #[arg(long)]
        architecture: String,
        /// SHA-256 of the instrumented contract binary, required only for
        /// native fault-contract requirements.
        #[arg(long)]
        contract_binary_sha256: Option<String>,
        #[arg(long)]
        service_identity_sha256: Option<String>,
        /// New canonical expected-receipt path; existing output is rejected.
        #[arg(long)]
        out: PathBuf,
    },
    /// Assemble the fixed acceptance matrix from canonical, predeclared
    /// single-case expectations. Receipt files are not accepted here.
    AssembleExpected {
        #[arg(long)]
        candidate_sha: String,
        /// One or more canonical files produced by `acceptance expected-case`.
        #[arg(long, required = true, num_args = 1..)]
        expected_case: Vec<PathBuf>,
        /// New canonical expected-manifest path; existing output is rejected.
        #[arg(long)]
        out: PathBuf,
    },
    /// Validate and print the canonical fixed v1 expected-receipt manifest.
    Plan {
        #[arg(long)]
        expected: PathBuf,
    },
    /// Reject incomplete, duplicate, or differently-bound acceptance receipts.
    Verify {
        #[arg(long)]
        expected: PathBuf,
        #[arg(long, required = true)]
        receipt: Vec<PathBuf>,
    },
    /// Execute one acceptance case. A05 does not emit a receipt until its required
    /// release-binary vector/image runtime is configured.
    Run {
        #[arg(
            long,
            required_unless_present = "expected_case",
            conflicts_with = "expected_case"
        )]
        expected: Option<PathBuf>,
        /// Canonical binding for this one requested case; receipt aggregation accepts only --expected.
        #[arg(long, conflicts_with = "expected")]
        expected_case: Option<PathBuf>,
        #[arg(long, value_parser = parse_acceptance_case)]
        case: AcceptanceCase,
        #[arg(long, value_parser = parse_acceptance_lane)]
        lane: AcceptanceLane,
        #[arg(long, default_value = "core", value_parser = parse_acceptance_subcase)]
        subcase: AcceptanceSubcase,
        #[arg(long, value_parser = parse_native_os)]
        os: NativeOs,
        #[arg(long)]
        bin: PathBuf,
        #[arg(long)]
        fixture: PathBuf,
        #[arg(long)]
        work_dir: PathBuf,
        #[arg(long)]
        receipt: PathBuf,
        /// Instrumented contract binary for A10/native/core and
        /// A08/native/mock-failure only.
        #[arg(long)]
        contract_bin: Option<PathBuf>,
        /// Required only for A07 office-real.
        #[arg(long)]
        office_converter: Option<PathBuf>,
        /// Create-only native preflight binding, required only for A07 office-real.
        #[arg(long)]
        preflight_report: Option<PathBuf>,
        /// Required acknowledgement for native scheduler registration in the
        /// A03/A11 service-native lane.
        #[arg(long)]
        allow_native_service: bool,
        /// Required only for the real-provider A08 subcases.
        #[arg(long, value_parser = parse_provider)]
        provider: Option<Provider>,
        /// Reconstructed immutable campaign records; required only for A08.
        #[arg(long)]
        reservation_state_dir: Option<PathBuf>,
        #[arg(long)]
        campaign_id: Option<String>,
        #[arg(long)]
        allocation_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum AuthenticatedLocalPhase {
    Ocr,
    Embedding,
}

#[derive(Debug, Subcommand)]
enum ProviderBudgetCommands {
    /// Bind a new campaign state directory before any provider request can reserve spend.
    Initialize {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        campaign_id: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        cap_microusd: u64,
    },
    /// Reserve an allocation immediately before one paid provider request.
    Reserve {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        campaign_id: String,
        #[arg(long)]
        candidate_sha: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        allocation_id: String,
        #[arg(long)]
        cap_microusd: u64,
        #[arg(long)]
        amount_microusd: u64,
    },
}

#[derive(Debug, Subcommand)]
enum ReleaseCommands {
    /// Print the native Rust target triple for a candidate build.
    NativeTarget,
    /// Print the checked-out candidate's Cargo.lock SHA-256.
    LockSha256 {
        #[arg(long)]
        repo: PathBuf,
    },
    /// Print the SHA-256 identity of one bounded candidate archive.
    ArchiveSha256 {
        #[arg(long)]
        archive: PathBuf,
    },
    /// Install the fixed release-audit tools into a new directory.
    PrepareTools {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        cargo_home: PathBuf,
    },
    /// Create an isolated, lock-bound Cargo cache home for candidate builds.
    PrepareCargoHome {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        source_cargo_home: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Build a bound release-candidate binary for the native target.
    Build {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        candidate_sha: String,
        #[arg(long)]
        target: String,
        #[arg(long)]
        target_dir: PathBuf,
        #[arg(long)]
        cargo_home: PathBuf,
    },
    /// Package one already-bound candidate binary into a deterministic archive.
    Package {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        binary: PathBuf,
        #[arg(long)]
        target: String,
        #[arg(long)]
        tools_dir: PathBuf,
        #[arg(long)]
        cargo_home: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Verify archive layout, canonical JSON, checksums, and embedded binding.
    Verify {
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        checksums: Option<PathBuf>,
        /// Trusted digest retained outside the archive/sidecar pair.
        #[arg(long)]
        expected_archive_sha256: String,
        /// Clean exact-candidate checkout used to rederive source bindings.
        #[arg(long)]
        source_repo: PathBuf,
        #[arg(long)]
        expected_commit: String,
        #[arg(long)]
        expected_lock_sha256: String,
    },
    /// Extract and smoke-test only the verified candidate archive.
    Smoke {
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        checksums: Option<PathBuf>,
        /// Trusted digest retained outside the archive/sidecar pair.
        #[arg(long)]
        expected_archive_sha256: String,
        /// Clean exact-candidate checkout used to rederive source bindings.
        #[arg(long)]
        source_repo: PathBuf,
        #[arg(long)]
        expected_commit: String,
        #[arg(long)]
        expected_lock_sha256: String,
        #[arg(long)]
        work_dir: PathBuf,
        #[arg(long)]
        receipt: Option<PathBuf>,
    },
    /// Require two independently produced candidate output directories to match byte-for-byte.
    Compare {
        #[arg(long)]
        left: PathBuf,
        #[arg(long)]
        right: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum FixtureCommands {
    /// Copy, initialize, index, and attest one absolute external fixture root.
    Register {
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = DEFAULT_BIN)]
        bin: PathBuf,
        #[arg(long, value_parser = parse_fixture_mode)]
        mode: FixtureMode,
        /// Restrict registration to named corpus personas.
        #[arg(long)]
        persona: Vec<String>,
        #[arg(long, default_value_t = 8, value_parser = parse_drain_rounds)]
        drain_rounds: usize,
    },
}

#[derive(Debug, Subcommand)]
enum RerankCommands {
    /// Freeze a closed synthetic or fixture-B candidate pool for offline reranking.
    Dump {
        #[arg(long, value_parser = parse_rerank_dataset)]
        dataset: RerankDataset,
        /// Golden query file; repeat to combine suites. Defaults to the full
        /// and short frozen query sets for the synthetic dataset.
        #[arg(long)]
        golden: Vec<PathBuf>,
        /// Synthetic corpus or fixture-B registration root, depending on --dataset.
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long, default_value = DEFAULT_BIN)]
        bin: PathBuf,
        #[arg(long, default_value_t = 100, value_parser = parse_rerank_limit)]
        limit: usize,
        /// New output file. Existing files are never overwritten.
        #[arg(long)]
        out: PathBuf,
    },
    /// Apply a GPU reranker ordering to a frozen rerank dump and score it.
    Apply {
        /// Output from a current canonical synthetic or fixture-B dump producer.
        #[arg(long)]
        input: PathBuf,
        /// GPU reranker JSON containing the model identity and rankings.
        #[arg(long)]
        output: PathBuf,
        /// Optional new report file. Existing files are never overwritten.
        #[arg(long)]
        report: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum OcrCommands {
    /// Invoke the fixed official Mistral OCR HTTP API and publish one normalized Rust response.
    Provider {
        #[arg(long)]
        document: PathBuf,
        #[arg(long, value_parser = parse_mistral_ocr_model)]
        model: String,
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        include_image_base64: bool,
        /// New normalized response file. Existing files are never overwritten.
        #[arg(long)]
        out: PathBuf,
    },
    /// Render explicit images through the Pillow/reportlab adapter.
    Render {
        /// Absolute Python interpreter containing Pillow and reportlab.
        #[arg(long)]
        python: PathBuf,
        /// Absolute path to the checked-in renderer adapter.
        #[arg(long)]
        adapter: PathBuf,
        #[arg(long)]
        request_id: String,
        /// Explicit image input. Repeat for additional pages.
        #[arg(long)]
        image: Vec<PathBuf>,
        /// New PDF file. Its parent directory must already exist.
        #[arg(long)]
        out: PathBuf,
    },
    /// Evaluate one versioned ground truth and normalized OCR response into a create-only report.
    Evaluate {
        #[arg(long)]
        ground_truth: PathBuf,
        #[arg(long)]
        response: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum PersonaCommands {
    /// Emit one exact canonical persona plan.
    Plan {
        #[arg(long, value_enum)]
        profile: PersonaProfile,
        #[arg(long)]
        out: PathBuf,
    },
    /// Strict-read a plan and emit its deterministic history schedule.
    Schedule {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Strict-read a plan and emit a compact renderer receipt (never corpus bytes).
    Render {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Materialize one exact canonical persona bundle into a new replay root.
    Materialize {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        schedule: PathBuf,
        #[arg(long)]
        render: PathBuf,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        replay_id: String,
    },
    /// Create one exact plan-bound workspace without adopting an existing root.
    Scaffold {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        root: PathBuf,
    },
    /// Independently attest one Rust materialization and publish a report.
    Attest {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Coordinate plan-bound persona and scope writers.
    Lease {
        #[command(subcommand)]
        command: PersonaLeaseCommands,
    },
}

#[derive(Debug, Subcommand)]
enum PersonaLeaseCommands {
    /// Acquire one persona writer lease.
    Claim {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        persona: String,
        #[arg(long)]
        session: String,
        #[arg(long)]
        label: Option<String>,
    },
    /// Show the current persona writer lease without its release token.
    Show {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        persona: String,
    },
    /// Release one persona writer lease.
    Release {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        persona: String,
        #[arg(long)]
        release_token: String,
    },
    /// Force-recover one persona writer lease for the exact session.
    Recover {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        persona: String,
        #[arg(long)]
        session: String,
        #[arg(long)]
        reason: String,
    },
    /// Coordinate writers for one plan-owned scope.
    Scope {
        #[command(subcommand)]
        command: PersonaScopeLeaseCommands,
    },
}

#[derive(Debug, Subcommand)]
enum PersonaScopeLeaseCommands {
    /// Acquire one scope writer lease under an active persona session.
    Claim {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        persona: String,
        #[arg(long)]
        scope_id: String,
        #[arg(long)]
        parent_session: String,
        #[arg(long)]
        worker_session: String,
        #[arg(long)]
        label: Option<String>,
    },
    /// Show the current scope writer lease without its release token.
    Show {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        persona: String,
        #[arg(long)]
        scope_id: String,
    },
    /// Release one scope writer lease.
    Release {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        persona: String,
        #[arg(long)]
        scope_id: String,
        #[arg(long)]
        parent_session: String,
        #[arg(long)]
        release_token: String,
    },
    /// Force-recover one scope writer lease for the exact sessions.
    Recover {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        persona: String,
        #[arg(long)]
        scope_id: String,
        #[arg(long)]
        parent_session: String,
        #[arg(long)]
        worker_session: String,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Debug, Subcommand)]
enum BenchmarkCommands {
    /// Build and seal the macOS comparator runtime manifest around a privileged image build.
    ComparatorRuntime {
        #[command(subcommand)]
        command: ComparatorRuntimeCommands,
    },
    /// Attest matching indexed and pristine fixture-B trees before measurement.
    BaselineAttest {
        #[arg(long, default_value = "eval/golden-queries-fixture-b.jsonl")]
        golden: PathBuf,
        #[arg(long)]
        fixture_root: PathBuf,
        #[arg(long)]
        baseline_corpus: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Compare Kio with Spotlight and ripgrep-all on frozen fixture-B.
    Baseline {
        #[arg(long, default_value = "eval/golden-queries-fixture-b.jsonl")]
        golden: PathBuf,
        #[arg(long)]
        fixture_root: PathBuf,
        #[arg(long)]
        baseline_corpus: PathBuf,
        #[arg(long)]
        attestation: PathBuf,
        #[arg(long, default_value = DEFAULT_BIN)]
        bin: PathBuf,
        #[arg(long, default_value = "mdfind")]
        mdfind: PathBuf,
        /// Administrator-provided sealed runtime containing bin/{rga,
        /// rga-preproc,pandoc,pdftotext,rg} and its Mach-O closure.
        #[arg(long)]
        comparator_runtime: Option<PathBuf>,
        #[arg(long)]
        online_query: bool,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Measure the frozen external raster/vector Q_hard fixture.
    Qhard {
        #[arg(long, default_value = "eval/golden-queries-qhard.jsonl")]
        golden: PathBuf,
        #[arg(long)]
        fixture_root: PathBuf,
        #[arg(long, default_value = "qhard")]
        tree: String,
        #[arg(long, default_value = "qhard")]
        env_name: String,
        #[arg(long)]
        attestation: Option<PathBuf>,
        #[arg(long, default_value = DEFAULT_BIN)]
        bin: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 10)]
        k: usize,
        /// Forward only the named query-embedding credentials, if present.
        #[arg(long)]
        online_query: bool,
        /// Generated synthetic corpus measured in this same evaluator invocation.
        #[arg(long)]
        synthetic_corpus: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum ComparatorRuntimeCommands {
    /// Build, verify, and publish the macOS comparator runtime in one process.
    Install,
}

#[derive(Debug, Subcommand)]
enum ScaleCommands {
    /// Materialize one Rust v3 deterministic scale lane.
    Generate {
        #[arg(long)]
        out: PathBuf,
        #[arg(long, value_enum)]
        profile: ScaleProfile,
        /// Create a current-text baseline or a separately owned history lane.
        #[arg(long, value_enum)]
        lane: ScaleLane,
        /// Reset only a fully validated current Rust-owned fixture.
        #[arg(long)]
        reset_owned: bool,
    },
    /// Initialize and index all scale scopes in an isolated device root.
    Prepare {
        #[arg(long)]
        corpus: PathBuf,
        #[arg(long)]
        bin: PathBuf,
    },
    /// Independently attest a prepared Rust v3 scale fixture.
    Attest {
        #[arg(long)]
        corpus: PathBuf,
        /// Optional create-only external report, or the canonical corpus leaf.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Measure deterministic search latency on paired attested Rust v3 fixtures.
    Benchmark {
        #[arg(long)]
        current_corpus: PathBuf,
        #[arg(long)]
        history_corpus: PathBuf,
        #[arg(long)]
        bin: PathBuf,
        #[arg(long)]
        warmups: usize,
        #[arg(long)]
        samples: usize,
        /// New report file. Existing measurements are never overwritten.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    Input(String),
    #[error(transparent)]
    Manifest(#[from] kio_eval::manifest::ManifestError),
    #[error(transparent)]
    Runner(#[from] kio_eval::runner::RunnerError),
    #[error(transparent)]
    Generator(#[from] kio_eval::generator::GeneratorError),
    #[error(transparent)]
    ScaleFixture(#[from] kio_eval::scale_fixture::ScaleFixtureError),
    #[error(transparent)]
    ScalePrepare(#[from] kio_eval::scale_prepare::ScalePrepareError),
    #[error(transparent)]
    ScaleAttest(#[from] kio_eval::scale_attest::AttestError),
    #[error(transparent)]
    ScaleBenchmark(#[from] kio_eval::scale_benchmark::ScaleBenchmarkError),
    #[error(transparent)]
    Crossscope(#[from] kio_eval::crossscope::CrossscopeError),
    #[error(transparent)]
    Rerank(#[from] kio_eval::rerank::RerankDumpError),
    #[error(transparent)]
    RerankApply(#[from] kio_eval::rerank::RerankApplyError),
    #[error(transparent)]
    FixtureRegister(#[from] kio_eval::fixture_register::FixtureRegisterError),
    #[error(transparent)]
    U7(#[from] kio_eval::u7::U7Error),
    #[error(transparent)]
    Ocr(#[from] kio_eval::ocr_eval::OcrEvalError),
    #[error(transparent)]
    Replay(#[from] kio_eval::replay::ReplayError),
    #[error(transparent)]
    PersonaMaterialize(#[from] kio_eval::persona_materialize::PersonaMaterializeError),
    #[error(transparent)]
    PersonaScaffold(#[from] kio_eval::persona_scaffold::PersonaScaffoldError),
    #[error(transparent)]
    PersonaLease(#[from] kio_eval::persona_lease::PersonaLeaseError),
    #[error(transparent)]
    PersonaAttest(#[from] kio_eval::persona_attest::PersonaAttestError),
    #[error(transparent)]
    Release(#[from] kio_eval::release::ReleaseError),
}

fn generate_corpus(out: PathBuf, force: bool) -> Result<ExitCode, AppError> {
    let summary = kio_eval::generator::generate_corpus(&out, force)?;
    println!("[ok] コーパス生成: {}", summary.output.display());
    println!(
        "     files={} anchors={} scopes={}",
        summary.file_count, summary.anchor_count, summary.scope_count
    );
    for (scope, count) in summary.per_scope {
        println!("       - {scope:12}: {count} files");
    }
    println!("     manifest: {}", summary.manifest_path.display());
    Ok(ExitCode::Success)
}

fn parse_scenario(value: &str) -> Result<String, String> {
    match value {
        "M3-1" | "M3-2" | "M3-3" => Ok(value.to_owned()),
        _ => Err("must be M3-1, M3-2, or M3-3".to_owned()),
    }
}

fn parse_acceptance_case(value: &str) -> Result<AcceptanceCase, String> {
    match value {
        "A01" => Ok(AcceptanceCase::A01),
        "A02" => Ok(AcceptanceCase::A02),
        "A03" => Ok(AcceptanceCase::A03),
        "A04" => Ok(AcceptanceCase::A04),
        "A05" => Ok(AcceptanceCase::A05),
        "A06" => Ok(AcceptanceCase::A06),
        "A07" => Ok(AcceptanceCase::A07),
        "A08" => Ok(AcceptanceCase::A08),
        "A09" => Ok(AcceptanceCase::A09),
        "A10" => Ok(AcceptanceCase::A10),
        "A11" => Ok(AcceptanceCase::A11),
        "A12" => Ok(AcceptanceCase::A12),
        _ => Err("must be one of A01 through A12".into()),
    }
}

fn parse_acceptance_lane(value: &str) -> Result<AcceptanceLane, String> {
    match value {
        "native-contract" => Ok(AcceptanceLane::NativeContract),
        "office-real" => Ok(AcceptanceLane::OfficeReal),
        "service-native" => Ok(AcceptanceLane::ServiceNative),
        "provider-live" => Ok(AcceptanceLane::ProviderLive),
        "distribution" => Ok(AcceptanceLane::Distribution),
        _ => Err(
            "must be native-contract, office-real, service-native, provider-live, or distribution"
                .into(),
        ),
    }
}

fn parse_acceptance_subcase(value: &str) -> Result<AcceptanceSubcase, String> {
    match value {
        "core" => Ok(AcceptanceSubcase::Core),
        "local-trust" => Ok(AcceptanceSubcase::LocalTrust),
        "mock-failure" => Ok(AcceptanceSubcase::MockFailure),
        "mistral" => Ok(AcceptanceSubcase::Mistral),
        "gemini" => Ok(AcceptanceSubcase::Gemini),
        "authenticated-local" => Ok(AcceptanceSubcase::AuthenticatedLocal),
        _ => Err(
            "must be core, local-trust, mock-failure, mistral, gemini, or authenticated-local"
                .into(),
        ),
    }
}

fn parse_provider(value: &str) -> Result<Provider, String> {
    match value {
        "mistral" => Ok(Provider::Mistral),
        "gemini" => Ok(Provider::Gemini),
        _ => Err("must be mistral or gemini".into()),
    }
}

fn parse_native_os(value: &str) -> Result<NativeOs, String> {
    match value {
        "linux" => Ok(NativeOs::Linux),
        "macos" => Ok(NativeOs::Macos),
        "windows" => Ok(NativeOs::Windows),
        _ => Err("must be linux, macos, or windows".into()),
    }
}

fn parse_mistral_ocr_model(value: &str) -> Result<String, String> {
    if value == kio_eval::ocr_eval::MISTRAL_OCR_MODEL {
        Ok(value.to_owned())
    } else {
        Err(format!(
            "OCR provider model must be exactly {}",
            kio_eval::ocr_eval::MISTRAL_OCR_MODEL
        ))
    }
}

fn parse_recall(value: &str) -> Result<f64, String> {
    let value: f64 = value.parse().map_err(|_| "must be a number".to_owned())?;
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err("must be finite and in [0, 1]".to_owned())
    }
}

fn parse_rerank_limit(value: &str) -> Result<usize, String> {
    let value: usize = value.parse().map_err(|_| "must be an integer".to_owned())?;
    if (1..=100).contains(&value) {
        Ok(value)
    } else {
        Err("must be in 1..=100".to_owned())
    }
}

fn parse_rerank_dataset(value: &str) -> Result<RerankDataset, String> {
    value.parse()
}

fn parse_fixture_mode(value: &str) -> Result<FixtureMode, String> {
    value.parse()
}

fn parse_drain_rounds(value: &str) -> Result<usize, String> {
    let value: usize = value.parse().map_err(|_| "must be an integer".to_owned())?;
    if (1..=64).contains(&value) {
        Ok(value)
    } else {
        Err("must be in 1..=64".to_owned())
    }
}

fn parse_u7_threshold(value: &str) -> Result<f64, String> {
    let value: f64 = value.parse().map_err(|_| "must be a number".to_owned())?;
    if value.is_finite() && (0.0 < value && value <= 1.0) {
        Ok(value)
    } else {
        Err("must be finite and in (0, 1]".to_owned())
    }
}

fn print_rerank_summary(summary: &RerankDumpSummary) {
    println!(
        "dumped   : {} queries, {} candidates",
        summary.dumped_queries, summary.candidates
    );
    println!("skipped  : {}", summary.skipped_queries);
    println!(
        "baseline : Recall@10 = {:.4}",
        summary.baseline_recall_at_10
    );
    println!("out      : {}", summary.output.display());
}

fn print_rerank_apply_summary(summary: &RerankApplySummary) {
    println!("model     : {}", summary.model.escape_default());
    println!(
        "queries   : {} ({} ranked)",
        summary.queries, summary.ranked_queries
    );
    println!(
        "Recall@10 : {:.4} -> {:.4}  ({:+.4})",
        summary.before_recall_at_10,
        summary.after_recall_at_10,
        summary.after_recall_at_10 - summary.before_recall_at_10
    );
    println!(
        "improved  : {}   worsened: {}",
        summary.improved, summary.worsened
    );
    for problem in &summary.problems {
        println!("[warn] {}", problem.escape_default());
    }
    if let Some(report) = &summary.report {
        println!("report    : {}", report.display());
    }
}

fn bundled_eval_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../eval")
        .join(name)
}

fn output_is_within_input_root(output: &Path, root: &Path) -> Result<bool, AppError> {
    let absolute = if output.is_absolute() {
        output.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|e| AppError::Input(format!("cannot resolve output path: {e}")))?
            .join(output)
    };
    let parent = absolute
        .parent()
        .ok_or_else(|| AppError::Input("output has no parent".into()))?;
    let parent = fs::canonicalize(parent)
        .map_err(|e| AppError::Input(format!("cannot canonicalize output parent: {e}")))?;
    let root = fs::canonicalize(root)
        .map_err(|e| AppError::Input(format!("cannot canonicalize synthetic corpus: {e}")))?;
    Ok(parent.starts_with(root))
}

fn u7_image_mime(path: &Path) -> Result<&'static str, AppError> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Ok("image/png"),
        Some("jpg" | "jpeg") => Ok("image/jpeg"),
        Some("webp") => Ok("image/webp"),
        _ => Err(AppError::Input(format!(
            "U7 image must have a .png, .jpg, .jpeg, or .webp extension: {}",
            path.display()
        ))),
    }
}

fn u7_served_vector(
    agent: &ureq::Agent,
    base_url: &str,
    request: &u7::ServedRequest,
) -> Result<Vec<f64>, AppError> {
    let url = format!("{}/v1/embeddings", base_url.trim_end_matches('/'));
    let mut response = agent
        .post(&url)
        .send_json(&request.body)
        .map_err(|error| AppError::Input(format!("U7 served request failed: {error}")))?;
    if !response.status().is_success() {
        return Err(AppError::Input(format!(
            "U7 served endpoint returned HTTP {}",
            response.status()
        )));
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_U7_RESPONSE_BYTES as u64)
        .read_to_vec()
        .map_err(|error| AppError::Input(format!("U7 served response exceeds bounds: {error}")))?;
    let value = serde_json::from_slice(&bytes)
        .map_err(|error| AppError::Input(format!("U7 served response is not JSON: {error}")))?;
    Ok(u7::parse_served_embedding(&value)?)
}

fn canonical_explicit_path(
    path: &Path,
    boundary: &str,
    label: &str,
    directory: bool,
) -> Result<PathBuf, AppError> {
    if !path.is_absolute() {
        return Err(AppError::Input(format!(
            "{boundary} {label} must be absolute"
        )));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AppError::Input(format!("cannot inspect {boundary} {label}: {error}")))?;
    let expected_type = if directory {
        metadata.file_type().is_dir()
    } else {
        metadata.file_type().is_file()
    };
    if !expected_type {
        return Err(AppError::Input(format!(
            "{boundary} {label} must be a real {}",
            if directory {
                "directory"
            } else {
                "regular file"
            }
        )));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        AppError::Input(format!("cannot canonicalize {boundary} {label}: {error}"))
    })?;
    if canonical != path {
        return Err(AppError::Input(format!(
            "{boundary} {label} must use its canonical path spelling"
        )));
    }
    Ok(canonical)
}

fn u7_adapter_environment() -> BTreeMap<OsString, OsString> {
    let mut environment = BTreeMap::from([
        (OsString::from("LANG"), OsString::from("C.UTF-8")),
        (OsString::from("LC_ALL"), OsString::from("C.UTF-8")),
        (OsString::from("TZ"), OsString::from("UTC")),
        (
            OsString::from("PYTHONDONTWRITEBYTECODE"),
            OsString::from("1"),
        ),
        (OsString::from("PYTHONNOUSERSITE"), OsString::from("1")),
        (OsString::from("PYTHONSAFEPATH"), OsString::from("1")),
        (
            OsString::from("HF_HUB_DISABLE_TELEMETRY"),
            OsString::from("1"),
        ),
        (OsString::from("HF_HUB_OFFLINE"), OsString::from("1")),
        (OsString::from("TRANSFORMERS_OFFLINE"), OsString::from("1")),
        (
            OsString::from("TOKENIZERS_PARALLELISM"),
            OsString::from("false"),
        ),
    ]);
    for name in [
        "CUDA_VISIBLE_DEVICES",
        "HF_HOME",
        "TORCH_HOME",
        "TRANSFORMERS_CACHE",
        "XDG_CACHE_HOME",
    ] {
        if let Some(value) = env::var_os(name) {
            environment.insert(OsString::from(name), value);
        }
    }
    environment
}

struct U7RunOptions<'a> {
    base_url: &'a str,
    model: &'a str,
    reference_adapter: &'a Path,
    reference_python: &'a Path,
    reference_model: &'a Path,
    texts: &'a [String],
    images: &'a [PathBuf],
    threshold: f64,
    out: &'a Path,
}

fn run_u7(options: U7RunOptions<'_>) -> Result<ExitCode, AppError> {
    let U7RunOptions {
        base_url,
        model,
        reference_adapter,
        reference_python,
        reference_model,
        texts,
        images,
        threshold,
        out,
    } = options;
    if texts.is_empty() {
        return Err(AppError::Input(
            "U7 requires at least one --text control".into(),
        ));
    }
    if texts.len() > MAX_U7_TEXTS || images.len() > MAX_U7_IMAGES {
        return Err(AppError::Input(
            "U7 sample count exceeds its fixed bound".into(),
        ));
    }
    let adapter = canonical_explicit_path(reference_adapter, "U7", "reference adapter", false)?;
    let python = canonical_explicit_path(reference_python, "U7", "reference Python", false)?;
    let reference_model = canonical_explicit_path(reference_model, "U7", "reference model", true)?;
    let text_bytes = texts.iter().try_fold(0_usize, |total, text| {
        if text.len() > MAX_U7_TEXT_BYTES {
            Err(AppError::Input("U7 text control exceeds byte bound".into()))
        } else {
            total
                .checked_add(text.len())
                .filter(|total| *total <= MAX_U7_TOTAL_INPUT_BYTES)
                .ok_or_else(|| AppError::Input("U7 inputs exceed byte bound".into()))
        }
    })?;
    let mut input_bytes = text_bytes;
    let mut image_bytes = Vec::with_capacity(images.len());
    for image in images {
        let bytes =
            read_bounded_regular_file(image, MAX_U7_IMAGE_BYTES as u64).map_err(|error| {
                AppError::Input(format!("cannot read U7 image {}: {error}", image.display()))
            })?;
        input_bytes = input_bytes
            .checked_add(bytes.len())
            .filter(|total| *total <= MAX_U7_TOTAL_INPUT_BYTES)
            .ok_or_else(|| AppError::Input("U7 inputs exceed byte bound".into()))?;
        image_bytes.push(bytes);
    }

    let mut reference_requests = Vec::with_capacity(texts.len() + images.len());
    for (index, text) in texts.iter().enumerate() {
        reference_requests.push(u7::reference_text_request(format!("text-{index}"), text));
    }
    for (index, (path, bytes)) in images.iter().zip(&image_bytes).enumerate() {
        let mime = u7_image_mime(path)?;
        reference_requests.push(u7::reference_image_request(
            format!("image-{index}"),
            mime,
            bytes,
        ));
    }
    u7::validate_adapter_requests(&reference_requests)?;

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(U7_HTTP_TIMEOUT))
        .build()
        .into();
    let mut served = Vec::with_capacity(texts.len() + images.len());
    for text in texts {
        let vector = u7_served_vector(&agent, base_url, &u7::served_text_request(model, text))?;
        served.push(vector);
    }
    for (path, bytes) in images.iter().zip(&image_bytes) {
        let mime = u7_image_mime(path)?;
        let vector = u7_served_vector(
            &agent,
            base_url,
            &u7::served_image_request(model, mime, bytes),
        )?;
        served.push(vector);
    }
    let dimensions = served
        .first()
        .map(Vec::len)
        .ok_or_else(|| AppError::Input("U7 has no samples".into()))?;
    if served.iter().any(|vector| vector.len() != dimensions) {
        return Err(AppError::Input(
            "U7 served responses do not share one native dimension".into(),
        ));
    }
    let requests = reference_requests
        .into_iter()
        .zip(served.iter().map(Vec::len))
        .collect::<Vec<_>>();
    let adapter_vectors = u7::run_adapter(
        &AdapterCommand {
            program: python,
            args: vec![
                adapter.to_string_lossy().into_owned(),
                reference_model.to_string_lossy().into_owned(),
            ],
            environment: u7_adapter_environment(),
        },
        &requests,
        &AdapterLimits::default(),
    )?;
    let mut text_scores = Vec::new();
    let mut image_scores = Vec::new();
    for ((request, _), (served, reference)) in
        requests.iter().zip(served.iter().zip(adapter_vectors))
    {
        let score = u7::cosine(served, &reference)?;
        match request.identity.modality {
            Modality::Text => text_scores.push(score),
            Modality::Image => image_scores.push(score),
        }
    }
    let report = u7::report(
        model,
        reference_model.display().to_string(),
        dimensions,
        &text_scores,
        &image_scores,
        threshold,
    )?;
    let input_root = env::current_dir()
        .map_err(|error| AppError::Input(format!("cannot resolve U7 input root: {error}")))?;
    let artifact = CreateOnlyArtifact::bind(out, &input_root, "U7 report")
        .map_err(|error| AppError::Input(error.to_string()))?;
    let mut report_bytes = serde_jcs::to_vec(&report)
        .map_err(|error| AppError::Input(format!("cannot canonicalize U7 report: {error}")))?;
    report_bytes.push(b'\n');
    artifact
        .publish(&report_bytes, U7_REPORT_MAX_BYTES)
        .map_err(|error| AppError::Input(error.to_string()))?;
    println!("report : {}", artifact.public_path().display());
    println!("verdict: {:?}", report.verdict.reason);
    Ok(if report.verdict.adoptable {
        ExitCode::Success
    } else {
        ExitCode::Failure
    })
}

fn run_ocr_evaluate(
    ground_truth: &Path,
    response: &Path,
    out: &Path,
) -> Result<ExitCode, AppError> {
    let truth_bytes =
        read_bounded_regular_file(ground_truth, kio_eval::ocr_eval::MAX_JSON_BYTES as u64)
            .map_err(|error| AppError::Input(format!("cannot read OCR ground truth: {error}")))?;
    let response_bytes =
        read_bounded_regular_file(response, kio_eval::ocr_eval::MAX_JSON_BYTES as u64)
            .map_err(|error| AppError::Input(format!("cannot read OCR response: {error}")))?;
    let truth = kio_eval::ocr_eval::parse_ground_truth(&truth_bytes)?;
    let response = kio_eval::ocr_eval::parse_response(&response_bytes)?;
    let report = kio_eval::ocr_eval::evaluate(&truth, &response)?;
    kio_eval::ocr_eval::write_report_create_only(out, &report)?;
    println!("report : {}", out.display());
    println!("verdict: {:?}", report.verdict);
    Ok(
        if matches!(report.verdict, kio_eval::ocr_eval::Verdict::Passed) {
            ExitCode::Success
        } else {
            ExitCode::Failure
        },
    )
}

fn canonical_new_output(path: &Path, boundary: &str) -> Result<PathBuf, AppError> {
    if !path.is_absolute() {
        return Err(AppError::Input(format!(
            "{boundary} output must be absolute"
        )));
    }
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Input(format!("{boundary} output has no parent")))?;
    let canonical_parent = fs::canonicalize(parent).map_err(|error| {
        AppError::Input(format!(
            "cannot canonicalize {boundary} output parent: {error}"
        ))
    })?;
    if canonical_parent != parent {
        return Err(AppError::Input(format!(
            "{boundary} output parent must use its canonical path spelling"
        )));
    }
    let leaf = path
        .file_name()
        .ok_or_else(|| AppError::Input(format!("{boundary} output has no filename")))?;
    let mut components = Path::new(leaf).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(AppError::Input(format!(
            "{boundary} output filename is unsafe"
        )));
    }
    Ok(canonical_parent.join(leaf))
}

fn run_ocr_provider(
    document: &Path,
    model: &str,
    request_id: &str,
    include_image_base64: bool,
    out: &Path,
) -> Result<ExitCode, AppError> {
    let document = canonical_explicit_path(document, "OCR", "document", false)?;
    let out = canonical_new_output(out, "OCR provider")?;
    let api_key = env::var("MISTRAL_API_KEY")
        .ok()
        .ok_or_else(|| AppError::Input("OCR provider requires MISTRAL_API_KEY".into()))?;
    let normalized = kio_eval::ocr_eval::request_mistral_ocr(
        request_id,
        model,
        &document,
        include_image_base64,
        &api_key,
    )?;
    kio_eval::ocr_eval::write_response_create_only(&out, &normalized)?;
    println!("response: {}", out.display());
    Ok(ExitCode::Success)
}

fn run_ocr_renderer(
    python: &Path,
    adapter: &Path,
    request_id: &str,
    images: &[PathBuf],
    out: &Path,
) -> Result<ExitCode, AppError> {
    let python = canonical_explicit_path(python, "OCR", "renderer Python", false)?;
    let adapter = canonical_explicit_path(adapter, "OCR", "renderer adapter", false)?;
    let out = canonical_new_output(out, "OCR renderer")?;
    let images = images
        .iter()
        .map(|path| canonical_explicit_path(path, "OCR", "renderer image", false))
        .collect::<Result<Vec<_>, _>>()?;
    let request = kio_eval::ocr_eval::RenderRequest {
        schema: kio_eval::ocr_eval::RENDER_REQUEST_SCHEMA.into(),
        request_id: request_id.to_owned(),
        output_pdf: out.to_string_lossy().into_owned(),
        input_images: images
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
    };
    let response = kio_eval::ocr_eval::invoke_renderer(
        &kio_eval::ocr_eval::RendererCommand { python, adapter },
        &request,
        &kio_eval::ocr_eval::RendererLimits::default(),
    )?;
    println!("rendered: {}", out.display());
    println!("sha256 : {}", response.output_sha256);
    Ok(ExitCode::Success)
}

fn scenario_name(value: Scenario) -> &'static str {
    match value {
        Scenario::M3_1 => "M3-1",
        Scenario::M3_2 => "M3-2",
        Scenario::M3_3 => "M3-3",
    }
}

fn expected_flags(query: &kio_eval::manifest::GoldenQuery) -> Vec<String> {
    let mut flags = query.flags.clone();
    if let Some(required) = query.scenario.required_flag()
        && !flags.iter().any(|flag| flag == required)
    {
        flags.push(required.to_owned());
    }
    flags
}

fn verify_logs(
    bin: &Path,
    corpus: &BoundCorpus,
    history: &kio_eval::manifest::HistoryManifest,
    environment: &[(OsString, OsString)],
) -> Vec<String> {
    let mut problems = Vec::new();
    for scope in SCOPES {
        let Some(bound_scope) = corpus.scope(scope) else {
            problems.push(format!("history log unavailable: {scope}"));
            continue;
        };
        let mut command = Command::new(bin);
        command
            .arg("--json")
            .arg("log")
            .env_clear()
            .envs(environment.iter().cloned());
        if bound_scope.configure_command_cwd(&mut command).is_err() {
            problems.push(format!("history log unavailable: {scope}"));
            continue;
        }
        let output = run_bounded_command(&mut command, BoundedProcessOptions::default(), None);
        let Ok(output) = output else {
            problems.push(format!("history log unavailable: {scope}"));
            continue;
        };
        let response: Option<serde_json::Value> = serde_json::from_str(output.stdout.trim()).ok();
        let commits = response
            .as_ref()
            .and_then(|value| value.get("commits"))
            .and_then(serde_json::Value::as_array);
        let Some(commits) = commits else {
            problems.push(format!("history log unavailable: {scope}"));
            continue;
        };
        let messages = commits
            .iter()
            .map(|commit| commit.get("message").and_then(serde_json::Value::as_str))
            .collect::<Option<Vec<_>>>();
        let expected = &history.verified[scope];
        let expected_messages = expected
            .messages
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        if !output.status.success()
            || commits.len() != expected.commit_count
            || messages.as_deref() != Some(expected_messages.as_slice())
        {
            problems.push(format!("history log is stale: {scope}"));
        }
    }
    problems
}

fn read_bound_regular(scope: &BoundScope, name: &std::ffi::OsStr) -> Result<Vec<u8>, AppError> {
    let handle = scope
        .try_clone_handle()
        .map_err(|error| AppError::Input(error.to_string()))?;
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(&handle, Path::new(name), &options)
        .map_err(|error| AppError::Input(error.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|error| AppError::Input(error.to_string()))?;
    if !metadata.is_file() || metadata.len() > MAX_RAW_OBJECT_BYTES {
        return Err(AppError::Input(format!(
            "unsafe or oversized corpus file: {}/{}",
            scope.name(),
            name.to_string_lossy()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(AppError::Input(format!(
                "multiply-linked corpus file: {}/{}",
                scope.name(),
                name.to_string_lossy()
            )));
        }
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_RAW_OBJECT_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::Input(error.to_string()))?;
    if bytes.len() as u64 > MAX_RAW_OBJECT_BYTES {
        return Err(AppError::Input(format!(
            "oversized corpus file: {}/{}",
            scope.name(),
            name.to_string_lossy()
        )));
    }
    Ok(bytes)
}

fn tree_fingerprint(corpus: &BoundCorpus) -> Result<Vec<(String, String, String)>, AppError> {
    let mut rows = Vec::new();
    for scope in corpus.scopes() {
        let handle = scope
            .try_clone_handle()
            .map_err(|error| AppError::Input(error.to_string()))?;
        let entries = cap_fs::read_base_dir(&handle)
            .map_err(|error| AppError::Input(format!("cannot list {}: {error}", scope.name())))?;
        let mut entries = entries
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Input(format!("cannot list {}: {error}", scope.name())))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if entry.file_name() == ".kio"
                || !entry
                    .file_type()
                    .map_err(|error| AppError::Input(error.to_string()))?
                    .is_file()
            {
                continue;
            }
            let name = entry.file_name();
            let bytes = read_bound_regular(scope, &name)?;
            rows.push((
                scope.name().to_owned(),
                name.to_string_lossy().into_owned(),
                hash_bytes(&bytes),
            ));
        }
    }
    Ok(rows)
}

fn verify_restore(
    bin: &Path,
    corpus: &BoundCorpus,
    records: &[ScoredRecord],
    environment: &[(OsString, OsString)],
) -> Vec<String> {
    let before = match tree_fingerprint(corpus) {
        Ok(value) => value,
        Err(error) => return vec![error.to_string()],
    };
    let mut pointers = HashMap::new();
    for record in records.iter().filter(|record| record.scenario == "M3-3") {
        for hit in record.response.results.iter().take(10) {
            if record.expected.contains(
                &kio_eval::RecallResult {
                    raw_hash: hit.pointer.raw_hash.clone(),
                    section_id: hit.pointer.section_id.clone(),
                    heading_path: hit.pointer.heading_path.clone(),
                    path_at_commit: hit.pointer.path_at_commit.clone(),
                }
                .key(),
            ) {
                pointers
                    .entry(hit.pointer.raw_hash.clone())
                    .or_insert_with(|| hit.pointer_value.clone());
            }
        }
    }
    let mut problems = Vec::new();
    let plan = match frozen_history_plan() {
        Ok(plan) => plan,
        Err(error) => return vec![error.to_string()],
    };
    for entry in plan
        .operations
        .iter()
        .filter_map(|operation| match operation {
            HistoryOperation::Delete {
                scope,
                file,
                before_raw_sha256,
                ..
            } => Some((scope, file, before_raw_sha256)),
            _ => None,
        })
    {
        let (scope, file, before_raw_sha256) = entry;
        let raw = format!("sha256:{before_raw_sha256}");
        let Some(pointer) = pointers.get(&raw) else {
            problems.push(format!(
                "deleted result absent for restore: {}/{}",
                scope, file
            ));
            continue;
        };
        let destination = tempfile::tempdir();
        let Ok(destination) = destination else {
            problems.push("could not create restore destination".to_owned());
            continue;
        };
        let pointer = serde_json::to_string(pointer).expect("JSON value serializes");
        let Some(research) = corpus.scope("research") else {
            problems.push("research scope is unavailable for restore".to_owned());
            break;
        };
        let mut command = Command::new(bin);
        command
            .arg("--json")
            .arg("export")
            .arg(pointer)
            .arg("--to")
            .arg(destination.path())
            .env_clear()
            .envs(environment.iter().cloned());
        if let Err(error) = research.configure_command_cwd(&mut command) {
            problems.push(format!("restore failed for {}/{}: {error}", scope, file));
            continue;
        }
        let output = run_bounded_command(&mut command, BoundedProcessOptions::default(), None);
        let Ok(output) = output else {
            problems.push(format!(
                "restore failed for {}/{}: {}",
                scope,
                file,
                output.unwrap_err()
            ));
            continue;
        };
        if !output.status.success() {
            problems.push(format!(
                "restore failed for {}/{}: {}",
                scope,
                file,
                output.stderr.trim()
            ));
            continue;
        }
        let files = walk_regular_files(destination.path());
        if files.len() != 1 {
            problems.push(format!("restore count mismatch for {}/{}", scope, file));
            continue;
        }
        match read_bounded_regular_file(&files[0], MAX_RAW_OBJECT_BYTES)
            .map(|bytes| hash_bytes(&bytes))
        {
            Ok(actual) if actual == raw => {}
            _ => problems.push(format!("restore hash mismatch for {}/{}", scope, file)),
        }
    }
    match tree_fingerprint(corpus) {
        Ok(after) if after == before => {}
        Ok(_) => problems.push("restore mutated the source corpus working tree".to_owned()),
        Err(error) => problems.push(error.to_string()),
    }
    problems
}

fn walk_regular_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_file()) {
            files.push(entry.path());
        } else if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            files.extend(walk_regular_files(&entry.path()));
        }
    }
    files
}

pub fn run(args: Args) -> Result<ExitCode, AppError> {
    if let Some(command) = args.command.as_ref() {
        return match command {
            Commands::Persona { command } => run_persona(command),
            Commands::Fixture { command } => match command {
                FixtureCommands::Register {
                    corpus,
                    out,
                    bin,
                    mode,
                    persona,
                    drain_rounds,
                } => {
                    let summary = kio_eval::fixture_register::register(FixtureRegisterOptions {
                        corpus: corpus.clone(),
                        out: out.clone(),
                        bin: bin.clone(),
                        mode: *mode,
                        personas: persona.clone(),
                        drain_rounds: *drain_rounds,
                    })?;
                    println!(
                        "fixture: {} scopes, {} indexed, {} pending",
                        summary.scopes, summary.indexed, summary.pending
                    );
                    println!("report : {}", summary.report.display());
                    Ok(
                        if summary.indexed == summary.scopes && summary.pending == 0 {
                            ExitCode::Success
                        } else {
                            ExitCode::Failure
                        },
                    )
                }
            },
            Commands::Rerank { command } => match command {
                RerankCommands::Dump {
                    dataset,
                    golden,
                    corpus,
                    bin,
                    limit,
                    out,
                } => {
                    let summary = match dataset {
                        RerankDataset::Synthetic => {
                            let golden = if golden.is_empty() {
                                vec![
                                    bundled_eval_path("golden-queries.jsonl"),
                                    bundled_eval_path("golden-queries-short.jsonl"),
                                ]
                            } else {
                                golden.clone()
                            };
                            kio_eval::rerank::run(RerankDumpOptions {
                                golden,
                                corpus: corpus.clone(),
                                bin: bin.clone(),
                                limit: *limit,
                                out: out.clone(),
                            })?
                        }
                        RerankDataset::FixtureB => {
                            if golden.len() != 1 {
                                return Err(AppError::Input(
                                    "rerank dump --dataset fixture-b requires exactly one --golden"
                                        .into(),
                                ));
                            }
                            let gemini_api_key = env::var_os("GEMINI_API_KEY")
                                .filter(|value| !value.is_empty())
                                .ok_or_else(|| {
                                    AppError::Input(
                                        "rerank dump --dataset fixture-b requires GEMINI_API_KEY for query embeddings"
                                            .into(),
                                    )
                                })?;
                            kio_eval::rerank::run_fixture_b(FixtureRerankDumpOptions {
                                root: corpus.clone(),
                                golden: golden[0].clone(),
                                bin: bin.clone(),
                                limit: *limit,
                                out: out.clone(),
                                gemini_api_key,
                            })?
                        }
                    };
                    print_rerank_summary(&summary);
                    Ok(ExitCode::Success)
                }
                RerankCommands::Apply {
                    input,
                    output,
                    report,
                } => {
                    let summary = kio_eval::rerank::apply(RerankApplyOptions {
                        input: input.clone(),
                        output: output.clone(),
                        report: report.clone(),
                    })?;
                    let succeeded = summary.ranked_queries > 0 && summary.problems.is_empty();
                    print_rerank_apply_summary(&summary);
                    Ok(if succeeded {
                        ExitCode::Success
                    } else {
                        ExitCode::Failure
                    })
                }
            },
            Commands::U7 {
                base_url,
                model,
                reference_adapter,
                reference_python,
                reference_model,
                text,
                image,
                threshold,
                out,
            } => run_u7(U7RunOptions {
                base_url,
                model,
                reference_adapter,
                reference_python,
                reference_model,
                texts: text,
                images: image,
                threshold: *threshold,
                out,
            }),
            Commands::Ocr { command } => match command {
                OcrCommands::Provider {
                    document,
                    model,
                    request_id,
                    include_image_base64,
                    out,
                } => run_ocr_provider(document, model, request_id, *include_image_base64, out),
                OcrCommands::Render {
                    python,
                    adapter,
                    request_id,
                    image,
                    out,
                } => run_ocr_renderer(python, adapter, request_id, image, out),
                OcrCommands::Evaluate {
                    ground_truth,
                    response,
                    out,
                } => run_ocr_evaluate(ground_truth, response, out),
            },
            Commands::Crossscope {
                golden,
                corpus,
                bin,
                out,
                dry_run,
            } => {
                let code = kio_eval::crossscope::run(CrossscopeOptions {
                    golden: golden.clone(),
                    corpus: corpus.clone(),
                    bin: bin.clone(),
                    out: out.clone(),
                    dry_run: *dry_run,
                })?;
                Ok(code)
            }
            Commands::GenerateCorpus { out, force } => generate_corpus(out.clone(), *force),
            Commands::ReplayHistory { corpus, bin } => {
                let summary = kio_eval::replay::replay_history(corpus, bin)?;
                println!(
                    "[ok] history replay: scopes={} commits={}",
                    summary.scopes, summary.commits
                );
                println!("     manifest: {}", summary.manifest.display());
                Ok(ExitCode::Success)
            }
            Commands::Scale { command } => match command {
                ScaleCommands::Generate {
                    out,
                    profile,
                    lane,
                    reset_owned,
                } => {
                    let outcome =
                        kio_eval::scale_fixture::generate(out, *profile, *lane, *reset_owned)?;
                    println!("[ok] scale fixture {outcome:?}: {}", out.display());
                    Ok(ExitCode::Success)
                }
                ScaleCommands::Prepare { corpus, bin } => {
                    let summary = kio_eval::scale_prepare::prepare(corpus, bin)?;
                    println!("[ok] scale fixture prepared: {}", summary.corpus.display());
                    println!(
                        "     initialized={} indexed={}",
                        summary.initialized_scopes, summary.indexed_scopes
                    );
                    println!("     report: {}", summary.report.display());
                    Ok(ExitCode::Success)
                }
                ScaleCommands::Attest { corpus, out } => {
                    let summary =
                        kio_eval::scale_attest::attest_and_publish(corpus, out.as_deref())?;
                    println!("[ok] scale fixture attested: {}", summary.corpus.display());
                    println!(
                        "     scopes={} current_chunks={}",
                        summary.scopes, summary.current_chunks
                    );
                    println!("     report: {}", summary.report.display());
                    Ok(ExitCode::Success)
                }
                ScaleCommands::Benchmark {
                    current_corpus,
                    history_corpus,
                    bin,
                    warmups,
                    samples,
                    out,
                } => {
                    let summary = kio_eval::scale_benchmark::benchmark(
                        current_corpus,
                        history_corpus,
                        bin,
                        *warmups,
                        *samples,
                        out,
                    )?;
                    println!("[ok] scale benchmark: {}", summary.report.display());
                    Ok(if summary.acceptance_failed {
                        ExitCode::Failure
                    } else {
                        ExitCode::Success
                    })
                }
            },
            Commands::Benchmark { command } => match command {
                BenchmarkCommands::ComparatorRuntime { command } => match command {
                    ComparatorRuntimeCommands::Install => {
                        let summary = qhard::install_comparator_runtime()
                            .map_err(|error| AppError::Input(error.to_string()))?;
                        println!("[ok] comparator runtime installed: {summary:?}");
                        Ok(ExitCode::Success)
                    }
                },
                BenchmarkCommands::BaselineAttest {
                    golden,
                    fixture_root,
                    baseline_corpus,
                    out,
                } => {
                    let bytes = qhard::generate_baseline_attestation(BaselineAttestOptions {
                        golden: golden.clone(),
                        fixture_root: fixture_root.clone(),
                        baseline_corpus: baseline_corpus.clone(),
                    })
                    .map_err(|e| AppError::Input(e.to_string()))?;
                    qhard::write_baseline_attestation(out, fixture_root, baseline_corpus, &bytes)
                        .map_err(|e| AppError::Input(e.to_string()))?;
                    Ok(ExitCode::Success)
                }
                BenchmarkCommands::Baseline {
                    golden,
                    fixture_root,
                    baseline_corpus,
                    attestation,
                    bin,
                    mdfind,
                    comparator_runtime,
                    online_query,
                    out,
                } => {
                    let report = qhard::run_baseline(BaselineOptions {
                        golden: golden.clone(),
                        fixture_root: fixture_root.clone(),
                        baseline_corpus: baseline_corpus.clone(),
                        attestation: Some(attestation.clone()),
                        bin: bin.clone(),
                        mdfind: mdfind.clone(),
                        comparator_runtime: comparator_runtime.clone(),
                        online_query: *online_query,
                    })
                    .map_err(|e| AppError::Input(e.to_string()))?;
                    let rendered = serde_json::to_vec_pretty(&report)
                        .map_err(|e| AppError::Input(e.to_string()))?;
                    if let Some(path) = out {
                        qhard::write_baseline_report(path, fixture_root, baseline_corpus, &report)
                            .map_err(|e| AppError::Input(e.to_string()))?;
                    } else {
                        println!("{}", String::from_utf8_lossy(&rendered));
                    }
                    Ok(if report.acceptance_passed() {
                        ExitCode::Success
                    } else {
                        ExitCode::Failure
                    })
                }
                BenchmarkCommands::Qhard {
                    golden,
                    fixture_root,
                    tree,
                    env_name,
                    attestation,
                    bin,
                    out,
                    k,
                    online_query,
                    synthetic_corpus,
                } => {
                    // A combined acceptance run must use one immutable binary
                    // for both lanes; normal evaluation otherwise resolves
                    // its public `--bin` path immediately before each search.
                    let combined_binary = synthetic_corpus
                        .is_some()
                        .then(|| qhard::snapshot_binary(bin))
                        .transpose()
                        .map_err(|error| AppError::Input(error.to_string()))?;
                    let measurement_bin = combined_binary
                        .as_ref()
                        .map(|(_, path)| path)
                        .unwrap_or(bin);
                    let report = qhard::run(QhardOptions {
                        golden: golden.clone(),
                        fixture_root: fixture_root.clone(),
                        tree: tree.clone(),
                        env_name: env_name.clone(),
                        attestation: attestation.clone(),
                        bin: measurement_bin.to_path_buf(),
                        k: *k,
                        online_query: *online_query,
                    })
                    .map_err(|error| AppError::Input(error.to_string()))?;
                    let mut report = report;
                    let mut synthetic_input_root = None;
                    if let Some(corpus) = synthetic_corpus {
                        synthetic_input_root = Some(corpus.clone());
                        let synthetic_snapshot = qhard::snapshot_regular_tree(corpus)
                            .map_err(|error| AppError::Input(error.to_string()))?;
                        let temporary = tempfile::tempdir().map_err(|error| {
                            AppError::Input(format!(
                                "cannot create combined measurement workspace: {error}"
                            ))
                        })?;
                        let frozen_golden = bundled_eval_path("golden-queries.jsonl");
                        let golden_bytes = fs::read(&frozen_golden).map_err(|error| {
                            AppError::Input(format!("cannot read frozen synthetic golden: {error}"))
                        })?;
                        if golden_bytes.len() > 64 * 1024
                            || kio_core::cas::hash_bytes(&golden_bytes)
                                != qhard::FROZEN_SYNTHETIC_M3_1_GOLDEN_SHA256
                        {
                            return Err(AppError::Input(
                                "synthetic M3-1 golden differs from the frozen contract".into(),
                            ));
                        }
                        let synthetic_golden = temporary.path().join("golden-queries.jsonl");
                        fs::write(&synthetic_golden, golden_bytes).map_err(|error| {
                            AppError::Input(format!(
                                "cannot snapshot frozen synthetic golden: {error}"
                            ))
                        })?;
                        let synthetic_out = temporary.path().join("synthetic-results.json");
                        let synthetic_report = temporary.path().join("synthetic-report.md");
                        let outcome = run(Args {
                            command: None,
                            golden: Some(synthetic_golden.clone()),
                            corpus: Some(synthetic_snapshot.path().to_path_buf()),
                            corpus_manifest: None,
                            history_manifest: None,
                            bin: measurement_bin.to_path_buf(),
                            out: Some(synthetic_out.clone()),
                            report: Some(synthetic_report),
                            scenario: vec!["M3-1".to_owned()],
                            min_recall: 0.8,
                            dry_run: false,
                        })?;
                        if outcome != ExitCode::Success {
                            return Err(AppError::Input(
                                "same-invocation synthetic M3-1 measurement did not pass".into(),
                            ));
                        }
                        synthetic_snapshot
                            .verify_source_unchanged()
                            .map_err(|error| AppError::Input(error.to_string()))?;
                        let results: kio_eval::runner::EvaluationResults =
                            serde_json::from_slice(&fs::read(&synthetic_out).map_err(|error| {
                                AppError::Input(format!(
                                    "cannot read same-invocation synthetic result: {error}"
                                ))
                            })?)
                            .map_err(|error| {
                                AppError::Input(format!(
                                    "invalid same-invocation synthetic result: {error}"
                                ))
                            })?;
                        let m31 = results.scenarios.get("M3-1").ok_or_else(|| {
                            AppError::Input("same-invocation result omitted M3-1".into())
                        })?;
                        if m31.n_queries != 18 || m31.n_scored != 18 {
                            return Err(AppError::Input(
                                "same-invocation synthetic M3-1 did not measure exactly 18 queries"
                                    .into(),
                            ));
                        }
                        let hits = results
                            .queries
                            .iter()
                            .filter(|row| row.scenario == "M3-1" && row.recall_at_10 == Some(1.0))
                            .count();
                        report
                            .combine_synthetic_m3_1(&synthetic_golden, hits, 18)
                            .map_err(|error| AppError::Input(error.to_string()))?;
                    }
                    let rendered = serde_json::to_vec_pretty(&report).map_err(|error| {
                        AppError::Input(format!("cannot serialize Q_hard report: {error}"))
                    })?;
                    if let Some(path) = out {
                        if let Some(synthetic_root) = &synthetic_input_root
                            && output_is_within_input_root(path, synthetic_root)?
                        {
                            return Err(AppError::Input(
                                "Q_hard report must be outside synthetic corpus input".into(),
                            ));
                        }
                        qhard::write_report(path, fixture_root, &report)
                            .map_err(|error| AppError::Input(error.to_string()))?;
                    } else {
                        println!("{}", String::from_utf8_lossy(&rendered));
                    }
                    // Q_hard-only reports are useful evidence, but they are
                    // explicitly not the combined 26-query M3-1 acceptance
                    // gate.  Never turn an ineligible measurement into a
                    // successful acceptance exit status.
                    Ok(if report.acceptance_passed() {
                        ExitCode::Success
                    } else {
                        ExitCode::Failure
                    })
                }
            },
            Commands::Release { command } => match command {
                ReleaseCommands::NativeTarget => {
                    println!("{}", kio_eval::release::native_target()?);
                    Ok(ExitCode::Success)
                }
                ReleaseCommands::LockSha256 { repo } => {
                    println!("{}", kio_eval::release::candidate_lock_sha256(repo)?);
                    Ok(ExitCode::Success)
                }
                ReleaseCommands::ArchiveSha256 { archive } => {
                    println!("{}", kio_eval::release::candidate_archive_sha256(archive)?);
                    Ok(ExitCode::Success)
                }
                ReleaseCommands::PrepareTools { out, cargo_home } => {
                    kio_eval::release::prepare_tools(&kio_eval::release::PrepareToolsOptions {
                        output_dir: out.clone(),
                        cargo_home: cargo_home.clone(),
                    })?;
                    println!("[ok] pinned release tools: {}", out.display());
                    Ok(ExitCode::Success)
                }
                ReleaseCommands::PrepareCargoHome {
                    repo,
                    source_cargo_home,
                    out,
                } => {
                    kio_eval::release::prepare_cargo_home(
                        &kio_eval::release::PrepareCargoHomeOptions {
                            repo: repo.clone(),
                            source_cargo_home: source_cargo_home.clone(),
                            output_dir: out.clone(),
                        },
                    )?;
                    println!("[ok] isolated Cargo home: {}", out.display());
                    Ok(ExitCode::Success)
                }
                ReleaseCommands::Build {
                    repo,
                    candidate_sha,
                    target,
                    target_dir,
                    cargo_home,
                } => {
                    let summary = kio_eval::release::build_candidate(
                        &kio_eval::release::BuildCandidateOptions {
                            repo: repo.clone(),
                            candidate_sha: candidate_sha.clone(),
                            target: target.clone(),
                            target_dir: target_dir.clone(),
                            cargo_home: cargo_home.clone(),
                        },
                    )?;
                    println!(
                        "{}",
                        serde_json::to_string(&summary)
                            .map_err(|error| AppError::Input(error.to_string()))?
                    );
                    Ok(ExitCode::Success)
                }
                ReleaseCommands::Package {
                    repo,
                    binary,
                    target,
                    tools_dir,
                    cargo_home,
                    out,
                } => {
                    let summary = kio_eval::release::package_candidate(
                        &kio_eval::release::PackageCandidateOptions {
                            repo: repo.clone(),
                            binary: binary.clone(),
                            target: target.clone(),
                            output_dir: out.clone(),
                            tools_dir: tools_dir.clone(),
                            cargo_home: cargo_home.clone(),
                        },
                    )?;
                    println!(
                        "{}",
                        serde_json::to_string(&summary)
                            .map_err(|error| AppError::Input(error.to_string()))?
                    );
                    Ok(ExitCode::Success)
                }
                ReleaseCommands::Verify {
                    archive,
                    checksums,
                    expected_archive_sha256,
                    source_repo,
                    expected_commit,
                    expected_lock_sha256,
                } => {
                    let summary = kio_eval::release::verify_candidate(
                        &kio_eval::release::VerifyCandidateOptions {
                            archive: archive.clone(),
                            checksum: checksums.clone(),
                            expected_archive_sha256: expected_archive_sha256.clone(),
                            expected_repo: Some(source_repo.clone()),
                            expected_commit: Some(expected_commit.clone()),
                            expected_lock_sha256: Some(expected_lock_sha256.clone()),
                        },
                    )?;
                    println!(
                        "{}",
                        serde_json::to_string(&summary)
                            .map_err(|error| AppError::Input(error.to_string()))?
                    );
                    Ok(ExitCode::Success)
                }
                ReleaseCommands::Smoke {
                    archive,
                    checksums,
                    expected_archive_sha256,
                    source_repo,
                    expected_commit,
                    expected_lock_sha256,
                    work_dir,
                    receipt,
                } => {
                    let summary = kio_eval::release::smoke_candidate(
                        &kio_eval::release::SmokeCandidateOptions {
                            verify: kio_eval::release::VerifyCandidateOptions {
                                archive: archive.clone(),
                                checksum: checksums.clone(),
                                expected_archive_sha256: expected_archive_sha256.clone(),
                                expected_repo: Some(source_repo.clone()),
                                expected_commit: Some(expected_commit.clone()),
                                expected_lock_sha256: Some(expected_lock_sha256.clone()),
                            },
                            work_dir: work_dir.clone(),
                            receipt: receipt.clone(),
                        },
                    )?;
                    println!(
                        "{}",
                        serde_json::to_string(&summary)
                            .map_err(|error| AppError::Input(error.to_string()))?
                    );
                    Ok(ExitCode::Success)
                }
                ReleaseCommands::Compare { left, right } => {
                    kio_eval::release::compare_candidate_dirs(left, right)?;
                    println!("[ok] candidate outputs are byte-identical");
                    Ok(ExitCode::Success)
                }
            },
            Commands::Acceptance { command } => match command {
                AcceptanceCommands::AuthenticatedLocal {
                    phase,
                    expected_case,
                    os,
                    bin,
                    fixture,
                    work_dir,
                    receipt,
                    ocr_endpoint,
                    embedding_endpoint,
                    ca_pem,
                    service_identity,
                    service_identity_sha256,
                } => {
                    let expected = read_expected_receipt(expected_case)
                        .map_err(|error| AppError::Input(error.to_string()))?;
                    let requirement = AcceptanceRequirement {
                        case: AcceptanceCase::A08,
                        lane: AcceptanceLane::ProviderLive,
                        subcase: AcceptanceSubcase::AuthenticatedLocal,
                        os: *os,
                    };
                    if expected.requirement != requirement {
                        return Err(AppError::Input(
                            "expected binding does not match A08 authenticated-local and the selected OS".into(),
                        ));
                    }
                    let options = AuthenticatedLocalOptions {
                        binary: bin.clone(),
                        fixture: fixture.clone(),
                        expected,
                        work_dir: work_dir.clone(),
                        receipt: receipt.clone(),
                        ocr_endpoint: ocr_endpoint.clone(),
                        embedding_endpoint: embedding_endpoint.clone(),
                        ca_pem: ca_pem.clone(),
                        service_identity: service_identity.clone(),
                        service_identity_sha256: service_identity_sha256.clone(),
                    };
                    match phase {
                        AuthenticatedLocalPhase::Ocr => {
                            run_ocr(&options)
                                .map_err(|error| AppError::Input(error.to_string()))?;
                            println!(
                                "[ok] authenticated-local OCR checkpoint written; embedding phase remains"
                            );
                        }
                        AuthenticatedLocalPhase::Embedding => {
                            run_embedding(&options)
                                .map_err(|error| AppError::Input(error.to_string()))?;
                            println!("[ok] authenticated-local acceptance receipt written");
                        }
                    }
                    Ok(ExitCode::Success)
                }
                AcceptanceCommands::TargetBinding {
                    candidate_sha,
                    os,
                    target,
                    archive_sha256,
                    binary_sha256,
                    version,
                    out,
                } => {
                    kio_eval::acceptance_distribution::write_target_binding(
                        &kio_eval::acceptance_distribution::TargetBindingOptions {
                            candidate_sha: candidate_sha.clone(),
                            os: *os,
                            target: target.clone(),
                            archive_sha256: archive_sha256.clone(),
                            binary_sha256: binary_sha256.clone(),
                            version: version.clone(),
                            out: out.clone(),
                        },
                    )
                    .map_err(|e| AppError::Input(e.to_string()))?;
                    Ok(ExitCode::Success)
                }
                AcceptanceCommands::Distribution {
                    expected,
                    expected_case,
                    os,
                    archive,
                    checksums,
                    repro_archive,
                    repro_checksums,
                    source_repo,
                    expected_lock_sha256,
                    work_dir,
                    receipt,
                } => {
                    let requirement = AcceptanceRequirement {
                        case: AcceptanceCase::A12,
                        lane: AcceptanceLane::Distribution,
                        subcase: AcceptanceSubcase::Core,
                        os: *os,
                    };
                    let expected = match (expected.as_ref(), expected_case.as_ref()) {
                        (Some(expected), None) => read_expected_manifest(expected)
                            .map_err(|e| AppError::Input(e.to_string()))?
                            .plan()
                            .map_err(|e| AppError::Input(e.to_string()))?
                            .expected_for(&requirement)
                            .map_err(|e| AppError::Input(e.to_string()))?
                            .clone(),
                        (None, Some(expected_case)) => {
                            let expected = read_expected_receipt(expected_case)
                                .map_err(|e| AppError::Input(e.to_string()))?;
                            if expected.requirement != requirement {
                                return Err(AppError::Input(
                                    "single-case expected binding does not match the requested distribution selector".into(),
                                ));
                            }
                            expected
                        }
                        _ => return Err(AppError::Input(
                            "acceptance distribution requires exactly one of --expected or --expected-case".into(),
                        )),
                    };
                    let summary = kio_eval::acceptance_distribution::run(
                        &kio_eval::acceptance_distribution::DistributionOptions {
                            expected,
                            archive: archive.clone(),
                            checksums: checksums.clone(),
                            repro_archive: repro_archive.clone(),
                            repro_checksums: repro_checksums.clone(),
                            source_repo: source_repo.clone(),
                            expected_lock_sha256: expected_lock_sha256.clone(),
                            work_dir: work_dir.clone(),
                            receipt: receipt.clone(),
                        },
                    )
                    .map_err(|e| AppError::Input(e.to_string()))?;
                    println!(
                        "{}",
                        serde_json::to_string(&summary)
                            .map_err(|e| AppError::Input(e.to_string()))?
                    );
                    Ok(ExitCode::Success)
                }
                AcceptanceCommands::Preflight {
                    runner_os,
                    office_converter,
                    out,
                } => {
                    let outcome = kio_eval::acceptance_preflight::run(
                        &kio_eval::acceptance_preflight::PreflightOptions {
                            runner_os: *runner_os,
                            office_converter: office_converter.clone(),
                            out: out.clone(),
                        },
                    )
                    .map_err(|error| AppError::Input(error.to_string()))?;
                    if !outcome.passed {
                        return Err(AppError::Input(
                            "native acceptance preflight failed; inspect the create-only report"
                                .into(),
                        ));
                    }
                    Ok(ExitCode::Success)
                }
                AcceptanceCommands::Budget { command } => {
                    let value = match command {
                        ProviderBudgetCommands::Initialize {
                            state_dir,
                            campaign_id,
                            provider,
                            cap_microusd,
                        } => serde_json::to_value(
                            initialize_campaign(state_dir, campaign_id, provider, *cap_microusd)
                                .map_err(|error| AppError::Input(error.to_string()))?,
                        ),
                        ProviderBudgetCommands::Reserve {
                            state_dir,
                            campaign_id,
                            candidate_sha,
                            provider,
                            allocation_id,
                            cap_microusd,
                            amount_microusd,
                        } => serde_json::to_value(
                            reserve_provider_budget(
                                state_dir,
                                campaign_id,
                                candidate_sha,
                                provider,
                                allocation_id,
                                *cap_microusd,
                                *amount_microusd,
                            )
                            .map_err(|error| AppError::Input(error.to_string()))?,
                        ),
                    }
                    .map_err(|error| AppError::Input(error.to_string()))?;
                    println!(
                        "{}",
                        serde_json::to_string(&value)
                            .map_err(|error| AppError::Input(error.to_string()))?
                    );
                    Ok(ExitCode::Success)
                }
                AcceptanceCommands::Expected { binding, out } => {
                    let binding = read_acceptance_binding(binding)
                        .map_err(|error| AppError::Input(error.to_string()))?;
                    let manifest = binding
                        .expected_manifest()
                        .map_err(|error| AppError::Input(error.to_string()))?;
                    write_expected_manifest_create_only(out, &manifest)
                        .map_err(|error| AppError::Input(error.to_string()))?;
                    println!("[ok] canonical fixed v1 expected manifest written");
                    Ok(ExitCode::Success)
                }
                AcceptanceCommands::ExpectedCase {
                    target_binding,
                    case,
                    lane,
                    subcase,
                    os,
                    fixture_id,
                    fixture_sha256,
                    workflow_path,
                    workflow_sha256,
                    workflow_commit,
                    run_id,
                    attempt,
                    architecture,
                    contract_binary_sha256,
                    service_identity_sha256,
                    out,
                } => {
                    let requirement = AcceptanceRequirement {
                        case: *case,
                        lane: *lane,
                        subcase: *subcase,
                        os: *os,
                    };
                    write_expected_case(&ExpectedCaseOptions {
                        target_binding: target_binding.clone(),
                        requirement,
                        fixture_id: fixture_id.clone(),
                        fixture_sha256: fixture_sha256.clone(),
                        workflow_path: workflow_path.clone(),
                        workflow_sha256: workflow_sha256.clone(),
                        workflow_commit: workflow_commit.clone(),
                        run_id: *run_id,
                        attempt: *attempt,
                        architecture: architecture.clone(),
                        contract_binary_sha256: contract_binary_sha256.clone(),
                        service_identity_sha256: service_identity_sha256.clone(),
                        out: out.clone(),
                    })
                    .map_err(|error| AppError::Input(error.to_string()))?;
                    println!("[ok] canonical single-case expected receipt written");
                    Ok(ExitCode::Success)
                }
                AcceptanceCommands::AssembleExpected {
                    candidate_sha,
                    expected_case,
                    out,
                } => {
                    let expected = expected_case
                        .iter()
                        .map(|path| {
                            read_expected_receipt(path)
                                .map_err(|error| AppError::Input(error.to_string()))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let manifest = assemble_expected_cases(candidate_sha, expected)
                        .map_err(|error| AppError::Input(error.to_string()))?;
                    write_expected_manifest_create_only(out, &manifest)
                        .map_err(|error| AppError::Input(error.to_string()))?;
                    println!("[ok] canonical fixed v1 expected manifest assembled");
                    Ok(ExitCode::Success)
                }
                AcceptanceCommands::Plan { expected } => {
                    let manifest = read_expected_manifest(expected)
                        .map_err(|error| AppError::Input(error.to_string()))?;
                    let bytes = manifest
                        .canonical_bytes()
                        .map_err(|error| AppError::Input(error.to_string()))?;
                    println!("{}", String::from_utf8_lossy(&bytes));
                    Ok(ExitCode::Success)
                }
                AcceptanceCommands::Verify { expected, receipt } => {
                    let manifest = read_expected_manifest(expected)
                        .map_err(|error| AppError::Input(error.to_string()))?;
                    let plan = manifest
                        .plan()
                        .map_err(|error| AppError::Input(error.to_string()))?;
                    let receipts = receipt
                        .iter()
                        .map(|path| {
                            read_receipt(path).map_err(|error| AppError::Input(error.to_string()))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    plan.verify(&receipts)
                        .map_err(|error| AppError::Input(error.to_string()))?;
                    println!("[ok] fixed v1 acceptance receipt set is complete and bound");
                    Ok(ExitCode::Success)
                }
                AcceptanceCommands::Run {
                    expected,
                    expected_case,
                    case,
                    lane,
                    subcase,
                    os,
                    bin,
                    fixture,
                    work_dir,
                    receipt,
                    contract_bin,
                    office_converter,
                    preflight_report,
                    allow_native_service,
                    provider,
                    reservation_state_dir,
                    campaign_id,
                    allocation_id,
                } => {
                    let requirement = AcceptanceRequirement {
                        case: *case,
                        lane: *lane,
                        subcase: *subcase,
                        os: *os,
                    };
                    let expected = match (expected.as_ref(), expected_case.as_ref()) {
                        (Some(expected), None) => read_expected_manifest(expected)
                            .map_err(|error| AppError::Input(error.to_string()))?
                            .plan()
                            .map_err(|error| AppError::Input(error.to_string()))?
                            .expected_for(&requirement)
                            .map_err(|error| AppError::Input(error.to_string()))?
                            .clone(),
                        (None, Some(expected_case)) => {
                            let expected = read_expected_receipt(expected_case)
                                .map_err(|error| AppError::Input(error.to_string()))?;
                            if expected.requirement != requirement {
                                return Err(AppError::Input(
                                    "single-case expected binding does not match the requested acceptance selector".into(),
                                ));
                            }
                            expected
                        }
                        _ => return Err(AppError::Input(
                            "acceptance run requires exactly one of --expected or --expected-case"
                                .into(),
                        )),
                    };
                    let provider_fields_present = provider.is_some()
                        || reservation_state_dir.is_some()
                        || campaign_id.is_some()
                        || allocation_id.is_some();
                    let office_converter_present = office_converter.is_some();
                    let is_provider_a08 = requirement.case == AcceptanceCase::A08
                        && requirement.lane == AcceptanceLane::ProviderLive
                        && matches!(
                            requirement.subcase,
                            AcceptanceSubcase::Mistral | AcceptanceSubcase::Gemini
                        );
                    if provider_fields_present && !is_provider_a08 {
                        return Err(AppError::Input(
                            "provider reservation fields are valid only for A08 provider-live"
                                .into(),
                        ));
                    }
                    let is_office_a07 = requirement.case == AcceptanceCase::A07
                        && requirement.lane == AcceptanceLane::OfficeReal
                        && requirement.subcase == AcceptanceSubcase::Core;
                    let is_service_case = requirement.lane == AcceptanceLane::ServiceNative
                        && requirement.subcase == AcceptanceSubcase::Core
                        && matches!(requirement.case, AcceptanceCase::A03 | AcceptanceCase::A11);
                    let is_contract_binary_case = matches!(
                        (requirement.case, requirement.lane, requirement.subcase),
                        (
                            AcceptanceCase::A10,
                            AcceptanceLane::NativeContract,
                            AcceptanceSubcase::Core
                        ) | (
                            AcceptanceCase::A08,
                            AcceptanceLane::NativeContract,
                            AcceptanceSubcase::MockFailure
                        )
                    );
                    if office_converter_present && !is_office_a07 {
                        return Err(AppError::Input(
                            "--office-converter is valid only for A07 office-real".into(),
                        ));
                    }
                    if preflight_report.is_some() && !(is_office_a07 || is_service_case) {
                        return Err(AppError::Input(
                            "--preflight-report is valid only for A07 office-real or A03/A11 service-native".into(),
                        ));
                    }
                    if *allow_native_service && !is_service_case {
                        return Err(AppError::Input(
                            "--allow-native-service is valid only for A03/A11 service-native"
                                .into(),
                        ));
                    }
                    if contract_bin.is_some() && !is_contract_binary_case {
                        return Err(AppError::Input(
                            "--contract-bin is valid only for A10 native-contract/core or A08 native-contract/mock-failure".into(),
                        ));
                    }
                    if is_contract_binary_case && contract_bin.is_none() {
                        return Err(AppError::Input(
                            "this native fault-contract requirement requires --contract-bin".into(),
                        ));
                    }
                    let summary = match requirement.case {
                        AcceptanceCase::A03 if is_service_case => {
                            let preflight_report = preflight_report
                                .as_ref()
                                .ok_or_else(|| {
                                    AppError::Input(
                                        "A03 service-native requires --preflight-report".into(),
                                    )
                                })?
                                .clone();
                            if !*allow_native_service {
                                return Err(AppError::Input(
                                    "A03 service-native requires --allow-native-service".into(),
                                ));
                            }
                            run_service_a03(&ServiceOptions {
                                binary: bin.clone(),
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                                preflight_report,
                                required_allow_native_service: true,
                            })
                        }
                        AcceptanceCase::A11 if is_service_case => {
                            let preflight_report = preflight_report
                                .as_ref()
                                .ok_or_else(|| {
                                    AppError::Input(
                                        "A11 service-native requires --preflight-report".into(),
                                    )
                                })?
                                .clone();
                            if !*allow_native_service {
                                return Err(AppError::Input(
                                    "A11 service-native requires --allow-native-service".into(),
                                ));
                            }
                            run_a11(&ServiceOptions {
                                binary: bin.clone(),
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                                preflight_report,
                                required_allow_native_service: true,
                            })
                        }
                        AcceptanceCase::A07 if is_office_a07 => {
                            let office_converter = office_converter
                                .as_ref()
                                .ok_or_else(|| {
                                    AppError::Input(
                                        "A07 office-real requires --office-converter".into(),
                                    )
                                })?
                                .clone();
                            let preflight_report = preflight_report
                                .as_ref()
                                .ok_or_else(|| {
                                    AppError::Input(
                                        "A07 office-real requires --preflight-report".into(),
                                    )
                                })?
                                .clone();
                            run_a07(&A07OfficeOptions {
                                binary: bin.clone(),
                                office_converter,
                                preflight_report,
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                            })
                        }
                        AcceptanceCase::A04
                            if requirement.lane == AcceptanceLane::NativeContract
                                && requirement.subcase == AcceptanceSubcase::Core =>
                        {
                            run_policy_a04(&PolicyOptions {
                                binary: bin.clone(),
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                            })
                        }
                        AcceptanceCase::A04
                            if requirement.lane == AcceptanceLane::NativeContract
                                && requirement.subcase == AcceptanceSubcase::LocalTrust =>
                        {
                            run_local_trust_a04(&LocalTrustOptions {
                                binary: bin.clone(),
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                            })
                        }
                        AcceptanceCase::A08
                            if requirement.lane == AcceptanceLane::NativeContract
                                && requirement.subcase == AcceptanceSubcase::MockFailure =>
                        {
                            let contract_binary = contract_bin
                                .as_ref()
                                .ok_or_else(|| {
                                    AppError::Input(
                                        "A08 native-contract/mock-failure requires --contract-bin"
                                            .into(),
                                    )
                                })?
                                .clone();
                            run_a08_failure(&FailureOptions {
                                binary: bin.clone(),
                                contract_binary,
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                            })
                        }
                        AcceptanceCase::A08 if is_provider_a08 => {
                            let provider = provider.ok_or_else(|| {
                                AppError::Input("A08 provider requires --provider".into())
                            })?;
                            let reservation_state_dir = reservation_state_dir
                                .as_ref()
                                .ok_or_else(|| {
                                    AppError::Input(
                                        "A08 provider requires --reservation-state-dir".into(),
                                    )
                                })?
                                .clone();
                            let campaign_id = campaign_id
                                .as_ref()
                                .ok_or_else(|| {
                                    AppError::Input("A08 provider requires --campaign-id".into())
                                })?
                                .clone();
                            let allocation_id = allocation_id
                                .as_ref()
                                .ok_or_else(|| {
                                    AppError::Input("A08 provider requires --allocation-id".into())
                                })?
                                .clone();
                            run_a08_provider(&A08ProviderOptions {
                                binary: bin.clone(),
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                                provider,
                                reservation_state_dir,
                                campaign_id,
                                allocation_id,
                            })
                        }
                        AcceptanceCase::A01
                            if requirement.lane == AcceptanceLane::NativeContract
                                && requirement.subcase == AcceptanceSubcase::Core =>
                        {
                            run_a01(&A01Options {
                                binary: bin.clone(),
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                            })
                        }
                        AcceptanceCase::A02
                            if requirement.lane == AcceptanceLane::NativeContract
                                && requirement.subcase == AcceptanceSubcase::Core =>
                        {
                            run_a02(&NativeOptions {
                                binary: bin.clone(),
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                            })
                        }
                        AcceptanceCase::A03
                            if requirement.lane == AcceptanceLane::NativeContract
                                && requirement.subcase == AcceptanceSubcase::Core =>
                        {
                            run_a03(&NativeOptions {
                                binary: bin.clone(),
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                            })
                        }
                        AcceptanceCase::A05
                            if requirement.lane == AcceptanceLane::NativeContract
                                && requirement.subcase == AcceptanceSubcase::Core =>
                        {
                            run_a05(&A05Options {
                                binary: bin.clone(),
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                            })
                        }
                        AcceptanceCase::A06
                            if requirement.lane == AcceptanceLane::NativeContract
                                && requirement.subcase == AcceptanceSubcase::Core =>
                        {
                            run_a06(&A06Options {
                                binary: bin.clone(),
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                            })
                        }
                        AcceptanceCase::A09
                            if requirement.lane == AcceptanceLane::NativeContract
                                && requirement.subcase == AcceptanceSubcase::Core =>
                        {
                            run_a09(&A09Options {
                                binary: bin.clone(),
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                            })
                        }
                        AcceptanceCase::A10
                            if requirement.lane == AcceptanceLane::NativeContract
                                && requirement.subcase == AcceptanceSubcase::Core =>
                        {
                            let contract_binary = contract_bin
                                .as_ref()
                                .ok_or_else(|| {
                                    AppError::Input(
                                        "A10 native-contract/core requires --contract-bin".into(),
                                    )
                                })?
                                .clone();
                            run_a10(&FaultOptions {
                                release_binary: bin.clone(),
                                contract_binary,
                                fixture: fixture.clone(),
                                expected,
                                work_dir: work_dir.clone(),
                                receipt: receipt.clone(),
                            })
                        }
                        _ => {
                            return Err(AppError::Input(format!(
                                "acceptance executor is not implemented for {:?}/{:?}/{:?}",
                                requirement.case, requirement.lane, requirement.subcase
                            )));
                        }
                    }
                    .map_err(|error| AppError::Input(error.to_string()))?;
                    println!(
                        "{}",
                        serde_json::to_string(&summary)
                            .map_err(|error| AppError::Input(error.to_string()))?
                    );
                    Ok(ExitCode::Success)
                }
            },
        };
    }
    let golden = args
        .golden
        .unwrap_or_else(|| bundled_eval_path("golden-queries.jsonl"));
    let out = args
        .out
        .unwrap_or_else(|| bundled_eval_path("results.json"));
    let report = args
        .report
        .unwrap_or_else(|| bundled_eval_path("report.md"));
    let mut queries = load_golden_queries(&golden)?;
    if !args.scenario.is_empty() {
        queries.retain(|query| {
            args.scenario
                .iter()
                .any(|scenario| scenario == scenario_name(query.scenario))
        });
        if queries.is_empty() {
            return Err(AppError::Input(
                "--scenario に該当するクエリが無い".to_owned(),
            ));
        }
    }
    let corpus_manifest = args
        .corpus_manifest
        .clone()
        .or_else(|| {
            args.corpus
                .as_ref()
                .map(|path| path.join("corpus-manifest.json"))
        })
        .ok_or_else(|| {
            AppError::Input("--corpus か --corpus-manifest を指定すること".to_owned())
        })?;
    let history_manifest = args
        .history_manifest
        .clone()
        .or_else(|| {
            args.corpus
                .as_ref()
                .map(|path| path.join("history-manifest.json"))
        })
        .ok_or_else(|| {
            AppError::Input("--corpus か --history-manifest を指定すること".to_owned())
        })?;
    let corpus = load_corpus_manifest(&corpus_manifest)?;
    let history = load_history_manifest(&history_manifest, &corpus)?;
    let model = CorpusModel::new(&corpus);
    let resolver = Resolver::new(&corpus);
    let active = ["M3-1", "M3-2", "M3-3"]
        .iter()
        .filter(|scenario| {
            queries
                .iter()
                .any(|query| scenario_name(query.scenario) == **scenario)
        })
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    let problems = queries
        .iter()
        .flat_map(|query| validate_query(query, &model, &resolver))
        .collect::<Vec<_>>();
    if args.dry_run {
        return Ok(if problems.is_empty() {
            ExitCode::Success
        } else {
            ExitCode::Failure
        });
    }
    let corpus_dir = args
        .corpus
        .as_ref()
        .ok_or_else(|| AppError::Input("実行モードには --corpus が必要".to_owned()))?
        .canonicalize()
        .map_err(|error| AppError::Input(format!("corpus を開けない: {error}")))?;
    if !corpus_dir.is_dir() {
        return Err(AppError::Input(format!(
            "corpus がディレクトリではない: {}",
            corpus_dir.display()
        )));
    }
    let bound_corpus = BoundCorpus::bind(&corpus_dir, &corpus.scopes)
        .map_err(|error| AppError::Input(error.to_string()))?;
    let bin = args
        .bin
        .canonicalize()
        .map_err(|_| AppError::Input(format!("kio バイナリ不在: {}", args.bin.display())))?;
    if !bin.is_file() {
        return Err(AppError::Input(format!(
            "kio バイナリ不在: {}",
            bin.display()
        )));
    }
    let environment = bound_corpus.device().hermetic_environment();
    let log_problems = verify_logs(&bin, &bound_corpus, &history, &environment);
    if !log_problems.is_empty() {
        return Err(AppError::Input(log_problems.join("; ")));
    }
    let resolved = queries
        .iter()
        .map(|query| {
            let (expected, _) = resolver.resolve_expected(&query.expected);
            let problems = validate_query(query, &model, &resolver);
            ResolvedQuery {
                scenario: scenario_name(query.scenario).to_owned(),
                query: query.query.clone(),
                expected,
                resolution_error: (!problems.is_empty()).then(|| problems.join("; ")),
            }
        })
        .collect::<Vec<_>>();
    let flags = queries.iter().map(expected_flags).collect::<Vec<_>>();
    let mut next = 0usize;
    let mut attestation_failures = 0usize;
    let mut attestor = active
        .iter()
        .any(|scenario| scenario == "M3-2")
        .then(|| PointerAttestor::from_bound_corpus(&bound_corpus))
        .transpose()
        .map_err(|error| AppError::Input(error.to_string()))?;
    let (mut results, records) = evaluate_queries_with_validator(
        &resolved,
        args.min_recall,
        |query| {
            let flags = flags.get(next).ok_or_else(|| {
                kio_eval::runner::RunnerError::Input("query/flags mismatch".to_owned())
            })?;
            next += 1;
            let research = bound_corpus.scope("research").ok_or_else(|| {
                kio_eval::runner::RunnerError::Input("research scope is unavailable".to_owned())
            })?;
            let mut command = Command::new(&bin);
            command
                .arg("--json")
                .arg("search")
                .arg(&query.query)
                .arg("--all-scopes")
                .args(flags)
                .env_clear()
                .envs(environment.iter().cloned());
            research
                .configure_command_cwd(&mut command)
                .map_err(|error| kio_eval::runner::RunnerError::Input(error.to_string()))?;
            let output = run_bounded_command(&mut command, BoundedProcessOptions::default(), None)?;
            Ok(kio_eval::runner::SearchOutcome {
                returncode: output.status.code().unwrap_or(-1),
                stdout: output.stdout,
                stderr: output.stderr,
                duration_ms: output.duration.as_secs_f64() * 1_000.0,
            })
        },
        |query, response| {
            if query.scenario != "M3-2" {
                return Ok(None);
            }
            let attestor = attestor.as_mut().expect("M3-2 created an attestor");
            let mut problems = Vec::new();
            let mut attested = 0usize;
            for (index, hit) in response
                .results
                .iter()
                .take(MAX_POINTER_ATTESTATIONS_PER_QUERY)
                .enumerate()
            {
                match attestor.attest(&hit.pointer_value) {
                    Ok(()) => attested += 1,
                    Err(error) => problems.push(format!(
                        "result[{index}] pointer attestation failed: {error}"
                    )),
                }
            }
            if problems.is_empty() {
                Ok(Some(attested))
            } else {
                attestation_failures += 1;
                Err(problems.join("; "))
            }
        },
    )?;
    let mut coverage = assess_history_coverage(&records);
    if active.iter().any(|scenario| scenario == "M3-2") {
        coverage.pointer_attested = results.counts.n_pointer_attested;
        coverage.pointer_attestation_failures = attestation_failures;
        coverage.passes_pointer_attestation = attestation_failures == 0;
    }
    if active.iter().any(|scenario| scenario == "M3-3") && coverage.passes_m3_3 {
        coverage.set_restore_problems(verify_restore(&bin, &bound_corpus, &records, &environment));
    }
    results.history_coverage = coverage;
    write_results(&out, &results)?;
    write_report(&report, &results, &active)?;
    Ok(match final_exit_code(&results, &active) {
        0 => ExitCode::Success,
        2 => ExitCode::InvalidUsage,
        _ => ExitCode::Failure,
    })
}

fn run_persona(command: &PersonaCommands) -> Result<ExitCode, AppError> {
    use kio_eval::{
        persona_artifact::{publish_create_only, read_strict},
        persona_materialize::{MaterializeRequest, materialize},
        persona_plan::{MAX_CANONICAL_BYTES as PLAN_MAX, PersonaPlan, frozen_plan},
        persona_render_artifact::{MAX_CANONICAL_BYTES as RENDER_MAX, RenderArtifact},
        persona_schedule::build_suite_schedule,
    };
    match command {
        PersonaCommands::Plan { profile, out } => {
            let bytes = frozen_plan(*profile)
                .canonical_bytes()
                .map_err(|e| AppError::Input(e.to_string()))?;
            publish_create_only(out, &bytes, RENDER_MAX)
                .map_err(|e| AppError::Input(e.to_string()))?;
        }
        PersonaCommands::Schedule { plan, out } => {
            let bytes = read_strict(plan, PLAN_MAX).map_err(|e| AppError::Input(e.to_string()))?;
            let plan =
                PersonaPlan::parse_canonical(&bytes).map_err(|e| AppError::Input(e.to_string()))?;
            let bytes = build_suite_schedule(&plan)
                .and_then(|suite| suite.canonical_bytes())
                .map_err(|e| AppError::Input(e.to_string()))?;
            publish_create_only(out, &bytes, RENDER_MAX)
                .map_err(|e| AppError::Input(e.to_string()))?;
        }
        PersonaCommands::Render { plan, out } => {
            let bytes = read_strict(plan, PLAN_MAX).map_err(|e| AppError::Input(e.to_string()))?;
            let plan =
                PersonaPlan::parse_canonical(&bytes).map_err(|e| AppError::Input(e.to_string()))?;
            let bytes = RenderArtifact::build(&plan)
                .and_then(|artifact| artifact.canonical_bytes())
                .map_err(|e| AppError::Input(e.to_string()))?;
            publish_create_only(out, &bytes, RENDER_MAX)
                .map_err(|e| AppError::Input(e.to_string()))?;
        }
        PersonaCommands::Materialize {
            plan,
            schedule,
            render,
            destination,
            replay_id,
        } => {
            materialize(MaterializeRequest {
                plan,
                schedule,
                render,
                destination,
                replay_id,
            })?;
        }
        PersonaCommands::Scaffold { plan, root } => {
            emit_persona_json(&kio_eval::persona_scaffold::scaffold(plan, root)?)?;
        }
        PersonaCommands::Attest { root, out } => {
            kio_eval::persona_attest::attest(root, out)?;
        }
        PersonaCommands::Lease { command } => return run_persona_lease(command),
    }
    Ok(ExitCode::Success)
}

fn run_persona_lease(command: &PersonaLeaseCommands) -> Result<ExitCode, AppError> {
    use kio_eval::persona_lease;
    match command {
        PersonaLeaseCommands::Claim {
            root,
            persona,
            session,
            label,
        } => emit_persona_json(&persona_lease::claim(
            root,
            persona,
            session,
            label.as_deref(),
        )?)?,
        PersonaLeaseCommands::Show { root, persona } => {
            emit_persona_json(&persona_lease::show(root, persona)?)?
        }
        PersonaLeaseCommands::Release {
            root,
            persona,
            release_token,
        } => emit_persona_json(&persona_lease::release(root, persona, release_token)?)?,
        PersonaLeaseCommands::Recover {
            root,
            persona,
            session,
            reason,
        } => emit_persona_json(&persona_lease::recover(root, persona, session, reason)?)?,
        PersonaLeaseCommands::Scope { command } => match command {
            PersonaScopeLeaseCommands::Claim {
                root,
                persona,
                scope_id,
                parent_session,
                worker_session,
                label,
            } => emit_persona_json(&persona_lease::scope_claim(
                root,
                persona,
                scope_id,
                parent_session,
                worker_session,
                label.as_deref(),
            )?)?,
            PersonaScopeLeaseCommands::Show {
                root,
                persona,
                scope_id,
            } => emit_persona_json(&persona_lease::scope_show(root, persona, scope_id)?)?,
            PersonaScopeLeaseCommands::Release {
                root,
                persona,
                scope_id,
                parent_session,
                release_token,
            } => emit_persona_json(&persona_lease::scope_release(
                root,
                persona,
                scope_id,
                parent_session,
                release_token,
            )?)?,
            PersonaScopeLeaseCommands::Recover {
                root,
                persona,
                scope_id,
                parent_session,
                worker_session,
                reason,
            } => emit_persona_json(&persona_lease::scope_recover(
                root,
                persona,
                scope_id,
                parent_session,
                worker_session,
                reason,
            )?)?,
        },
    }
    Ok(ExitCode::Success)
}

fn emit_persona_json(value: &impl serde::Serialize) -> Result<(), AppError> {
    let value = serde_json::to_value(value).map_err(|error| AppError::Input(error.to_string()))?;
    let mut bytes = kio_core::cas::canonical_json_bytes(&value)
        .map_err(|error| AppError::Input(error.to_string()))?;
    bytes.push(b'\n');
    std::io::Write::write_all(&mut std::io::stdout(), &bytes)
        .map_err(|error| AppError::Input(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs, path::Path};

    use clap::Parser;
    use kio_core::ExitCode;
    use kio_eval::{
        acceptance::{
            ACCEPTANCE_MANIFEST_SCHEMA, AcceptanceCase, AcceptanceLane, AcceptanceManifest,
            AcceptanceSubcase, CandidateBinding, ExpectedReceipt, FixtureBinding, NativeOs,
            WorkflowBinding, fixed_v1_requirements, read_expected_manifest,
            write_expected_receipt_create_only,
        },
        boundary::BoundCorpus,
    };
    use serde_json::json;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use super::PersonaCommands;
    use super::{
        Args, Commands, RerankCommands, bundled_eval_path, output_is_within_input_root,
        parse_acceptance_case, parse_acceptance_lane, parse_acceptance_subcase, parse_drain_rounds,
        parse_fixture_mode, parse_native_os, parse_provider, parse_recall, parse_rerank_dataset,
        parse_rerank_limit, parse_scenario, parse_u7_threshold, run,
    };
    use kio_eval::acceptance_provider::Provider;

    #[test]
    fn cli_value_parsers_are_strict() {
        assert_eq!(parse_scenario("M3-2").unwrap(), "M3-2");
        assert!(parse_scenario("m3-2").is_err());
        assert_eq!(parse_recall("0").unwrap(), 0.0);
        assert!(parse_recall("NaN").is_err());
        assert!(parse_recall("1.01").is_err());
        assert_eq!(parse_rerank_limit("100").unwrap(), 100);
        assert!(parse_rerank_limit("0").is_err());
        assert!(parse_rerank_limit("101").is_err());
        assert!(parse_rerank_dataset("fixture-b").is_ok());
        assert!(parse_rerank_dataset("fixture_b").is_err());
        assert!(parse_fixture_mode("offline").is_ok());
        assert!(parse_fixture_mode("batch").is_err());
        assert_eq!(parse_drain_rounds("8").unwrap(), 8);
        assert!(parse_drain_rounds("0").is_err());
        assert_eq!(parse_u7_threshold("0.999").unwrap(), 0.999);
        assert!(parse_u7_threshold("NaN").is_err());
        assert_eq!(parse_acceptance_case("A01").unwrap(), AcceptanceCase::A01);
        assert!(parse_acceptance_case("a01").is_err());
        assert_eq!(
            parse_acceptance_lane("native-contract").unwrap(),
            AcceptanceLane::NativeContract
        );
        assert!(parse_acceptance_lane("native_contract").is_err());
        assert_eq!(
            parse_acceptance_subcase("authenticated-local").unwrap(),
            AcceptanceSubcase::AuthenticatedLocal
        );
        assert_eq!(parse_native_os("macos").unwrap(), NativeOs::Macos);
        assert!(parse_native_os("darwin").is_err());
        assert_eq!(parse_provider("mistral").unwrap(), Provider::Mistral);
        assert!(parse_provider("local").is_err());
    }

    fn acceptance_manifest() -> AcceptanceManifest {
        let expected = fixed_v1_requirements()
            .into_iter()
            .map(|requirement| {
                let target = match requirement.os {
                    NativeOs::Linux => "x86_64-unknown-linux-gnu",
                    NativeOs::Macos => "aarch64-apple-darwin",
                    NativeOs::Windows => "x86_64-pc-windows-msvc",
                };
                let contract_binary_sha256 = matches!(
                    (requirement.case, requirement.lane, requirement.subcase),
                    (
                        AcceptanceCase::A10,
                        AcceptanceLane::NativeContract,
                        AcceptanceSubcase::Core
                    ) | (
                        AcceptanceCase::A08,
                        AcceptanceLane::NativeContract,
                        AcceptanceSubcase::MockFailure
                    )
                )
                .then(|| "f".repeat(64));
                ExpectedReceipt {
                    requirement: requirement.clone(),
                    candidate: CandidateBinding {
                        candidate_sha: "a".repeat(40),
                        target: target.into(),
                        archive_sha256: "b".repeat(64),
                        binary_sha256: "c".repeat(64),
                        version: "1.0.0".into(),
                    },
                    fixture: FixtureBinding {
                        fixture_id: "fixture".into(),
                        sha256: "d".repeat(64),
                    },
                    workflow: WorkflowBinding {
                        workflow_path: ".github/workflows/acceptance.yml".into(),
                        workflow_sha256: "e".repeat(64),
                        workflow_commit: "a".repeat(40),
                        run_id: 1,
                        attempt: 1,
                        architecture: "x86_64".into(),
                    },
                    evaluator_sha256: "9".repeat(64),
                    service_identity_sha256: (requirement.case == AcceptanceCase::A08
                        && requirement.lane == AcceptanceLane::ProviderLive
                        && requirement.subcase == AcceptanceSubcase::AuthenticatedLocal)
                        .then(|| "7".repeat(64)),
                    contract_binary_sha256,
                }
            })
            .collect();
        AcceptanceManifest {
            schema: ACCEPTANCE_MANIFEST_SCHEMA.into(),
            expected,
        }
    }

    #[test]
    fn acceptance_run_uses_canonical_manifest_and_refuses_unimplemented_cases() {
        let directory = tempfile::tempdir().unwrap();
        let expected = directory.path().join("expected.json");
        fs::write(&expected, acceptance_manifest().canonical_bytes().unwrap()).unwrap();
        let receipt = directory.path().join("receipt.json");
        let args = Args::try_parse_from([
            "kio-eval",
            "acceptance",
            "run",
            "--expected",
            expected.to_str().unwrap(),
            "--case",
            "A02",
            "--lane",
            "native-contract",
            "--os",
            "linux",
            "--bin",
            "/definitely/missing/kio",
            "--fixture",
            "/definitely/missing/fixture",
            "--work-dir",
            directory.path().join("work").to_str().unwrap(),
            "--receipt",
            receipt.to_str().unwrap(),
        ])
        .unwrap();
        assert!(run(args).is_err());
        assert!(!receipt.exists());
    }

    #[test]
    fn acceptance_assemble_expected_builds_only_the_complete_predeclared_matrix() {
        let directory = tempfile::tempdir().unwrap();
        let source = acceptance_manifest();
        let mut arguments = vec![
            OsString::from("kio-eval"),
            OsString::from("acceptance"),
            OsString::from("assemble-expected"),
            OsString::from("--candidate-sha"),
            OsString::from("a".repeat(40)),
        ];
        for (index, expected) in source.expected.iter().enumerate() {
            let path = directory.path().join(format!("expected-{index}.json"));
            write_expected_receipt_create_only(&path, expected).unwrap();
            arguments.push(OsString::from("--expected-case"));
            arguments.push(path.into_os_string());
        }
        let output = directory.path().join("expected.json");
        arguments.push(OsString::from("--out"));
        arguments.push(output.clone().into_os_string());
        run(Args::try_parse_from(arguments).unwrap()).unwrap();
        assert_eq!(read_expected_manifest(&output).unwrap(), source);
    }

    #[test]
    fn acceptance_assemble_expected_rejects_an_incomplete_case_set() {
        let directory = tempfile::tempdir().unwrap();
        let source = acceptance_manifest();
        let input = directory.path().join("single-expected.json");
        write_expected_receipt_create_only(&input, &source.expected[0]).unwrap();
        let output = directory.path().join("expected.json");
        let candidate_sha = "a".repeat(40);
        let args = Args::try_parse_from([
            "kio-eval",
            "acceptance",
            "assemble-expected",
            "--candidate-sha",
            &candidate_sha,
            "--expected-case",
            input.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .unwrap();
        assert!(run(args).is_err());
        assert!(!output.exists());
    }

    #[test]
    fn acceptance_assemble_expected_rejects_a_receipt_file_as_case_input() {
        let directory = tempfile::tempdir().unwrap();
        let source = acceptance_manifest();
        let input = directory.path().join("receipt.json");
        let mut receipt = serde_json::to_value(&source.expected[0]).unwrap();
        let fields = receipt.as_object_mut().unwrap();
        fields.insert("schema".into(), json!("kio.acceptance.receipt/v3"));
        fields.insert("passed".into(), json!(true));
        fs::write(
            &input,
            kio_core::cas::canonical_json_bytes(&receipt).unwrap(),
        )
        .unwrap();
        let output = directory.path().join("expected.json");
        let candidate_sha = "a".repeat(40);
        let args = Args::try_parse_from([
            "kio-eval",
            "acceptance",
            "assemble-expected",
            "--candidate-sha",
            &candidate_sha,
            "--expected-case",
            input.to_str().unwrap(),
            "--out",
            output.to_str().unwrap(),
        ])
        .unwrap();
        assert!(run(args).is_err());
        assert!(!output.exists());
    }

    #[test]
    fn acceptance_a01_cli_dispatch_refuses_an_unbound_binary_without_a_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let expected = directory.path().join("expected.json");
        fs::write(&expected, acceptance_manifest().canonical_bytes().unwrap()).unwrap();
        let fixture = directory.path().join("fixture.txt");
        fs::write(&fixture, b"fixture\n").unwrap();
        let receipt = directory.path().join("receipt.json");
        let args = Args::try_parse_from([
            "kio-eval",
            "acceptance",
            "run",
            "--expected",
            expected.to_str().unwrap(),
            "--case",
            "A01",
            "--lane",
            "native-contract",
            "--os",
            "linux",
            "--bin",
            "/definitely/missing/kio",
            "--fixture",
            fixture.to_str().unwrap(),
            "--work-dir",
            directory.path().join("work").to_str().unwrap(),
            "--receipt",
            receipt.to_str().unwrap(),
        ])
        .unwrap();
        assert!(run(args).is_err());
        assert!(!receipt.exists());
    }

    #[test]
    fn acceptance_a05_cli_dispatch_never_creates_a_receipt_before_full_runtime_proof() {
        let directory = tempfile::tempdir().unwrap();
        let expected = directory.path().join("expected.json");
        fs::write(&expected, acceptance_manifest().canonical_bytes().unwrap()).unwrap();
        let receipt = directory.path().join("receipt.json");
        let args = Args::try_parse_from([
            "kio-eval",
            "acceptance",
            "run",
            "--expected",
            expected.to_str().unwrap(),
            "--case",
            "A05",
            "--lane",
            "native-contract",
            "--os",
            "linux",
            "--bin",
            "/definitely/missing/kio",
            "--fixture",
            "/definitely/missing/fixture-bundle",
            "--work-dir",
            directory.path().join("work").to_str().unwrap(),
            "--receipt",
            receipt.to_str().unwrap(),
        ])
        .unwrap();
        assert!(run(args).is_err());
        assert!(!receipt.exists());
    }

    #[test]
    fn acceptance_a06_cli_dispatch_refuses_an_unbound_binary_without_a_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let expected = directory.path().join("expected.json");
        fs::write(&expected, acceptance_manifest().canonical_bytes().unwrap()).unwrap();
        let receipt = directory.path().join("receipt.json");
        let args = Args::try_parse_from([
            "kio-eval",
            "acceptance",
            "run",
            "--expected",
            expected.to_str().unwrap(),
            "--case",
            "A06",
            "--lane",
            "native-contract",
            "--os",
            "linux",
            "--bin",
            "/definitely/missing/kio",
            "--fixture",
            "/definitely/missing/fixture-bundle",
            "--work-dir",
            directory.path().join("work").to_str().unwrap(),
            "--receipt",
            receipt.to_str().unwrap(),
        ])
        .unwrap();
        assert!(run(args).is_err());
        assert!(!receipt.exists());
    }

    #[test]
    fn acceptance_a09_cli_dispatch_refuses_an_unbound_binary_without_a_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let expected = directory.path().join("expected.json");
        fs::write(&expected, acceptance_manifest().canonical_bytes().unwrap()).unwrap();
        let fixture = directory.path().join("fixture.txt");
        fs::write(&fixture, b"fixture\n").unwrap();
        let receipt = directory.path().join("receipt.json");
        let args = Args::try_parse_from([
            "kio-eval",
            "acceptance",
            "run",
            "--expected",
            expected.to_str().unwrap(),
            "--case",
            "A09",
            "--lane",
            "native-contract",
            "--os",
            "linux",
            "--bin",
            "/definitely/missing/kio",
            "--fixture",
            fixture.to_str().unwrap(),
            "--work-dir",
            directory.path().join("work").to_str().unwrap(),
            "--receipt",
            receipt.to_str().unwrap(),
        ])
        .unwrap();
        assert!(run(args).is_err());
        assert!(!receipt.exists());
    }

    #[test]
    fn cutover_commands_have_one_nested_canonical_surface() {
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "fixture",
                "register",
                "--corpus",
                "/tmp/corpus",
                "--out",
                "/tmp/fixture",
                "--bin",
                "/tmp/kio",
                "--mode",
                "offline",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "ocr",
                "provider",
                "--document",
                "/tmp/input.pdf",
                "--model",
                "mistral-ocr-4-1",
                "--request-id",
                "manual-1",
                "--out",
                "/tmp/response.json",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "ocr",
                "provider",
                "--python",
                "/tmp/python3",
                "--document",
                "/tmp/input.pdf",
                "--model",
                "mistral-ocr-4-1",
                "--request-id",
                "manual-1",
                "--out",
                "/tmp/response.json",
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "ocr",
                "provider",
                "--document",
                "/tmp/input.pdf",
                "--model",
                "mistral-ocr-latest",
                "--request-id",
                "manual-1",
                "--out",
                "/tmp/response.json",
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "ocr",
                "render",
                "--python",
                "/tmp/python3",
                "--adapter",
                "/tmp/render_native.py",
                "--request-id",
                "render-1",
                "--image",
                "/tmp/input.png",
                "--out",
                "/tmp/rendered.pdf",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "ocr",
                "evaluate",
                "--ground-truth",
                "/tmp/truth.json",
                "--response",
                "/tmp/response.json",
                "--out",
                "/tmp/report.json",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "rerank",
                "dump",
                "--dataset",
                "synthetic",
                "--corpus",
                "/tmp/corpus",
                "--out",
                "/tmp/dump.json",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "rerank",
                "dump",
                "--dataset",
                "fixture-b",
                "--corpus",
                "/tmp/fixture",
                "--golden",
                "/tmp/golden.jsonl",
                "--out",
                "/tmp/dump.json",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "rerank",
                "apply",
                "--input",
                "/tmp/dump.json",
                "--output",
                "/tmp/output.json",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "u7",
                "--base-url",
                "http://127.0.0.1:8000",
                "--model",
                "model",
                "--reference-adapter",
                "/tmp/reference_adapter.py",
                "--reference-python",
                "/tmp/python3",
                "--reference-model",
                "model",
                "--text",
                "control",
                "--out",
                "/tmp/u7.json",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from(["kio-eval", "benchmark", "comparator-runtime", "install",])
                .is_ok()
        );
        assert!(
            Args::try_parse_from(["kio-eval", "benchmark", "comparator-runtime", "prepare",])
                .is_err()
        );
        assert!(
            Args::try_parse_from(["kio-eval", "benchmark", "comparator-runtime", "finalize",])
                .is_err()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "rerank-dump",
                "--corpus",
                "/tmp/corpus",
                "--out",
                "/tmp/dump.json",
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "rerank-apply",
                "--input",
                "/tmp/dump.json",
                "--output",
                "/tmp/output.json",
            ])
            .is_err()
        );
    }

    #[test]
    fn replay_history_has_one_required_canonical_surface() {
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "replay-history",
                "--corpus",
                "/tmp/corpus",
                "--bin",
                "/tmp/kio",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "persona",
                "lease",
                "claim",
                "--root",
                "/tmp/workspace",
                "--persona",
                "p01",
                "--session",
                "parent-01",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "persona",
                "lease",
                "scope",
                "claim",
                "--root",
                "/tmp/workspace",
                "--persona",
                "p01",
                "--scope-id",
                "p01-primary-01",
                "--parent-session",
                "parent-01",
                "--worker-session",
                "worker-01",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "persona",
                "lease",
                "claim",
                "--root",
                "/tmp/workspace",
                "--persona",
                "p01",
                "--session",
                "parent-01",
                "--owner-digest",
                "sha256:deadbeef",
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "persona",
                "lease",
                "scope-claim",
                "--root",
                "/tmp/workspace",
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from(["kio-eval", "replay-history", "--corpus", "/tmp/corpus"])
                .is_err()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "replay-history",
                "--corpus",
                "/tmp/corpus",
                "--bin",
                "/tmp/kio",
                "--manifest",
                "/tmp/legacy.json",
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "replay_history",
                "--corpus",
                "/tmp/corpus",
                "--bin",
                "/tmp/kio",
            ])
            .is_err()
        );
    }

    #[test]
    fn persona_has_only_nested_canonical_commands() {
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "persona",
                "plan",
                "--profile",
                "tiny",
                "--out",
                "/tmp/plan.json"
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "persona",
                "scaffold",
                "--plan",
                "/tmp/plan.json",
                "--root",
                "/tmp/persona-workspace",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "persona",
                "attest",
                "--root",
                "/tmp/materialized",
                "--out",
                "/tmp/persona-attestation.json",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "persona",
                "attest",
                "--root",
                "/tmp/materialized",
                "--out",
                "/tmp/persona-attestation.json",
                "--materialization-digest",
                "sha256:deadbeef",
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "persona",
                "schedule",
                "--plan",
                "/tmp/plan.json",
                "--out",
                "/tmp/schedule.json"
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "persona",
                "render",
                "--plan",
                "/tmp/plan.json",
                "--out",
                "/tmp/render.json"
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "persona",
                "materialize",
                "--plan",
                "/tmp/plan.json",
                "--schedule",
                "/tmp/schedule.json",
                "--render",
                "/tmp/render.json",
                "--destination",
                "/tmp/persona-root",
                "--replay-id",
                "replay-01",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "persona-plan",
                "--profile",
                "tiny",
                "--out",
                "/tmp/plan.json"
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from(["kio-eval", "persona", "plan", "--profile", "tiny"]).is_err()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "persona",
                "plan",
                "--profile",
                "legacy",
                "--out",
                "/tmp/plan.json"
            ])
            .is_err()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn persona_create_only_artifacts_are_strict() {
        let root = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(root.path()).unwrap();
        let plan = root.join("plan.json");
        let schedule = root.join("schedule.json");
        let render = root.join("render.json");
        assert_eq!(
            run(Args {
                command: Some(Commands::Persona {
                    command: PersonaCommands::Plan {
                        profile: kio_eval::persona_plan::PersonaProfile::Tiny,
                        out: plan.clone()
                    }
                }),
                golden: None,
                corpus: None,
                corpus_manifest: None,
                history_manifest: None,
                bin: Path::new("kio").to_path_buf(),
                out: None,
                report: None,
                scenario: vec![],
                min_recall: 0.8,
                dry_run: false
            })
            .unwrap(),
            ExitCode::Success
        );
        assert!(
            run(Args {
                command: Some(Commands::Persona {
                    command: PersonaCommands::Plan {
                        profile: kio_eval::persona_plan::PersonaProfile::Tiny,
                        out: plan.clone()
                    }
                }),
                golden: None,
                corpus: None,
                corpus_manifest: None,
                history_manifest: None,
                bin: Path::new("kio").to_path_buf(),
                out: None,
                report: None,
                scenario: vec![],
                min_recall: 0.8,
                dry_run: false
            })
            .is_err()
        );
        assert_eq!(
            run(Args {
                command: Some(Commands::Persona {
                    command: PersonaCommands::Schedule {
                        plan: plan.clone(),
                        out: schedule.clone()
                    }
                }),
                golden: None,
                corpus: None,
                corpus_manifest: None,
                history_manifest: None,
                bin: Path::new("kio").to_path_buf(),
                out: None,
                report: None,
                scenario: vec![],
                min_recall: 0.8,
                dry_run: false
            })
            .unwrap(),
            ExitCode::Success
        );
        assert_eq!(
            run(Args {
                command: Some(Commands::Persona {
                    command: PersonaCommands::Render {
                        plan: plan.clone(),
                        out: render.clone()
                    }
                }),
                golden: None,
                corpus: None,
                corpus_manifest: None,
                history_manifest: None,
                bin: Path::new("kio").to_path_buf(),
                out: None,
                report: None,
                scenario: vec![],
                min_recall: 0.8,
                dry_run: false
            })
            .unwrap(),
            ExitCode::Success
        );
        let plan_bytes = kio_eval::persona_artifact::read_strict(
            &plan,
            kio_eval::persona_render_artifact::MAX_CANONICAL_BYTES,
        )
        .unwrap();
        let parsed_plan =
            kio_eval::persona_plan::PersonaPlan::parse_canonical(&plan_bytes).unwrap();
        let schedule_bytes = kio_eval::persona_artifact::read_strict(
            &schedule,
            kio_eval::persona_render_artifact::MAX_CANONICAL_BYTES,
        )
        .unwrap();
        kio_eval::persona_schedule::SuiteSchedule::parse_canonical(&parsed_plan, &schedule_bytes)
            .unwrap();
        let render_bytes = kio_eval::persona_artifact::read_strict(
            &render,
            kio_eval::persona_render_artifact::MAX_CANONICAL_BYTES,
        )
        .unwrap();
        kio_eval::persona_render_artifact::RenderArtifact::parse_canonical(
            &parsed_plan,
            &render_bytes,
        )
        .unwrap();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let materialized = root.join("materialized");
            let workspace = root.join("workspace");
            assert_eq!(
                run(Args {
                    command: Some(Commands::Persona {
                        command: PersonaCommands::Materialize {
                            plan: plan.clone(),
                            schedule: schedule.clone(),
                            render: render.clone(),
                            destination: materialized.clone(),
                            replay_id: "replay-01".into(),
                        },
                    }),
                    golden: None,
                    corpus: None,
                    corpus_manifest: None,
                    history_manifest: None,
                    bin: Path::new("kio").to_path_buf(),
                    out: None,
                    report: None,
                    scenario: vec![],
                    min_recall: 0.8,
                    dry_run: false,
                })
                .unwrap(),
                ExitCode::Success
            );
            assert!(materialized.join("persona-materialization.json").is_file());
            let before = fs::read(materialized.join("persona-plan.json")).unwrap();
            assert!(
                run(Args {
                    command: Some(Commands::Persona {
                        command: PersonaCommands::Materialize {
                            plan: plan.clone(),
                            schedule: schedule.clone(),
                            render: render.clone(),
                            destination: materialized.clone(),
                            replay_id: "replay-01".into(),
                        },
                    }),
                    golden: None,
                    corpus: None,
                    corpus_manifest: None,
                    history_manifest: None,
                    bin: Path::new("kio").to_path_buf(),
                    out: None,
                    report: None,
                    scenario: vec![],
                    min_recall: 0.8,
                    dry_run: false,
                })
                .is_err()
            );
            assert_eq!(
                fs::read(materialized.join("persona-plan.json")).unwrap(),
                before
            );
            assert_eq!(
                run(Args {
                    command: Some(Commands::Persona {
                        command: PersonaCommands::Scaffold {
                            plan: plan.clone(),
                            root: workspace.clone(),
                        },
                    }),
                    golden: None,
                    corpus: None,
                    corpus_manifest: None,
                    history_manifest: None,
                    bin: Path::new("kio").to_path_buf(),
                    out: None,
                    report: None,
                    scenario: vec![],
                    min_recall: 0.8,
                    dry_run: false,
                })
                .unwrap(),
                ExitCode::Success
            );
            assert!(workspace.join("persona-workspace-owner.json").is_file());
            let owner_before = fs::read(workspace.join("persona-workspace-owner.json")).unwrap();
            assert!(
                run(Args {
                    command: Some(Commands::Persona {
                        command: PersonaCommands::Scaffold {
                            plan: plan.clone(),
                            root: workspace.clone(),
                        },
                    }),
                    golden: None,
                    corpus: None,
                    corpus_manifest: None,
                    history_manifest: None,
                    bin: Path::new("kio").to_path_buf(),
                    out: None,
                    report: None,
                    scenario: vec![],
                    min_recall: 0.8,
                    dry_run: false,
                })
                .is_err()
            );
            assert_eq!(
                fs::read(workspace.join("persona-workspace-owner.json")).unwrap(),
                owner_before
            );
        }
        let text = std::str::from_utf8(&render_bytes).unwrap();
        assert!(!text.contains("\"bytes\""));
        assert!(!text.contains("history_ready"));
        let relative = Path::new("relative.json");
        assert!(
            run(Args {
                command: Some(Commands::Persona {
                    command: PersonaCommands::Plan {
                        profile: kio_eval::persona_plan::PersonaProfile::Tiny,
                        out: relative.into()
                    }
                }),
                golden: None,
                corpus: None,
                corpus_manifest: None,
                history_manifest: None,
                bin: Path::new("kio").to_path_buf(),
                out: None,
                report: None,
                scenario: vec![],
                min_recall: 0.8,
                dry_run: false
            })
            .is_err()
        );
    }

    #[test]
    fn scale_has_one_required_canonical_subcommand_tree() {
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "scale",
                "generate",
                "--out",
                "/tmp/scale",
                "--profile",
                "tiny",
                "--lane",
                "current-text",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "scale",
                "prepare",
                "--corpus",
                "/tmp/scale",
                "--bin",
                "/tmp/kio",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from(["kio-eval", "scale", "attest", "--corpus", "/tmp/scale",])
                .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "scale",
                "benchmark",
                "--current-corpus",
                "/tmp/current",
                "--history-corpus",
                "/tmp/history",
                "--bin",
                "/tmp/kio",
                "--warmups",
                "1",
                "--samples",
                "1",
                "--out",
                "/tmp/report.json",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "scale",
                "benchmark",
                "--current-corpus",
                "/tmp/current",
                "--history-corpus",
                "/tmp/history",
                "--bin",
                "/tmp/kio",
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "benchmark",
                "scale",
                "--corpus",
                "/tmp/scale",
                "--bin",
                "/tmp/kio",
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "kio-eval",
                "scale",
                "generate",
                "--out",
                "/tmp/scale",
                "--profile",
                "legacy",
            ])
            .is_err()
        );
    }

    #[test]
    fn rerank_apply_empty_ranking_has_failure_exit_code() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("dump.json");
        let output = root.path().join("output.json");
        fs::write(
            &input,
            serde_json::to_vec(&json!({
                "note": "pass 1 of the reranker differential; generated by kio-eval rerank dump --dataset synthetic",
                "limit": 1,
                "queries": [{
                    "id": "golden#0", "scenario": "M3-1", "query": "needle",
                    "expected": [["sha256:a", null, "a.md"]],
                    "baseline_recall_at_10": 1.0,
                    "candidates": [{"key": ["sha256:a", null, "a.md"], "text": "candidate"}]
                }],
                "skipped": []
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            &output,
            br#"{"model":"test-model","queries":[{"id":"golden#0","ranking":[]}]}"#,
        )
        .unwrap();
        let code = run(Args {
            command: Some(Commands::Rerank {
                command: RerankCommands::Apply {
                    input,
                    output,
                    report: None,
                },
            }),
            golden: None,
            corpus: None,
            corpus_manifest: None,
            history_manifest: None,
            bin: Path::new("kio").to_path_buf(),
            out: None,
            report: None,
            scenario: vec![],
            min_recall: 0.8,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(code, ExitCode::Failure);
    }

    #[test]
    fn bundled_artifacts_do_not_depend_on_the_current_directory() {
        let golden = bundled_eval_path("golden-queries.jsonl");
        assert!(golden.ends_with(Path::new("eval/golden-queries.jsonl")));
        assert!(golden.is_absolute());
    }

    #[test]
    fn output_inside_synthetic_input_is_rejected() {
        let corpus = tempfile::tempdir().unwrap();
        assert!(
            output_is_within_input_root(&corpus.path().join("report.json"), corpus.path()).unwrap()
        );
    }

    #[test]
    fn device_environment_is_an_explicit_allowlist() {
        let corpus = tempfile::tempdir().unwrap();
        fs::create_dir(corpus.path().join("research")).unwrap();
        fs::create_dir(corpus.path().join("research/.kio")).unwrap();
        let bound = BoundCorpus::bind(corpus.path(), &["research".to_owned()]).unwrap();
        let values = bound.device().hermetic_environment();
        assert!(values.iter().any(|(key, _)| key == "PATH"));
        assert!(
            values
                .iter()
                .any(|(key, value)| key == "TZ" && value == "UTC")
        );
        assert!(values.iter().any(|(key, value)| {
            key == "HOME" && value == &OsString::from(bound.device().home())
        }));
        assert!(!values.iter().any(|(key, _)| key == "SSH_AUTH_SOCK"));
        assert!(!values.iter().any(|(key, _)| key == "GEMINI_API_KEY"));
    }
}
