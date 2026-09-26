//! Native watcher acceptance executors. These invoke only the supplied release
//! candidate in a private, fresh device environment.

use std::{
    collections::BTreeMap,
    fs::{self, File, FileTimes},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use kio_app::watch::{
    DirtyQueue, DirtyReason, EnqueueOutcome, QUEUE_CAPACITY, Reconcile, ReconcileCompletion,
    WatchEngine, WatchError, WatchRoot,
};

use crate::acceptance::{
    AcceptanceCase, AcceptanceError, AcceptanceLane, AcceptanceReceipt, AcceptanceSubcase,
    ExpectedReceipt, MAX_BINARY_BYTES, MAX_FIXTURE_BYTES, current_evaluator_sha256_matches,
    empty_runtime, sha256_bytes, sha256_regular_file, validate_expected, write_receipt_create_only,
};
use crate::acceptance_environment::IsolatedChildEnvironment;

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_WATCH_OUTPUT_BYTES: u64 = 256 * 1024;
const MAX_SCOPE_COUNT: usize = 4096;
const MAX_DIRECTORY_ENTRIES: usize = 16384;
const PRE_IGNORED_SEARCH_TOKEN: &str = "kio-preignored-child-token";
const POST_IGNORED_SEARCH_TOKEN: &str = "kio-postignored-child-token";
// This bounds a semantic convergence check over 39 independently durable
// scopes, including a 33-level chain. It is not the v1 performance benchmark:
// the observed macOS pass took 28s even without concurrent build load.
const WATCH_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct NativeOptions {
    pub binary: PathBuf,
    pub fixture: PathBuf,
    pub expected: ExpectedReceipt,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
}

/// A02 compares the final state produced by the real foreground native watcher
/// to a separately initialized tree indexed manually by that same candidate.
pub fn run_a02(options: &NativeOptions) -> Result<AcceptanceReceipt, AcceptanceError> {
    let requirement = &options.expected.requirement;
    if requirement.case != AcceptanceCase::A02
        || requirement.lane != AcceptanceLane::NativeContract
        || requirement.subcase != AcceptanceSubcase::Core
    {
        return Err(AcceptanceError::Invalid(
            "native watcher requirement is invalid".into(),
        ));
    }
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    verify_inputs(options)?;
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(AcceptanceError::Invalid(
            "native work directory and receipt must be create-only".into(),
        ));
    }

    fs::create_dir_all(&options.work_dir).map_err(io)?;
    private_dir(&options.work_dir)?;
    let work = options.work_dir.canonicalize().map_err(io)?;
    let watched = create_device_root(&work, "watched")?;
    let manual = create_device_root(&work, "manual")?;
    let outside = work.join("outside-sentinel");
    fs::create_dir(&outside).map_err(io)?;
    private_dir(&outside)?;
    write_file(
        &outside.join("sentinel.txt"),
        b"outside root must remain untouched\n",
    )?;
    let outside_before = tree_fingerprint(&outside)?;
    let seed = bounded_regular_bytes(&options.fixture, MAX_FIXTURE_BYTES)?;
    populate_initial(&watched.root, &seed)?;
    populate_initial(&manual.root, &seed)?;
    command(
        &options.binary,
        &watched.device,
        Some(&watched.root),
        &["--json", "init"],
    )?;
    command(
        &options.binary,
        &manual.device,
        Some(&manual.root),
        &["--json", "init"],
    )?;
    let mut child = WatchChild::start(&options.binary, &watched.device, &watched.root)?;
    let initial_success = wait_running(options, &watched, &mut child)?;
    apply_mutations(&watched.root, &seed)?;
    create_boundary_link(&watched.root.join("outside-boundary"), &outside)?;
    apply_mutations(&manual.root, &seed)?;
    wait_for_convergence(
        options,
        &watched,
        &mut child,
        initial_success,
        "native mutations",
    )?;

    // This is deliberately a foreground watcher stop; Drop remains the RAII
    // recovery path if any command or comparison fails.
    child.stop(&options.binary, &watched.device, &watched.root)?;
    if tree_fingerprint(&outside)? != outside_before {
        return Err(AcceptanceError::Command(
            "native watcher modified the outside-root sentinel through a link boundary".into(),
        ));
    }
    command(
        &options.binary,
        &manual.device,
        Some(&manual.root),
        &["--json", "index", "--offline"],
    )?;
    verify_inputs(options)?;

    let watched_scopes = collect_scope_manifests(&watched.root)?;
    let manual_scopes = collect_scope_manifests(&manual.root)?;
    assert_scope_equivalence(&watched.root, &watched_scopes, &manual.root, &manual_scopes)?;

    let receipt = AcceptanceReceipt {
        schema: "kio.acceptance.receipt/v4".into(),
        requirement: requirement.clone(),
        candidate: options.expected.candidate.clone(),
        fixture: options.expected.fixture.clone(),
        workflow: options.expected.workflow.clone(),
        evaluator_sha256: current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?,
        service_identity_sha256: options.expected.service_identity_sha256.clone(),
        contract_binary_sha256: options.expected.contract_binary_sha256.clone(),
        runtime: empty_runtime(),
        passed: true,
    };
    write_receipt_create_only(&options.receipt, &receipt)?;
    Ok(receipt)
}

/// A03 pairs a real native-watcher restart with the production durable queue.
/// Direct queue events below model backend notification loss only; reconciliation
/// still invokes the supplied candidate binary and A02 supplies OS-event proof.
pub fn run_a03(options: &NativeOptions) -> Result<AcceptanceReceipt, AcceptanceError> {
    let requirement = &options.expected.requirement;
    if requirement.case != AcceptanceCase::A03
        || requirement.lane != AcceptanceLane::NativeContract
        || requirement.subcase != AcceptanceSubcase::Core
    {
        return Err(AcceptanceError::Invalid(
            "native watcher requirement is invalid".into(),
        ));
    }
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    verify_inputs(options)?;
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(AcceptanceError::Invalid(
            "native work directory and receipt must be create-only".into(),
        ));
    }
    fs::create_dir_all(&options.work_dir).map_err(io)?;
    private_dir(&options.work_dir)?;
    let work = options.work_dir.canonicalize().map_err(io)?;
    let watched = create_device_root(&work, "watched")?;
    let manual = create_device_root(&work, "manual")?;
    let seed = bounded_regular_bytes(&options.fixture, MAX_FIXTURE_BYTES)?;
    populate_a03_initial(&watched.root, &seed)?;
    populate_a03_initial(&manual.root, &seed)?;
    command(
        &options.binary,
        &watched.device,
        Some(&watched.root),
        &["--json", "init"],
    )?;
    command(
        &options.binary,
        &manual.device,
        Some(&manual.root),
        &["--json", "init"],
    )?;
    command(
        &options.binary,
        &manual.device,
        Some(&manual.root),
        &["--json", "index", "--offline"],
    )?;

    let mut first = WatchChild::start(&options.binary, &watched.device, &watched.root)?;
    wait_running(options, &watched, &mut first)?;
    let before_scopes = collect_scope_manifests(&watched.root)?;
    let before_ignored = before_scopes
        .get("managed-then-ignored")
        .cloned()
        .ok_or_else(|| {
            AcceptanceError::Command(
                "native watcher did not create the child scope before Ignore revocation".into(),
            )
        })?;
    if !before_ignored.contains_key("visible-before-revocation.txt") {
        return Err(AcceptanceError::Command(
            "native watcher did not index the child before Ignore revocation".into(),
        ));
    }
    let before = required_raw_hash(
        &collect_scope_manifests(&watched.root)?,
        "",
        "same-length.txt",
    )?;
    first.stop(&options.binary, &watched.device, &watched.root)?;

    apply_stopped_ignore_mutation(&watched.root)?;
    apply_stopped_ignore_mutation(&manual.root)?;
    replace_same_length_preserving_mtime(&watched.root.join("same-length.txt"))?;
    replace_same_length_preserving_mtime(&manual.root.join("same-length.txt"))?;
    let mut restarted = WatchChild::start(&options.binary, &watched.device, &watched.root)?;
    wait_running(options, &watched, &mut restarted)?;
    let after_scopes = collect_scope_manifests(&watched.root)?;
    assert_ignored_child_not_indexed(&after_scopes, &before_ignored)?;
    assert_effective_search_excludes(options, &watched, PRE_IGNORED_SEARCH_TOKEN)?;
    assert_effective_search_excludes(options, &watched, POST_IGNORED_SEARCH_TOKEN)?;
    command_must_fail(
        &options.binary,
        &watched.device,
        &watched.root.join("managed-then-ignored"),
        &["--json", "index", "--offline"],
        "targeted ignored child index unexpectedly succeeded",
    )?;
    let after = required_raw_hash(&after_scopes, "", "same-length.txt")?;
    if before == after {
        return Err(AcceptanceError::Command(
            "native watcher restart did not update same-length preserved-mtime bytes".into(),
        ));
    }
    assert_manifest_bytes(&watched.root, &after_scopes)?;
    restarted.stop(&options.binary, &watched.device, &watched.root)?;

    // The driver uses the shipped binary as its reconciler. It does not fake a
    // Kio outcome or alter release-only controls; queue injection is the bounded
    // representation of notifications that a real backend failed to deliver.
    run_queue_fault_driver(options, &watched)?;
    apply_queue_driver_mutations(&manual.root)?;
    command(
        &options.binary,
        &manual.device,
        Some(&manual.root),
        &["--json", "index", "--offline"],
    )?;
    verify_inputs(options)?;
    let watched_scopes = collect_scope_manifests(&watched.root)?;
    let manual_scopes = collect_scope_manifests(&manual.root)?;
    assert_mirror_equivalence(&watched.root, &watched_scopes, &manual.root, &manual_scopes)?;
    if required_raw_hash(&watched_scopes, "", "same-length.txt")? != after {
        return Err(AcceptanceError::Command(
            "queue fault driver regressed the recovered same-length hash".into(),
        ));
    }
    verify_moved_root_replacement_fails_closed(options, &watched)?;

    let receipt = AcceptanceReceipt {
        schema: "kio.acceptance.receipt/v4".into(),
        requirement: requirement.clone(),
        candidate: options.expected.candidate.clone(),
        fixture: options.expected.fixture.clone(),
        workflow: options.expected.workflow.clone(),
        evaluator_sha256: current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?,
        service_identity_sha256: options.expected.service_identity_sha256.clone(),
        contract_binary_sha256: options.expected.contract_binary_sha256.clone(),
        runtime: empty_runtime(),
        passed: true,
    };
    write_receipt_create_only(&options.receipt, &receipt)?;
    Ok(receipt)
}

fn populate_a03_initial(root: &Path, seed: &[u8]) -> Result<(), AcceptanceError> {
    write_file(&root.join("seed.md"), seed)?;
    write_file(&root.join("same-length.txt"), &[b'A'; 64])?;
    write_file(
        &root.join("managed-then-ignored/visible-before-revocation.txt"),
        format!(
            "managed child becomes ignored while watcher is stopped {PRE_IGNORED_SEARCH_TOKEN}\n"
        )
        .as_bytes(),
    )
}

fn apply_stopped_ignore_mutation(root: &Path) -> Result<(), AcceptanceError> {
    write_file(&root.join(".kioignore"), b"managed-then-ignored/\n")?;
    write_file(
        &root.join("managed-then-ignored/hidden-after-revocation.txt"),
        format!("must not be indexed after Ignore revocation {POST_IGNORED_SEARCH_TOKEN}\n")
            .as_bytes(),
    )
}

fn assert_ignored_child_not_indexed(
    manifests: &ScopeManifests,
    before_ignored: &ScopeFiles,
) -> Result<(), AcceptanceError> {
    let Some(after_ignored) = manifests.get("managed-then-ignored") else {
        return Err(AcceptanceError::Command(
            "watch restart erased the historical manifest for an ignored child".into(),
        ));
    };
    if after_ignored != before_ignored || after_ignored.contains_key("hidden-after-revocation.txt")
    {
        return Err(AcceptanceError::Command(
            "watch restart changed an ignored child's historical manifest".into(),
        ));
    }
    Ok(())
}

fn assert_effective_search_excludes(
    options: &NativeOptions,
    watched: &DeviceRoot,
    query: &str,
) -> Result<(), AcceptanceError> {
    for (args, label) in [
        (
            vec![
                "--json",
                "search",
                query,
                "--scope",
                ".",
                "--descendants",
                "--mode",
                "text",
                "--offline",
            ],
            "root descendant search",
        ),
        (
            vec![
                "--json",
                "search",
                query,
                "--all-scopes",
                "--mode",
                "text",
                "--offline",
            ],
            "all-scope search",
        ),
    ] {
        let value = json(&options.binary, &watched.device, &watched.root, &args)?;
        let results = value
            .get("results")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                AcceptanceError::Command(format!("{label} omitted its results array"))
            })?;
        if !results.is_empty() {
            return Err(AcceptanceError::Command(format!(
                "{label} returned an ignored child result"
            )));
        }
    }
    Ok(())
}

/// A moved root must never inherit authority through its old pathname or a
/// device-local watcher state record. Explicit re-registration is then the
/// only path that restores ordinary indexing at the moved location.
fn verify_moved_root_replacement_fails_closed(
    options: &NativeOptions,
    watched: &DeviceRoot,
) -> Result<(), AcceptanceError> {
    let managed_before = collect_scope_manifests(&watched.root)?;
    let moved = watched.device.join("moved-root");
    fs::rename(&watched.root, &moved).map_err(io)?;
    fs::create_dir(&watched.root).map_err(io)?;
    private_dir(&watched.root)?;

    watch_run_must_fail(
        &options.binary,
        &watched.device,
        &watched.root,
        "replacement root unexpectedly inherited watcher authority",
    )?;
    command_must_fail(
        &options.binary,
        &watched.device,
        &watched.root,
        &["--json", "index", "--offline"],
        "replacement root unexpectedly inherited indexing authority",
    )?;
    command_must_fail(
        &options.binary,
        &watched.device,
        &moved,
        &["--json", "index", "--offline"],
        "moved root remained indexable without explicit re-registration",
    )?;

    let preview_before = tree_fingerprint(&moved)?;
    let identity_before = scope_identity_and_heads(&moved, &managed_before)?;
    let moved_text = moved
        .to_str()
        .ok_or_else(|| AcceptanceError::Invalid("moved root is not UTF-8".into()))?;
    let preview = json(
        &options.binary,
        &watched.device,
        &moved,
        &["--json", "root", "register", moved_text, "--preview"],
    )?;
    if preview.get("preview").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(AcceptanceError::Command(
            "root register preview did not report preview mode".into(),
        ));
    }
    if tree_fingerprint(&moved)? != preview_before {
        return Err(AcceptanceError::Command(
            "root register preview changed managed knowledge or authority".into(),
        ));
    }

    let registered = json(
        &options.binary,
        &watched.device,
        &moved,
        &["--json", "root", "register", moved_text, "--yes"],
    )?;
    if registered.get("status").and_then(serde_json::Value::as_str) != Some("registered") {
        return Err(AcceptanceError::Command(
            "root register did not report a completed registration".into(),
        ));
    }
    let after_register = collect_scope_manifests(&moved)?;
    if after_register != managed_before
        || scope_identity_and_heads(&moved, &after_register)? != identity_before
    {
        return Err(AcceptanceError::Command(
            "root register changed managed scope identity, HEAD, or knowledge".into(),
        ));
    }

    command(
        &options.binary,
        &watched.device,
        Some(&moved),
        &["--json", "index", "--offline"],
    )?;
    let after_index = collect_scope_manifests(&moved)?;
    if after_index != managed_before {
        return Err(AcceptanceError::Command(
            "ordinary index after root registration changed the managed scope set".into(),
        ));
    }
    assert_global_search_rebound_from_old_root(options, watched, &moved)
}

fn scope_identity_and_heads(
    root: &Path,
    manifests: &ScopeManifests,
) -> Result<BTreeMap<String, (String, String)>, AcceptanceError> {
    let mut result = BTreeMap::new();
    for scope in manifests.keys() {
        let directory = if scope.is_empty() {
            root.to_path_buf()
        } else {
            root.join(scope)
        };
        let scope_id = read_scope_id(&directory)?;
        let head = String::from_utf8(bounded_regular_bytes(
            &directory.join(".kio/HEAD"),
            MAX_MANIFEST_BYTES,
        )?)
        .map_err(|_| AcceptanceError::Invalid("scope HEAD is not UTF-8".into()))?;
        result.insert(scope.clone(), (scope_id, head));
    }
    Ok(result)
}

fn assert_global_search_rebound_from_old_root(
    options: &NativeOptions,
    watched: &DeviceRoot,
    moved: &Path,
) -> Result<(), AcceptanceError> {
    let value = json(
        &options.binary,
        &watched.device,
        moved,
        &[
            "--json",
            "search",
            "periodic",
            "--all-scopes",
            "--mode",
            "text",
            "--offline",
        ],
    )?;
    let scopes = value
        .get("searched_scopes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            AcceptanceError::Command("global search omitted searched scopes after rebind".into())
        })?;
    let old = watched.root.display().to_string();
    let new = moved.display().to_string();
    if scopes.iter().any(|scope| {
        scope
            .get("scope_path")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|path| path.starts_with(&old))
    }) || !scopes.iter().any(|scope| {
        scope
            .get("scope_path")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|path| path.starts_with(&new))
    }) {
        return Err(AcceptanceError::Command(
            "global search retained an old root registration or omitted its rebound root".into(),
        ));
    }
    Ok(())
}

fn replace_same_length_preserving_mtime(path: &Path) -> Result<(), AcceptanceError> {
    let before = fs::metadata(path).map_err(io)?;
    let old_time = before.modified().map_err(io)?;
    let bytes = bounded_regular_bytes(path, MAX_FIXTURE_BYTES)?;
    let replacement = vec![b'B'; bytes.len()];
    if replacement.len() != bytes.len() || replacement == bytes {
        return Err(AcceptanceError::Invalid(
            "same-length A03 fixture is not replaceable".into(),
        ));
    }
    fs::write(path, replacement).map_err(io)?;
    File::open(path)
        .map_err(io)?
        .set_times(FileTimes::new().set_modified(old_time))
        .map_err(io)?;
    let after = fs::metadata(path).map_err(io)?;
    if after.len() != before.len() || after.modified().map_err(io)? != old_time {
        return Err(AcceptanceError::Command(
            "same-length preserved-mtime mutation was not retained".into(),
        ));
    }
    Ok(())
}

fn required_raw_hash(
    manifests: &ScopeManifests,
    scope: &str,
    path: &str,
) -> Result<String, AcceptanceError> {
    manifests
        .get(scope)
        .and_then(|files| files.get(path))
        .cloned()
        .ok_or_else(|| AcceptanceError::Command(format!("native scope is missing {scope}/{path}")))
}

#[derive(Default)]
struct CliReconcilerState {
    calls: AtomicUsize,
    failure: Mutex<Option<String>>,
}

struct CliReconciler {
    binary: PathBuf,
    isolated: PathBuf,
    root: PathBuf,
    state: Arc<CliReconcilerState>,
}

impl Reconcile for CliReconciler {
    fn reconcile(
        &self,
        root: &WatchRoot,
        _reason: DirtyReason,
        _changed_paths: &[PathBuf],
    ) -> Result<ReconcileCompletion, WatchError> {
        if root.canonical_root != self.root {
            *self.state.failure.lock().expect("driver failure lock") =
                Some("production queue supplied a different registered root".into());
            return Ok(ReconcileCompletion::Incomplete);
        }
        self.state.calls.fetch_add(1, Ordering::Relaxed);
        match command(
            &self.binary,
            &self.isolated,
            Some(&self.root),
            &["--json", "index", "--offline"],
        ) {
            Ok(_) => Ok(ReconcileCompletion::Complete),
            Err(error) => {
                *self.state.failure.lock().expect("driver failure lock") = Some(error.to_string());
                Ok(ReconcileCompletion::Incomplete)
            }
        }
    }
}

fn run_queue_fault_driver(
    options: &NativeOptions,
    device: &DeviceRoot,
) -> Result<(), AcceptanceError> {
    let scope_id = read_scope_id(&device.root)?;
    let root = WatchRoot::new(scope_id, device.root.clone(), 1)
        .map_err(|error| AcceptanceError::Command(error.to_string()))?;
    let queue_path = device.tmp.join("a03-watch-queue.sqlite");
    let state = Arc::new(CliReconcilerState::default());
    let engine = WatchEngine::open_with_interval(
        &queue_path,
        [root.clone()],
        CliReconciler {
            binary: options.binary.clone(),
            isolated: device.device.clone(),
            root: device.root.clone(),
            state: Arc::clone(&state),
        },
        Duration::from_secs(1),
    )
    .map_err(|error| AcceptanceError::Command(error.to_string()))?;
    drain_engine(&engine, &state, 1, "startup queue recovery")?;

    // No enqueue follows this mutation: the one-second production periodic
    // reconciler is the recovery path for an intentionally dropped native hint.
    write_file(
        &device.root.join("periodic-recovery.txt"),
        b"periodic recovery bytes\n",
    )?;
    thread::sleep(Duration::from_millis(1100));
    drain_engine(&engine, &state, 2, "dropped-notification periodic recovery")?;
    assert_engine_idle(&engine, &state, "dropped-notification periodic recovery")?;
    drop(engine);

    write_file(
        &device.root.join("overflow-recovery.txt"),
        b"overflow recovery bytes\n",
    )?;
    // Keep the periodic interval out of this leg: the claimed work below must
    // be the over-capacity collapse, rather than a coincident periodic scan.
    let overflow_state = Arc::new(CliReconcilerState::default());
    let overflow_engine = WatchEngine::open_with_interval(
        &queue_path,
        [root.clone()],
        CliReconciler {
            binary: options.binary.clone(),
            isolated: device.device.clone(),
            root: device.root.clone(),
            state: Arc::clone(&overflow_state),
        },
        Duration::from_secs(60),
    )
    .map_err(|error| AcceptanceError::Command(error.to_string()))?;
    drain_engine(
        &overflow_engine,
        &overflow_state,
        1,
        "overflow driver startup",
    )?;
    // A native notification can contain a burst of path hints. Exercise the
    // production batch API and its durable over-capacity collapse in one
    // transaction; thousands of separate fsyncs are not an acceptance gate.
    let hints = (0..=QUEUE_CAPACITY)
        .map(|index| device.root.join(format!("overflow-hint-{index}")))
        .collect::<Vec<_>>();
    let outcome = overflow_engine
        .enqueue_events(&root, &hints, DirtyReason::Native)
        .map_err(|error| AcceptanceError::Command(error.to_string()))?;
    if outcome != EnqueueOutcome::CollapsedToFullScan {
        return Err(AcceptanceError::Command(
            "production dirty queue did not collapse over-capacity hints".into(),
        ));
    }
    drain_engine(
        &overflow_engine,
        &overflow_state,
        2,
        "overflow full reconciliation",
    )?;
    assert_engine_idle(&overflow_engine, &overflow_state, "overflow recovery")?;
    drop(overflow_engine);

    // Claim durable work, simulate only process loss by dropping its private
    // queue handle, then reopen the real WatchEngine against that same SQLite.
    {
        let mut queue = DirtyQueue::open_private(&queue_path)
            .map_err(|error| AcceptanceError::Command(error.to_string()))?;
        queue
            .enqueue_full(&root, DirtyReason::Manual)
            .map_err(|error| AcceptanceError::Command(error.to_string()))?;
        if queue
            .claim_ready(
                SystemTime::now(),
                Duration::ZERO,
                Duration::ZERO,
                Duration::from_secs(30),
            )
            .map_err(|error| AcceptanceError::Command(error.to_string()))?
            .is_none()
        {
            return Err(AcceptanceError::Command(
                "production dirty queue did not claim restart work".into(),
            ));
        }
    }
    let restart_state = Arc::new(CliReconcilerState::default());
    let restarted = WatchEngine::open_with_interval(
        &queue_path,
        [root],
        CliReconciler {
            binary: options.binary.clone(),
            isolated: device.device.clone(),
            root: device.root.clone(),
            state: Arc::clone(&restart_state),
        },
        Duration::from_secs(60),
    )
    .map_err(|error| AcceptanceError::Command(error.to_string()))?;
    drain_engine(
        &restarted,
        &restart_state,
        1,
        "claimed-queue restart recovery",
    )?;
    assert_engine_idle(&restarted, &restart_state, "claimed-queue restart recovery")
}

fn drain_engine(
    engine: &WatchEngine<CliReconciler>,
    state: &CliReconcilerState,
    minimum_calls: usize,
    phase: &str,
) -> Result<(), AcceptanceError> {
    let deadline = Instant::now() + WATCH_TIMEOUT;
    let mut iterations = 0usize;
    loop {
        iterations += 1;
        if iterations > 128 {
            return Err(AcceptanceError::Command(format!(
                "production watch engine exceeded bounded reconciliation iterations for {phase}"
            )));
        }
        engine
            .reconcile_once()
            .map_err(|error| AcceptanceError::Command(error.to_string()))?;
        if let Some(failure) = state.failure.lock().expect("driver failure lock").clone() {
            return Err(AcceptanceError::Command(format!(
                "production CLI reconciler failed during {phase}: {failure}"
            )));
        }
        let status = engine
            .status()
            .map_err(|error| AcceptanceError::Command(error.to_string()))?;
        if state.calls.load(Ordering::Relaxed) >= minimum_calls
            && status.backlog == 0
            && !status.degraded
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(AcceptanceError::Command(format!(
                "production watch engine did not converge for {phase}"
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn assert_engine_idle(
    engine: &WatchEngine<CliReconciler>,
    state: &CliReconcilerState,
    phase: &str,
) -> Result<(), AcceptanceError> {
    let calls = state.calls.load(Ordering::Relaxed);
    for _ in 0..3 {
        if engine
            .reconcile_once()
            .map_err(|error| AcceptanceError::Command(error.to_string()))?
        {
            return Err(AcceptanceError::Command(format!(
                "production watch engine retained self-generated work after {phase}"
            )));
        }
    }
    if state.calls.load(Ordering::Relaxed) != calls
        || engine
            .status()
            .map_err(|error| AcceptanceError::Command(error.to_string()))?
            .backlog
            != 0
    {
        return Err(AcceptanceError::Command(format!(
            "production watch engine did not become idle after {phase}"
        )));
    }
    Ok(())
}

fn apply_queue_driver_mutations(root: &Path) -> Result<(), AcceptanceError> {
    write_file(
        &root.join("periodic-recovery.txt"),
        b"periodic recovery bytes\n",
    )?;
    write_file(
        &root.join("overflow-recovery.txt"),
        b"overflow recovery bytes\n",
    )
}

fn read_scope_id(root: &Path) -> Result<String, AcceptanceError> {
    let bytes = bounded_regular_bytes(&root.join(".kio/scope.json"), MAX_MANIFEST_BYTES)?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?
        .get("scope_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| AcceptanceError::Invalid("managed root scope.json lacks scope_id".into()))
}

struct DeviceRoot {
    device: PathBuf,
    root: PathBuf,
    tmp: PathBuf,
}

fn create_device_root(work: &Path, name: &str) -> Result<DeviceRoot, AcceptanceError> {
    let device = work.join(name);
    let root = device.join("root");
    fs::create_dir_all(&device).map_err(io)?;
    private_dir(&device)?;
    for path in [
        &root,
        &device.join("config"),
        &device.join("data"),
        &device.join("cache"),
        &device.join("tmp"),
    ] {
        fs::create_dir_all(path).map_err(io)?;
        private_dir(path)?;
    }
    Ok(DeviceRoot {
        device: device.canonicalize().map_err(io)?,
        root: root.canonicalize().map_err(io)?,
        tmp: device.join("tmp").canonicalize().map_err(io)?,
    })
}

fn populate_initial(root: &Path, seed: &[u8]) -> Result<(), AcceptanceError> {
    write_file(&root.join("seed.md"), seed)?;
    write_file(&root.join("delete-me.txt"), b"delete me\n")?;
    write_file(&root.join("rename-me.txt"), b"rename me\n")?;
    write_file(&root.join("move-source/move-me.txt"), b"move me\n")
}

fn apply_mutations(root: &Path, seed: &[u8]) -> Result<(), AcceptanceError> {
    // Empty, newly-created, and more-than-32-level-deep folders must all be
    // discovered. The leaf file gives the deepest scope a byte assertion too.
    fs::create_dir_all(root.join("unmanaged-empty")).map_err(io)?;
    let mut deep = root.join("deep");
    for level in 0..33 {
        deep.push(format!("level-{level:02}"));
    }
    write_file(&deep.join("deep-entry.txt"), b"deep native watcher bytes\n")?;
    write_file(
        &root.join("new-folder/new-entry.txt"),
        b"new native watcher bytes\n",
    )?;
    write_file(&root.join("seed.md"), &[seed, b"updated by A02\n"].concat())?;
    fs::remove_file(root.join("delete-me.txt")).map_err(io)?;
    fs::rename(root.join("rename-me.txt"), root.join("renamed.txt")).map_err(io)?;
    fs::create_dir_all(root.join("move-destination")).map_err(io)?;
    fs::rename(
        root.join("move-source/move-me.txt"),
        root.join("move-destination/moved.txt"),
    )
    .map_err(io)?;
    // This is both a content exclusion and a child-scope exclusion; neither
    // side of the comparison may contain an ignored scope or path.
    write_file(&root.join(".kioignore"), b"ignored/\n")?;
    write_file(&root.join("ignored/hidden.txt"), b"must never be indexed\n")
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), AcceptanceError> {
    let parent = path
        .parent()
        .ok_or_else(|| AcceptanceError::Invalid("fixture path has no parent".into()))?;
    fs::create_dir_all(parent).map_err(io)?;
    fs::write(path, bytes).map_err(io)
}

/// Build a real native link boundary. This is an acceptance prerequisite, not
/// an optional probe: a native watcher receipt cannot claim boundary behavior
/// on an OS where the test runner could not create the boundary.
#[cfg(unix)]
fn create_boundary_link(link: &Path, target: &Path) -> Result<(), AcceptanceError> {
    std::os::unix::fs::symlink(target, link).map_err(io)
}

/// Directory junctions do not require Developer Mode or a symlink privilege,
/// unlike Windows symbolic links. `mklink /J` is the OS-native fixture setup
/// used to exercise the same reparse-point boundary on the hosted runner.
#[cfg(windows)]
fn create_boundary_link(link: &Path, target: &Path) -> Result<(), AcceptanceError> {
    let link = link
        .to_str()
        .ok_or_else(|| AcceptanceError::Invalid("junction path is not UTF-8".into()))?;
    let target = target
        .to_str()
        .ok_or_else(|| AcceptanceError::Invalid("junction target is not UTF-8".into()))?;
    let output = Command::new("cmd")
        .args([
            "/D",
            "/S",
            "/C",
            &format!("mklink /J \"{link}\" \"{target}\""),
        ])
        .output()
        .map_err(io)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(AcceptanceError::Command(format!(
            "could not create required Windows junction: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(not(any(unix, windows)))]
fn create_boundary_link(_link: &Path, _target: &Path) -> Result<(), AcceptanceError> {
    Err(AcceptanceError::Invalid(
        "native link-boundary acceptance is unsupported on this platform".into(),
    ))
}

/// Capture every regular byte beneath a small sentinel tree without following
/// links. The acceptance fixture is bounded so this remains a precise guard,
/// rather than an unbounded filesystem walk.
fn tree_fingerprint(root: &Path) -> Result<BTreeMap<PathBuf, String>, AcceptanceError> {
    let mut entries = BTreeMap::new();
    tree_fingerprint_at(root, root, &mut entries)?;
    Ok(entries)
}

fn tree_fingerprint_at(
    root: &Path,
    current: &Path,
    entries: &mut BTreeMap<PathBuf, String>,
) -> Result<(), AcceptanceError> {
    if entries.len() >= MAX_DIRECTORY_ENTRIES {
        return Err(AcceptanceError::Invalid(
            "outside-root sentinel exceeds entry bound".into(),
        ));
    }
    for entry in fs::read_dir(current).map_err(io)? {
        let entry = entry.map_err(io)?;
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(|_| {
            AcceptanceError::Invalid("outside-root sentinel path escaped its root".into())
        })?;
        let metadata = fs::symlink_metadata(&path).map_err(io)?;
        if metadata.file_type().is_symlink() {
            entries.insert(relative.to_path_buf(), "symlink".into());
        } else if metadata.is_dir() {
            entries.insert(relative.to_path_buf(), "directory".into());
            tree_fingerprint_at(root, &path, entries)?;
        } else if metadata.is_file() {
            entries.insert(
                relative.to_path_buf(),
                format!(
                    "file:{}",
                    sha256_bytes(&bounded_regular_bytes(&path, MAX_FIXTURE_BYTES)?)
                ),
            );
        } else {
            return Err(AcceptanceError::Invalid(
                "outside-root sentinel contains an unsupported entry".into(),
            ));
        }
    }
    Ok(())
}

struct WatchChild {
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl WatchChild {
    fn start(binary: &Path, isolated: &Path, root: &Path) -> Result<Self, AcceptanceError> {
        let stdout = isolated.join("watch.stdout");
        let stderr = isolated.join("watch.stderr");
        let mut command = base(binary, isolated, Some(root))?;
        command
            .args([
                "--json",
                "watch",
                "run",
                "--reconcile-interval-seconds",
                "1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::from(File::create(&stdout).map_err(io)?))
            .stderr(Stdio::from(File::create(&stderr).map_err(io)?));
        Ok(Self {
            child: command.spawn().map_err(io)?,
            stdout,
            stderr,
        })
    }

    fn check(&mut self) -> Result<(), AcceptanceError> {
        self.check_output_size()?;
        if let Some(status) = self.child.try_wait().map_err(io)? {
            return Err(AcceptanceError::Command(format!(
                "native watcher exited unexpectedly with {status}"
            )));
        }
        Ok(())
    }

    fn stop(&mut self, binary: &Path, isolated: &Path, root: &Path) -> Result<(), AcceptanceError> {
        command(binary, isolated, Some(root), &["--json", "watch", "stop"])?;
        let deadline = Instant::now() + WATCH_TIMEOUT;
        loop {
            self.check_output_size()?;
            if self.child.try_wait().map_err(io)?.is_some() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(AcceptanceError::Command(
                    "native watcher did not stop".into(),
                ));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn check_output_size(&self) -> Result<(), AcceptanceError> {
        for path in [&self.stdout, &self.stderr] {
            if fs::metadata(path).map_err(io)?.len() > MAX_WATCH_OUTPUT_BYTES {
                return Err(AcceptanceError::Command(
                    "native watcher exceeded bounded output budget".into(),
                ));
            }
        }
        Ok(())
    }
}

impl Drop for WatchChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn wait_running(
    options: &NativeOptions,
    device: &DeviceRoot,
    child: &mut WatchChild,
) -> Result<u64, AcceptanceError> {
    wait_until(options, device, child, None, "initial reconciliation")
}

fn wait_for_convergence(
    options: &NativeOptions,
    device: &DeviceRoot,
    child: &mut WatchChild,
    after_success: u64,
    phase: &str,
) -> Result<(), AcceptanceError> {
    wait_until(options, device, child, Some(after_success), phase).map(|_| ())
}

fn wait_until(
    options: &NativeOptions,
    device: &DeviceRoot,
    child: &mut WatchChild,
    after_success: Option<u64>,
    phase: &str,
) -> Result<u64, AcceptanceError> {
    let deadline = Instant::now() + WATCH_TIMEOUT;
    loop {
        child.check()?;
        // Re-read the externally supplied fixture on each poll. This prevents a
        // changed fixture from silently becoming evidence while the watcher runs.
        verify_inputs(options)?;
        let value = json(
            &options.binary,
            &device.device,
            &device.root,
            &["--json", "watch", "status"],
        )?;
        let success = value
            .pointer("/last_observation/last_success_ms")
            .and_then(serde_json::Value::as_u64);
        if value.pointer("/status").and_then(serde_json::Value::as_str) == Some("running")
            && success.is_some_and(|success| after_success.is_none_or(|after| success > after))
            && value
                .pointer("/last_observation/backlog")
                .and_then(serde_json::Value::as_u64)
                == Some(0)
            && value
                .pointer("/last_observation/degraded")
                .and_then(serde_json::Value::as_bool)
                == Some(false)
        {
            return Ok(success.expect("checked above"));
        }
        if Instant::now() >= deadline {
            return Err(AcceptanceError::Command(format!(
                "native watcher did not converge for {phase}"
            )));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

type ScopeFiles = BTreeMap<String, String>;
type ScopeManifests = BTreeMap<String, ScopeFiles>;

fn collect_scope_manifests(root: &Path) -> Result<ScopeManifests, AcceptanceError> {
    let mut manifests = BTreeMap::new();
    collect_scope_manifests_at(root, root, &mut manifests)?;
    if manifests.len() > MAX_SCOPE_COUNT || !manifests.contains_key("") {
        return Err(AcceptanceError::Invalid(
            "native scope manifest collection is incomplete or exceeds its bound".into(),
        ));
    }
    Ok(manifests)
}

fn collect_scope_manifests_at(
    root: &Path,
    directory: &Path,
    manifests: &mut ScopeManifests,
) -> Result<(), AcceptanceError> {
    let manifest = directory.join(".kio/manifest.json");
    if manifest.is_file() {
        let relative = relative_directory(root, directory)?;
        if manifests
            .insert(relative, parse_manifest(&manifest)?)
            .is_some()
        {
            return Err(AcceptanceError::Invalid(
                "duplicate native scope manifest".into(),
            ));
        }
    }
    let mut entries = 0usize;
    for entry in fs::read_dir(directory).map_err(io)? {
        entries += 1;
        if entries > MAX_DIRECTORY_ENTRIES {
            return Err(AcceptanceError::Invalid(
                "native fixture directory exceeds entry bound".into(),
            ));
        }
        let entry = entry.map_err(io)?;
        let kind = entry.file_type().map_err(io)?;
        if kind.is_dir() && entry.file_name() != ".kio" {
            collect_scope_manifests_at(root, &entry.path(), manifests)?;
        }
    }
    Ok(())
}

fn parse_manifest(path: &Path) -> Result<ScopeFiles, AcceptanceError> {
    let bytes = bounded_regular_bytes(path, MAX_MANIFEST_BYTES)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| AcceptanceError::Json(error.to_string()))?;
    let files = value
        .get("files")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| AcceptanceError::Invalid("scope manifest missing files".into()))?;
    let mut result = BTreeMap::new();
    for file in files {
        let object = file.as_object().ok_or_else(|| {
            AcceptanceError::Invalid("scope manifest file is not an object".into())
        })?;
        let path = object
            .get("path")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty() && !value.contains('/'))
            .ok_or_else(|| AcceptanceError::Invalid("scope manifest has invalid path".into()))?;
        let hash = object
            .get("raw_hash")
            .and_then(serde_json::Value::as_str)
            .filter(|value| is_raw_hash(value))
            .ok_or_else(|| {
                AcceptanceError::Invalid("scope manifest has invalid raw hash".into())
            })?;
        let status = object
            .get("status")
            .and_then(serde_json::Value::as_str)
            .filter(|value| matches!(*value, "new" | "modified" | "deleted" | "unchanged"))
            .ok_or_else(|| AcceptanceError::Invalid("scope manifest has invalid status".into()))?;
        if status != "deleted" && result.insert(path.to_owned(), hash.to_owned()).is_some() {
            return Err(AcceptanceError::Invalid(
                "scope manifest duplicates a live path".into(),
            ));
        }
    }
    Ok(result)
}

fn assert_scope_equivalence(
    watched_root: &Path,
    watched: &ScopeManifests,
    manual_root: &Path,
    manual: &ScopeManifests,
) -> Result<(), AcceptanceError> {
    assert_mirror_equivalence(watched_root, watched, manual_root, manual)?;
    assert_expected_final_state(watched)?;
    if !watched.contains_key("unmanaged-empty")
        || !watched.keys().any(|scope| scope.matches('/').count() >= 33)
    {
        return Err(AcceptanceError::Command(
            "native watcher did not retain empty and deep discovered scopes".into(),
        ));
    }
    Ok(())
}

fn assert_mirror_equivalence(
    watched_root: &Path,
    watched: &ScopeManifests,
    manual_root: &Path,
    manual: &ScopeManifests,
) -> Result<(), AcceptanceError> {
    if watched != manual {
        return Err(AcceptanceError::Command(
            "native watcher final scope/path/raw-hash state differs from manual index mirror"
                .into(),
        ));
    }
    if watched
        .keys()
        .any(|scope| scope == "ignored" || scope.starts_with("ignored/"))
        || watched
            .values()
            .any(|files| files.contains_key("hidden.txt"))
    {
        return Err(AcceptanceError::Command(
            "native watcher indexed an ignored scope or file".into(),
        ));
    }
    // Manifest equality alone could compare two stale writers. Bind every
    // reported live raw hash to the actual regular bytes beneath both roots.
    assert_manifest_bytes(watched_root, watched)?;
    assert_manifest_bytes(manual_root, manual)?;
    Ok(())
}

fn assert_expected_final_state(manifests: &ScopeManifests) -> Result<(), AcceptanceError> {
    let root = manifests
        .get("")
        .ok_or_else(|| AcceptanceError::Command("native root scope is missing".into()))?;
    if !root.contains_key("seed.md")
        || !root.contains_key("renamed.txt")
        || root.contains_key("delete-me.txt")
        || root.contains_key("rename-me.txt")
    {
        return Err(AcceptanceError::Command(
            "native watcher did not apply the root add/update/delete/rename state".into(),
        ));
    }
    let moved = manifests
        .get("move-destination")
        .ok_or_else(|| AcceptanceError::Command("native moved-to scope is missing".into()))?;
    if !moved.contains_key("moved.txt")
        || manifests
            .get("move-source")
            .is_some_and(|files| files.contains_key("move-me.txt"))
    {
        return Err(AcceptanceError::Command(
            "native watcher did not apply the file move state".into(),
        ));
    }
    Ok(())
}

fn assert_manifest_bytes(root: &Path, manifests: &ScopeManifests) -> Result<(), AcceptanceError> {
    for (scope, files) in manifests {
        let scope_root = if scope.is_empty() {
            root.to_path_buf()
        } else {
            root.join(scope)
        };
        for (name, raw_hash) in files {
            let actual = bounded_regular_bytes(&scope_root.join(name), MAX_FIXTURE_BYTES)?;
            let expected = format!("sha256:{}", sha256_bytes(&actual));
            if raw_hash != &expected {
                return Err(AcceptanceError::Command(
                    "scope manifest raw hash does not bind the current file bytes".into(),
                ));
            }
        }
    }
    Ok(())
}

fn relative_directory(root: &Path, directory: &Path) -> Result<String, AcceptanceError> {
    let relative = directory.strip_prefix(root).map_err(|_| {
        AcceptanceError::Invalid("native scope is outside its canonical fixture root".into())
    })?;
    if relative.as_os_str().is_empty() {
        return Ok(String::new());
    }
    let value = relative
        .to_str()
        .ok_or_else(|| AcceptanceError::Invalid("native scope path is not UTF-8".into()))?
        .replace('\\', "/");
    if value
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(AcceptanceError::Invalid(
            "native scope relative path is invalid".into(),
        ));
    }
    Ok(value)
}

fn is_raw_hash(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn verify_inputs(options: &NativeOptions) -> Result<(), AcceptanceError> {
    if sha256_regular_file(&options.binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
    {
        return Err(AcceptanceError::Invalid(
            "binary hash differs from candidate binding".into(),
        ));
    }
    let fixture = bounded_regular_bytes(&options.fixture, MAX_FIXTURE_BYTES)?;
    if sha256_bytes(&fixture) != options.expected.fixture.sha256 {
        return Err(AcceptanceError::Invalid(
            "fixture differs from candidate binding".into(),
        ));
    }
    Ok(())
}

fn bounded_regular_bytes(path: &Path, maximum: u64) -> Result<Vec<u8>, AcceptanceError> {
    let metadata = fs::symlink_metadata(path).map_err(io)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > maximum {
        return Err(AcceptanceError::Invalid(
            "native acceptance input must be a bounded regular file".into(),
        ));
    }
    fs::read(path).map_err(io)
}

fn json(
    binary: &Path,
    isolated: &Path,
    root: &Path,
    args: &[&str],
) -> Result<serde_json::Value, AcceptanceError> {
    serde_json::from_slice(&command(binary, isolated, Some(root), args)?)
        .map_err(|e| AcceptanceError::Json(e.to_string()))
}

fn command(
    binary: &Path,
    isolated: &Path,
    root: Option<&Path>,
    args: &[&str],
) -> Result<Vec<u8>, AcceptanceError> {
    let output = base(binary, isolated, root)?
        .args(args)
        .output()
        .map_err(io)?;
    if !output.status.success() {
        return Err(AcceptanceError::Command(format!(
            "{} failed with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn command_must_fail(
    binary: &Path,
    isolated: &Path,
    root: &Path,
    args: &[&str],
    success_message: &str,
) -> Result<(), AcceptanceError> {
    let output = base(binary, isolated, Some(root))?
        .args(args)
        .output()
        .map_err(io)?;
    if output.status.success() {
        return Err(AcceptanceError::Command(success_message.into()));
    }
    Ok(())
}

fn watch_run_must_fail(
    binary: &Path,
    isolated: &Path,
    root: &Path,
    success_message: &str,
) -> Result<(), AcceptanceError> {
    let mut command = base(binary, isolated, Some(root))?;
    command
        .args([
            "--json",
            "watch",
            "run",
            "--reconcile-interval-seconds",
            "1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(io)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().map_err(io)? {
            if status.success() {
                return Err(AcceptanceError::Command(success_message.into()));
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AcceptanceError::Command(success_message.into()));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn base(binary: &Path, isolated: &Path, root: Option<&Path>) -> Result<Command, AcceptanceError> {
    let mut command = Command::new(binary);
    IsolatedChildEnvironment::new(
        isolated,
        isolated.join("config"),
        isolated.join("data"),
        isolated.join("cache"),
        isolated.join("tmp"),
    )
    .apply(&mut command)?;
    if let Some(root) = root {
        command.current_dir(root);
    }
    Ok(command)
}

#[cfg(unix)]
fn private_dir(path: &Path) -> Result<(), AcceptanceError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io)
}

#[cfg(not(unix))]
fn private_dir(_path: &Path) -> Result<(), AcceptanceError> {
    Ok(())
}

fn io(error: std::io::Error) -> AcceptanceError {
    AcceptanceError::Io(error.to_string())
}
