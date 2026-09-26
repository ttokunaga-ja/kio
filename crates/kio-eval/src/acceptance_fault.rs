//! A10 crash-boundary acceptance driver.
//!
//! This module deliberately has no product-side fault injection.  It drives a
//! separately supplied debug contract binary through the narrow durability
//! barrier protocol, then uses the shipped release candidate for every normal
//! initialization, recovery, and smoke command.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use kio_core::{
    cas::{ObjectKind, hash_bytes, is_hash},
    dag::CommitType,
    scope::Repository,
};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::Value;

use crate::acceptance::{
    AcceptanceCase, AcceptanceError, AcceptanceLane, AcceptanceReceipt, AcceptanceSubcase,
    ExpectedReceipt, MAX_BINARY_BYTES, MAX_FIXTURE_BYTES, current_evaluator_sha256_matches,
    empty_runtime, sha256_bytes, sha256_regular_file, validate_expected,
};
use crate::acceptance_environment::IsolatedChildEnvironment;

const READY_ENV: &str = "KIO_TEST_DURABILITY_READY";
const POINT_ENV: &str = "KIO_TEST_DURABILITY_POINT";
const MAX_CHILD_OUTPUT_BYTES: u64 = 256 * 1024;
const MAX_READY_BYTES: u64 = 1024;
const FAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Inputs for the native A10 executor. `contract_binary` is the debug build
/// whose immutable hash is bound separately from the release candidate.
#[derive(Debug, Clone)]
pub struct FaultOptions {
    pub release_binary: PathBuf,
    pub contract_binary: PathBuf,
    pub fixture: PathBuf,
    pub expected: ExpectedReceipt,
    pub work_dir: PathBuf,
    pub receipt: PathBuf,
}

/// A real command and the exact debug-only barrier point at which it is killed.
/// The core durability seam owns the point vocabulary; callers must therefore
/// pass only strings published by that seam.
#[derive(Debug, Clone, Copy)]
pub struct FaultExperiment {
    pub name: &'static str,
    pub point: &'static str,
    pub command: &'static [&'static str],
}

/// Publication is the minimum A10 surface and has no external service or
/// ledger precondition. Other points use the same [`FaultExperiment`] driver
/// after their operation-specific fixture setup has established its real
/// preconditions.
pub const PUBLICATION_EXPERIMENTS: &[FaultExperiment] = &[
    FaultExperiment {
        name: "publication-journal",
        point: "publication_journal",
        command: &[
            "--json",
            "snapshot",
            "create",
            "-m",
            "a10-publication-journal",
        ],
    },
    FaultExperiment {
        name: "publication-manifest",
        point: "publication_manifest",
        command: &[
            "--json",
            "snapshot",
            "create",
            "-m",
            "a10-publication-manifest",
        ],
    },
    FaultExperiment {
        name: "publication-head",
        point: "publication_head",
        command: &["--json", "snapshot", "create", "-m", "a10-publication-head"],
    },
];

/// Executes one A10/native-contract/core run.  The function intentionally
/// emits no receipt until every requested barrier was reached, abruptly killed,
/// and followed by release-candidate recovery and smoke checks.
pub fn run_a10(options: &FaultOptions) -> Result<AcceptanceReceipt, AcceptanceError> {
    let requirement = &options.expected.requirement;
    if requirement.case != AcceptanceCase::A10
        || requirement.lane != AcceptanceLane::NativeContract
        || requirement.subcase != AcceptanceSubcase::Core
    {
        return Err(AcceptanceError::Invalid(
            "fault acceptance requirement is invalid".into(),
        ));
    }
    validate_expected(&options.expected)?;
    current_evaluator_sha256_matches(&options.expected.evaluator_sha256)?;
    verify_inputs(options)?;
    if options.work_dir.exists() || options.receipt.exists() {
        return Err(AcceptanceError::Invalid(
            "fault work directory and receipt must be create-only".into(),
        ));
    }

    fs::create_dir_all(&options.work_dir).map_err(io)?;
    private_dir(&options.work_dir)?;
    let work = options.work_dir.canonicalize().map_err(io)?;
    run_atomic_init_experiments(options, &work)?;
    run_publication_experiments(options, &work)?;
    run_restore_experiments(options, &work)?;
    run_ledger_experiments(options, &work)?;
    run_replica_experiment(options, &work)?;
    run_queue_experiments(options, &work)?;

    verify_inputs(options)?;
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
    crate::acceptance::write_receipt_create_only(&options.receipt, &receipt)?;
    Ok(receipt)
}

// The acceptance fixture is small; metadata is bounded independently from
// provider or product limits so a corrupt journal cannot exhaust the driver.
const MAX_PUBLICATION_BYTES: u64 = 256 * 1024;
type PublicationFiles = BTreeMap<String, Vec<u8>>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationIntent {
    version: u64,
    expected_head: String,
    new_head: String,
    manifest: Value,
}

fn publication_error(message: impl Into<String>) -> AcceptanceError {
    AcceptanceError::Command(message.into())
}

fn publication_head(root: &Path) -> Result<String, AcceptanceError> {
    let bytes = bounded_regular_bytes(&root.join(".kio/HEAD"), 128)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| publication_error("publication HEAD is not UTF-8"))?;
    let head = text.trim_end_matches('\n');
    if !is_hash(head) || text != format!("{head}\n") {
        return Err(publication_error(
            "publication HEAD is not a canonical hash ref",
        ));
    }
    Ok(head.to_owned())
}

fn publication_manifest(root: &Path) -> Result<Value, AcceptanceError> {
    json_value(&bounded_regular_bytes(
        &root.join(".kio/manifest.json"),
        MAX_PUBLICATION_BYTES,
    )?)
}

fn read_publication_intent(
    root: &Path,
    old_head: &str,
) -> Result<PublicationIntent, AcceptanceError> {
    let bytes = bounded_regular_bytes(
        &root.join(".kio/publication-v1.json"),
        MAX_PUBLICATION_BYTES,
    )?;
    let intent: PublicationIntent =
        serde_json::from_slice(&bytes).map_err(|error| AcceptanceError::Json(error.to_string()))?;
    if intent.version != 1
        || intent.expected_head != old_head
        || !is_hash(&intent.expected_head)
        || !is_hash(&intent.new_head)
        || intent.new_head == old_head
    {
        return Err(publication_error(
            "publication journal does not bind a new child of captured HEAD",
        ));
    }
    Ok(intent)
}

fn assert_publication_projection(
    root: &Path,
    head: &str,
    manifest: &Value,
) -> Result<(), AcceptanceError> {
    if publication_head(root)? != head || publication_manifest(root)? != *manifest {
        return Err(publication_error(
            "publication HEAD or manifest differs from the exact expected checkpoint",
        ));
    }
    Ok(())
}

fn assert_publication_manifest(
    manifest: &Value,
    before: &PublicationFiles,
    after: &PublicationFiles,
) -> Result<(), AcceptanceError> {
    let mut rows = BTreeMap::new();
    for (path, bytes) in after {
        let status = match before.get(path) {
            None => "new",
            Some(previous) if previous != bytes => "modified",
            Some(_) => "unchanged",
        };
        rows.insert(
            path.clone(),
            serde_json::json!({
                "path": path, "raw_hash": hash_bytes(bytes), "status": status,
            }),
        );
    }
    for (path, bytes) in before {
        if !after.contains_key(path) {
            rows.insert(
                path.clone(),
                serde_json::json!({
                    "path": path, "raw_hash": hash_bytes(bytes), "status": "deleted",
                }),
            );
        }
    }
    let expected = Value::Array(rows.into_values().collect());
    if manifest.get("schema_version") != Some(&Value::from(1))
        || manifest.get("files") != Some(&expected)
    {
        return Err(publication_error(
            "publication manifest does not preserve exact fixture hashes and add/modify/delete statuses",
        ));
    }
    Ok(())
}

fn assert_publication_bytes(actual: &[u8], expected: &[u8]) -> Result<(), AcceptanceError> {
    if actual != expected {
        return Err(publication_error(
            "publication raw or working bytes differ from independent fixture",
        ));
    }
    Ok(())
}

fn assert_recovered_publication(
    root: &Path,
    intent: &PublicationIntent,
    message: &str,
    expected: &PublicationFiles,
) -> Result<(), AcceptanceError> {
    assert_publication_projection(root, &intent.new_head, &intent.manifest)?;
    match fs::symlink_metadata(root.join(".kio/publication-v1.json")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(error)),
        Ok(_) => return Err(publication_error("recovery retained a publication journal")),
    }
    // This open refuses pending recovery; it cannot certify the debug child's
    // effects by replaying them inside the evaluator.
    let repo =
        Repository::open_read_only(root).map_err(|error| publication_error(error.to_string()))?;
    let commit = repo
        .read_commit(&intent.new_head)
        .map_err(|error| publication_error(error.to_string()))?;
    if commit.parent.as_deref() != Some(intent.expected_head.as_str())
        || commit.commit_type != CommitType::Manual
        || commit.object_type != "commit"
        || commit.message != message
        || commit.restore_provenance.is_some()
        || !commit.purged_raws.is_empty()
    {
        return Err(publication_error(
            "recovered publication is not the exact manual snapshot child",
        ));
    }
    let tree = repo
        .read_tree(&commit.tree)
        .map_err(|error| publication_error(error.to_string()))?;
    let expected_hashes: BTreeMap<_, _> = expected
        .iter()
        .map(|(path, bytes)| (path.clone(), hash_bytes(bytes)))
        .collect();
    let actual_hashes: BTreeMap<_, _> = tree
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.raw_hash.clone()))
        .collect();
    if tree.entries.len() != expected.len() || actual_hashes != expected_hashes {
        return Err(publication_error(
            "recovered full tree differs from independent fixture hash map",
        ));
    }
    let store = repo.object_store();
    for (path, bytes) in expected {
        let raw = store
            .read_object(ObjectKind::Raw, &hash_bytes(bytes))
            .map_err(|error| publication_error(error.to_string()))?;
        assert_publication_bytes(&raw.bytes, bytes)?;
        assert_publication_bytes(
            &bounded_regular_bytes(&root.join(path), MAX_FIXTURE_BYTES)?,
            bytes,
        )?;
    }
    let mut actual_paths = Vec::new();
    for entry in fs::read_dir(root).map_err(io)? {
        let entry = entry.map_err(io)?;
        if entry.file_name() == ".kio" {
            continue;
        }
        if !entry.file_type().map_err(io)?.is_file() {
            return Err(publication_error(
                "publication working tree contains an unexpected non-file",
            ));
        }
        actual_paths.push(
            entry
                .file_name()
                .into_string()
                .map_err(|_| publication_error("publication working path is not UTF-8"))?,
        );
    }
    actual_paths.sort();
    if actual_paths != expected.keys().cloned().collect::<Vec<_>>() {
        return Err(publication_error(
            "publication working tree includes missing or unexpected paths",
        ));
    }
    Ok(())
}

fn assert_publication_repair(bytes: &[u8], repaired: bool) -> Result<(), AcceptanceError> {
    let value = json_value(bytes)?;
    let repairs = value
        .get("repaired")
        .and_then(Value::as_array)
        .ok_or_else(|| publication_error("release init lacks a repair report"))?;
    if (repaired
        && !repairs
            .iter()
            .any(|value| value.as_str() == Some("publication")))
        || (!repaired && !repairs.is_empty())
    {
        return Err(publication_error(
            "release init publication repair report differs",
        ));
    }
    Ok(())
}

fn run_publication_experiments(options: &FaultOptions, work: &Path) -> Result<(), AcceptanceError> {
    for experiment in PUBLICATION_EXPERIMENTS {
        let device = Device::create_named(work, experiment.name)?;
        let before = BTreeMap::from([
            (
                "fixture.bin".into(),
                bounded_regular_bytes(&options.fixture, MAX_FIXTURE_BYTES)?,
            ),
            ("modify.md".into(), b"publication original bytes\n".to_vec()),
            ("delete.md".into(), b"publication deleted bytes\n".to_vec()),
        ]);
        for (path, bytes) in &before {
            fs::write(device.root.join(path), bytes).map_err(io)?;
        }
        run_release(options, &device, &["--json", "init"])?;
        run_release(
            options,
            &device,
            &["--json", "snapshot", "create", "-m", "a10-baseline"],
        )?;
        let old_head = publication_head(&device.root)?;
        let old_manifest = publication_manifest(&device.root)?;
        let mut expected = before.clone();
        expected.insert("modify.md".into(), b"publication changed bytes\n".to_vec());
        expected.insert(
            "added.md".into(),
            format!("{} added bytes\n", experiment.name).into_bytes(),
        );
        expected.remove("delete.md");
        for (path, bytes) in &expected {
            fs::write(device.root.join(path), bytes).map_err(io)?;
        }
        fs::remove_file(device.root.join("delete.md")).map_err(io)?;
        verify_inputs(options)?;
        fault_once(options, &device, experiment)?;
        let intent = read_publication_intent(&device.root, &old_head)?;
        assert_publication_manifest(&intent.manifest, &before, &expected)?;
        let (checkpoint_head, checkpoint_manifest) = match experiment.point {
            "publication_journal" => (&old_head, &old_manifest),
            "publication_head" => (&intent.new_head, &old_manifest),
            "publication_manifest" => (&intent.new_head, &intent.manifest),
            _ => return Err(publication_error("unsupported publication checkpoint")),
        };
        assert_publication_projection(&device.root, checkpoint_head, checkpoint_manifest)?;
        // All three barriers roll forward; only the release CLI performs that
        // mutation. status/log are pure reads and cannot stand in for init.
        assert_publication_repair(&run_release(options, &device, &["--json", "init"])?, true)?;
        let message = experiment
            .command
            .last()
            .copied()
            .ok_or_else(|| publication_error("publication experiment lacks message"))?;
        assert_recovered_publication(&device.root, &intent, message, &expected)?;
        let manifest_bytes = bounded_regular_bytes(
            &device.root.join(".kio/manifest.json"),
            MAX_PUBLICATION_BYTES,
        )?;
        assert_publication_repair(&run_release(options, &device, &["--json", "init"])?, false)?;
        assert_recovered_publication(&device.root, &intent, message, &expected)?;
        assert_publication_bytes(
            &bounded_regular_bytes(
                &device.root.join(".kio/manifest.json"),
                MAX_PUBLICATION_BYTES,
            )?,
            &manifest_bytes,
        )?;
        run_release(options, &device, &["--json", "status"])?;
        run_release(options, &device, &["--json", "log"])?;
        // A snapshot of the unchanged tree must not create another commit.
        run_release(
            options,
            &device,
            &["--json", "snapshot", "create", "-m", "a10-recovery"],
        )?;
        assert_recovered_publication(&device.root, &intent, message, &expected)?;
        verify_inputs(options)?;
    }
    Ok(())
}

fn verify_inputs(options: &FaultOptions) -> Result<(), AcceptanceError> {
    if sha256_regular_file(&options.release_binary, MAX_BINARY_BYTES)?
        != options.expected.candidate.binary_sha256
    {
        return Err(AcceptanceError::Invalid(
            "release binary hash differs from candidate binding".into(),
        ));
    }
    let expected_contract = options
        .expected
        .contract_binary_sha256
        .as_deref()
        .ok_or_else(|| {
            AcceptanceError::Invalid(
                "A10 expected receipt lacks a contract-binary hash binding".into(),
            )
        })?;
    if sha256_regular_file(&options.contract_binary, MAX_BINARY_BYTES)? != expected_contract {
        return Err(AcceptanceError::Invalid(
            "contract binary hash differs from expected receipt binding".into(),
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

fn fault_once(
    options: &FaultOptions,
    device: &Device,
    experiment: &FaultExperiment,
) -> Result<(), AcceptanceError> {
    fault_command(
        options,
        device,
        experiment.name,
        experiment.point,
        experiment.command,
    )
}

fn run_atomic_init_experiments(options: &FaultOptions, work: &Path) -> Result<(), AcceptanceError> {
    for (name, point) in [
        ("atomic-write-staged", "atomic_write_staged"),
        ("atomic-write-published", "atomic_write_published"),
        ("atomic-remove-ready", "atomic_remove_ready"),
        ("atomic-remove-quarantined", "atomic_remove_quarantined"),
        ("atomic-remove-deleted", "atomic_remove_deleted"),
    ] {
        let device = Device::create_named(work, name)?;
        verify_inputs(options)?;
        fault_command(options, &device, name, point, &["--json", "init"])?;
        let planned_scope_id = if point == "atomic_write_staged" {
            None
        } else {
            Some(read_planned_scope_id(&device.root)?)
        };
        // Recovery is release-only: a durable atomic residue must either be
        // resolved or fail loudly, never be silently discarded by the driver.
        run_release(options, &device, &["--json", "init"])?;
        run_release(options, &device, &["--json", "status"])?;
        run_release(options, &device, &["--json", "log"])?;
        if let Some(planned_scope_id) = planned_scope_id {
            assert_recovered_scope_identity(&device.root, &planned_scope_id)?;
        }
        let root_entries = fs::read_dir(&device.root)
            .map_err(io)?
            .map(|entry| entry.map_err(io))
            .collect::<Result<Vec<_>, _>>()?;
        if root_entries.len() != 1 || root_entries[0].file_name() != ".kio" {
            return Err(AcceptanceError::Command(format!(
                "{point} recovery left atomic/bootstrap artifacts outside .kio"
            )));
        }
        run_release(
            options,
            &device,
            &["--json", "snapshot", "create", "-m", "a10-atomic-recovery"],
        )?;
        verify_inputs(options)?;
    }
    Ok(())
}

fn read_planned_scope_id(root: &Path) -> Result<String, AcceptanceError> {
    let journal = root.join(".kio/.root-init-journal.json");
    let bytes = bounded_regular_bytes(&journal, 64 * 1024)?;
    let value = json_value(&bytes)?;
    let scope_id = value
        .get("planned_scope_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| is_ulid(id))
        .ok_or_else(|| {
            AcceptanceError::Command("bootstrap journal lacks valid planned_scope_id".into())
        })?;
    Ok(scope_id.to_owned())
}

fn assert_recovered_scope_identity(root: &Path, planned: &str) -> Result<(), AcceptanceError> {
    for leaf in [".kio/scope.json", ".kio/management.json"] {
        let bytes = bounded_regular_bytes(&root.join(leaf), 64 * 1024)?;
        let value = json_value(&bytes)?;
        let actual = value.get("scope_id").and_then(serde_json::Value::as_str);
        if actual != Some(planned) {
            return Err(AcceptanceError::Command(format!(
                "bootstrap recovery did not retain planned scope identity in {leaf}"
            )));
        }
    }
    Ok(())
}

fn is_ulid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 26
        && bytes[0].is_ascii_digit()
        && bytes[0] <= b'7'
        && bytes.iter().skip(1).all(|byte| matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'))
}

fn fault_command(
    options: &FaultOptions,
    device: &Device,
    name: &str,
    point: &str,
    args: &[&str],
) -> Result<(), AcceptanceError> {
    let ready = device.tmp.join(format!("{name}.ready"));
    let stdout = device.tmp.join(format!("{name}.stdout"));
    let stderr = device.tmp.join(format!("{name}.stderr"));
    let mut command = base(&options.contract_binary, device)?;
    command
        .args(args)
        .env(POINT_ENV, point)
        .env(READY_ENV, &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::from(File::create(&stdout).map_err(io)?))
        .stderr(Stdio::from(File::create(&stderr).map_err(io)?));
    if is_ledger_write_fault(point) {
        command
            .env("KIO_TEST_MISTRAL_OCR", "mock")
            .env("MISTRAL_API_KEY", "a10-test-key");
    }
    let mut child = AbruptChild::new(command.spawn().map_err(io)?);
    wait_for_ready(&mut child, &ready, point, &stdout, &stderr)?;
    child.kill_and_wait()?;
    check_output_budget(&stdout)?;
    check_output_budget(&stderr)?;
    Ok(())
}

fn run_contract_mock(
    options: &FaultOptions,
    device: &Device,
    args: &[&str],
) -> Result<Vec<u8>, AcceptanceError> {
    let output = base(&options.contract_binary, device)?
        .env("KIO_TEST_MISTRAL_OCR", "mock")
        .env("MISTRAL_API_KEY", "a10-test-key")
        .args(args)
        .output()
        .map_err(io)?;
    if !output.status.success() {
        return Err(AcceptanceError::Command(format!(
            "contract mock setup {} failed with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn run_restore_experiments(options: &FaultOptions, work: &Path) -> Result<(), AcceptanceError> {
    for (name, point) in [
        ("restore-journal", "restore_journal"),
        ("restore-working-bytes", "restore_working_bytes"),
        ("restore-head", "restore_head"),
    ] {
        let device = Device::create_named(work, name)?;
        run_release(options, &device, &["--json", "init"])?;
        fs::write(device.root.join("a.md"), b"old unique orchid phrase\n").map_err(io)?;
        let source = commit_hash(&run_release(
            options,
            &device,
            &["--json", "index", "--offline"],
        )?)?;
        fs::write(device.root.join("a.md"), b"new unique tulip phrase\n").map_err(io)?;
        let head = commit_hash(&run_release(
            options,
            &device,
            &["--json", "index", "--offline"],
        )?)?;
        fault_command(
            options,
            &device,
            name,
            point,
            &["--json", "restore", &source, "--expected-head", &head],
        )?;
        run_release(options, &device, &["--json", "repair", "recover-restore"])?;
        let resolved_head = fs::read_to_string(device.root.join(".kio/HEAD")).map_err(io)?;
        let resolved_head = resolved_head.trim();
        let published = point == "restore_head";
        let expected_bytes = if published {
            b"old unique orchid phrase\n".as_slice()
        } else {
            b"new unique tulip phrase\n".as_slice()
        };
        if resolved_head.is_empty()
            || fs::read(device.root.join("a.md")).map_err(io)? != expected_bytes
        {
            return Err(AcceptanceError::Command(format!(
                "{point} recovery did not restore exact working bytes and HEAD"
            )));
        }
        if !published && resolved_head != head {
            return Err(AcceptanceError::Command(format!(
                "{point} recovery did not retain the captured current HEAD"
            )));
        }
        if published {
            let commit = json_value(&run_release(
                options,
                &device,
                &["--json", "inspect", resolved_head],
            )?)?;
            if commit.get("parent").and_then(serde_json::Value::as_str) != Some(head.as_str()) {
                return Err(AcceptanceError::Command(format!(
                    "{point} recovery did not retain a linear child of the captured HEAD"
                )));
            }
        }
        let query = if published { "orchid" } else { "tulip" };
        let search = json_value(&run_release(
            options,
            &device,
            &["--json", "search", query, "--mode", "text"],
        )?)?;
        if search
            .get("results")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|results| results.is_empty())
        {
            return Err(AcceptanceError::Command(format!(
                "{point} recovery text search did not return the resolved content"
            )));
        }
        run_release(options, &device, &["--json", "repair", "all"])?;
        run_release(
            options,
            &device,
            &["--json", "search", query, "--mode", "text"],
        )?;
        verify_inputs(options)?;
    }
    Ok(())
}

fn commit_hash(bytes: &[u8]) -> Result<String, AcceptanceError> {
    json_value(bytes)?
        .get("commit_hash")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| AcceptanceError::Command("index response lacks commit_hash".into()))
}

fn run_ledger_experiments(options: &FaultOptions, work: &Path) -> Result<(), AcceptanceError> {
    for (name, point) in [
        ("ledger-before-commit", "ledger_checkpoint"),
        ("ledger-after-commit", "ledger_write_committed"),
        ("ledger-backup-proof", "ledger_backup_proof"),
        ("ledger-restore-journal", "ledger_restore_journal"),
        ("ledger-restore-database", "ledger_restore_database"),
    ] {
        let device = Device::create_named(work, name)?;
        run_release(options, &device, &["--json", "init"])?;
        run_release(options, &device, &["--json", "ledger", "init"])?;
        let backup = device.home.join("ledger-backup");
        fs::create_dir(&backup).map_err(io)?;
        private_dir(&backup)?;
        if is_ledger_write_fault(point) {
            fs::write(
                device.root.join("document.pdf"),
                b"%PDF-1.4\nA10 ledger fixture\n",
            )
            .map_err(io)?;
            run_contract_mock(
                options,
                &device,
                &["--json", "adapter", "approve", "--all", "--yes"],
            )?;
            run_contract_mock(options, &device, &["--json", "index", "--yes"])?;
        }
        match point {
            "ledger_checkpoint" | "ledger_write_committed" => fault_command(
                options,
                &device,
                name,
                point,
                &["--json", "batch", "resume"],
            )?,
            "ledger_backup_proof" => fault_command(
                options,
                &device,
                name,
                point,
                &[
                    "--json",
                    "ledger",
                    "backup",
                    "--to",
                    backup.to_str().ok_or_else(|| {
                        AcceptanceError::Invalid("ledger backup path is not UTF-8".into())
                    })?,
                ],
            )?,
            "ledger_restore_journal" | "ledger_restore_database" => {
                run_release(
                    options,
                    &device,
                    &[
                        "--json",
                        "ledger",
                        "backup",
                        "--to",
                        backup.to_str().ok_or_else(|| {
                            AcceptanceError::Invalid("ledger backup path is not UTF-8".into())
                        })?,
                    ],
                )?;
                fs::remove_file(device.home.join("data/kio/cost-ledger.sqlite")).map_err(io)?;
                fault_command(
                    options,
                    &device,
                    name,
                    point,
                    &[
                        "--json",
                        "ledger",
                        "restore",
                        "--from",
                        backup.to_str().ok_or_else(|| {
                            AcceptanceError::Invalid("ledger backup path is not UTF-8".into())
                        })?,
                    ],
                )?;
            }
            _ => unreachable!("fixed ledger durability point"),
        }
        // The release binary resolves an interrupted initialization/restore
        // through its public lifecycle commands, then proves the DB is usable.
        if point.starts_with("ledger_restore") {
            run_release(
                options,
                &device,
                &[
                    "--json",
                    "ledger",
                    "restore",
                    "--from",
                    backup.to_str().ok_or_else(|| {
                        AcceptanceError::Invalid("ledger backup path is not UTF-8".into())
                    })?,
                ],
            )?;
        }
        if is_ledger_write_fault(point) {
            recover_ledger_write_crash(options, &device, point)?;
        } else {
            run_release(options, &device, &["--json", "ledger", "status"])?;
        }
        if !device.home.join("data/kio/cost-ledger.sqlite").is_file() {
            return Err(AcceptanceError::Command(format!(
                "{point} recovery did not retain a ledger database"
            )));
        }
        verify_inputs(options)?;
    }
    Ok(())
}

fn is_ledger_write_fault(point: &str) -> bool {
    matches!(point, "ledger_checkpoint" | "ledger_write_committed")
}

fn recover_ledger_write_crash(
    options: &FaultOptions,
    device: &Device,
    point: &str,
) -> Result<(), AcceptanceError> {
    let before_recovery = ledger_observation(device)?;
    let pending_path = device
        .home
        .join("data/kio/cost-ledger.sqlite.write.pending.json");
    let pending_before_status = fs::read(&pending_path).map_err(io)?;
    let output = base(&options.release_binary, device)?
        .args(["--json", "ledger", "status"])
        .output()
        .map_err(io)?;
    if output.status.code() != Some(4) {
        return Err(AcceptanceError::Command(format!(
            "{point} status returned {} instead of exit 4: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let failure: Value = serde_json::from_slice(&output.stderr).map_err(|error| {
        AcceptanceError::Json(format!("{point} status returned invalid JSON: {error}"))
    })?;
    if failure["error_code"] != "KIO-E-LEDGER-SNAPSHOT-UNSAFE-001" {
        return Err(AcceptanceError::Command(format!(
            "{point} status did not report the expected fail-closed integrity error"
        )));
    }
    if ledger_observation(device)? != before_recovery
        || fs::read(&pending_path).map_err(io)? != pending_before_status
    {
        return Err(AcceptanceError::Command(format!(
            "{point} status silently changed a pending ledger write"
        )));
    }
    let expected_recovery = if point == "ledger_checkpoint" {
        "restored_before_checkpoint"
    } else {
        "finalized_committed"
    };
    let recovery = json_value(&run_release(
        options,
        device,
        &["--json", "ledger", "recover"],
    )?)?;
    if recovery["status"] != "ready" || recovery["recovery"] != expected_recovery {
        return Err(AcceptanceError::Command(format!(
            "{point} ledger recover did not report {expected_recovery}"
        )));
    }
    let after_recovery = ledger_observation(device)?;
    if point == "ledger_checkpoint" {
        if after_recovery.database_sequence != 0
            || after_recovery.checkpoint_sequence != 0
            || after_recovery.requests != 0
            || after_recovery.costs != 0
        {
            return Err(AcceptanceError::Command(
                "before-commit recovery did not restore the exact pre-write SQL state".into(),
            ));
        }
    } else if after_recovery != before_recovery
        || after_recovery.database_sequence != after_recovery.checkpoint_sequence
        || after_recovery.requests != 1
        || after_recovery.costs != 0
        || after_recovery.pending_reservations != 1
    {
        return Err(AcceptanceError::Command(
            "after-commit recovery did not preserve the exact unknown reservation".into(),
        ));
    }
    let repeated = json_value(&run_release(
        options,
        device,
        &["--json", "ledger", "recover"],
    )?)?;
    if repeated["status"] != "ready" || repeated["recovery"] != "no_pending" {
        return Err(AcceptanceError::Command(format!(
            "{point} repeated ledger recover was not idempotent"
        )));
    }
    run_release(options, device, &["--json", "ledger", "status"])?;
    if ledger_observation(device)? != after_recovery {
        return Err(AcceptanceError::Command(format!(
            "{point} recovery or ready status resent or rewrote the reservation"
        )));
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
struct LedgerObservation {
    checkpoint_sequence: i64,
    database_sequence: i64,
    requests: i64,
    costs: i64,
    pending_reservations: i64,
}

fn ledger_observation(device: &Device) -> Result<LedgerObservation, AcceptanceError> {
    let checkpoint: Value = serde_json::from_slice(
        &fs::read(
            device
                .home
                .join("data/kio/cost-ledger.sqlite.checkpoint.json"),
        )
        .map_err(io)?,
    )
    .map_err(|error| AcceptanceError::Json(format!("decode ledger checkpoint: {error}")))?;
    let checkpoint_sequence = checkpoint["seq"].as_i64().ok_or_else(|| {
        AcceptanceError::Command("ledger checkpoint lacks an integer sequence".into())
    })?;
    let connection = Connection::open_with_flags(
        device.home.join("data/kio/cost-ledger.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|error| {
        AcceptanceError::Command(format!("open checkpoint ledger read-only: {error}"))
    })?;
    let database_sequence = connection
        .query_row(
            "SELECT sequence FROM ledger_metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .map_err(|error| {
            AcceptanceError::Command(format!("read checkpoint ledger sequence: {error}"))
        })?;
    let requests = connection
        .query_row("SELECT COUNT(*) FROM batch_requests", [], |row| row.get(0))
        .map_err(|error| AcceptanceError::Command(format!("read ledger requests: {error}")))?;
    let costs = connection
        .query_row("SELECT COUNT(*) FROM cost_ledger", [], |row| row.get(0))
        .map_err(|error| AcceptanceError::Command(format!("read ledger costs: {error}")))?;
    let pending_reservations = connection
        .query_row(
            "SELECT COUNT(*) FROM batch_requests WHERE state=0 AND intent_token IS NOT NULL AND batch_job_id IS NULL",
            [],
            |row| row.get(0),
        )
        .map_err(|error| {
            AcceptanceError::Command(format!("read pending ledger reservations: {error}"))
        })?;
    Ok(LedgerObservation {
        checkpoint_sequence,
        database_sequence,
        requests,
        costs,
        pending_reservations,
    })
}

fn run_replica_experiment(options: &FaultOptions, work: &Path) -> Result<(), AcceptanceError> {
    let device = Device::create_named(work, "replica-published")?;
    run_release(options, &device, &["--json", "init"])?;
    fs::write(
        device.root.join("replica.md"),
        b"replica durable orchid phrase\n",
    )
    .map_err(io)?;
    run_release(options, &device, &["--json", "index", "--offline"])?;
    fs::write(
        device.root.join("replica.md"),
        b"replica durable tulip phrase\n",
    )
    .map_err(io)?;
    fault_command(
        options,
        &device,
        "replica-published",
        "replica_published",
        // `repair rebuild-db` deliberately rebuilds from the current committed
        // HEAD, so it cannot publish the unindexed tulip working bytes. Index
        // is the writer that commits those bytes, replaces the source index,
        // and then reaches the replica publication barrier.
        &["--json", "index", "--offline"],
    )?;
    run_release(options, &device, &["--json", "repair", "replica"])?;
    let search = json_value(&run_release(
        options,
        &device,
        &["--json", "search", "tulip", "--mode", "text"],
    )?)?;
    if search
        .get("results")
        .and_then(serde_json::Value::as_array)
        .is_none_or(|rows| rows.is_empty())
    {
        return Err(AcceptanceError::Command(
            "replica recovery did not serve the changed source bytes".into(),
        ));
    }
    verify_inputs(options)
}

fn run_queue_experiments(options: &FaultOptions, work: &Path) -> Result<(), AcceptanceError> {
    for (name, point) in [
        ("queue-claimed", "queue_claimed"),
        ("queue-completed", "queue_completed"),
    ] {
        let device = Device::create_named(work, name)?;
        run_release(options, &device, &["--json", "init"])?;
        fs::write(device.root.join("queue.md"), b"queue before bytes\n").map_err(io)?;
        run_release(options, &device, &["--json", "index", "--offline"])?;
        fault_watch_queue(options, &device, name, point)?;
        // A normal watcher must reclaim any pending durable row and converge.
        let mut release = base(&options.release_binary, &device)?;
        release
            .args([
                "--json",
                "watch",
                "run",
                "--reconcile-interval-seconds",
                "1",
            ])
            .stdin(Stdio::null());
        let mut child = AbruptChild::new(release.spawn().map_err(io)?);
        let deadline = Instant::now() + FAULT_TIMEOUT;
        loop {
            let status = json_value(&run_release(
                options,
                &device,
                &["--json", "watch", "status"],
            )?)?;
            // `watch status` wraps the live engine state under
            // `last_observation`; a top-level backlog has never been part of
            // this CLI contract. Require a successful observation too, so an
            // initial empty queue cannot satisfy the crash-recovery check.
            if status
                .pointer("/status")
                .and_then(serde_json::Value::as_str)
                == Some("running")
                && status
                    .pointer("/last_observation/backlog")
                    .and_then(serde_json::Value::as_u64)
                    == Some(0)
                && status
                    .pointer("/last_observation/degraded")
                    .and_then(serde_json::Value::as_bool)
                    == Some(false)
                && status
                    .pointer("/last_observation/last_success_ms")
                    .and_then(serde_json::Value::as_u64)
                    .is_some()
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err(AcceptanceError::Command(format!(
                    "{point} release watch did not converge"
                )));
            }
            thread::sleep(Duration::from_millis(50));
        }
        child.kill_and_wait()?;
        let search = json_value(&run_release(
            options,
            &device,
            &["--json", "search", "after", "--mode", "text"],
        )?)?;
        if search
            .get("results")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|rows| rows.is_empty())
        {
            return Err(AcceptanceError::Command(format!(
                "{point} release watch did not index changed bytes"
            )));
        }
    }
    Ok(())
}

fn fault_watch_queue(
    options: &FaultOptions,
    device: &Device,
    name: &str,
    point: &str,
) -> Result<(), AcceptanceError> {
    let ready = device.tmp.join(format!("{name}.ready"));
    let stdout = device.tmp.join(format!("{name}.stdout"));
    let stderr = device.tmp.join(format!("{name}.stderr"));
    let mut command = base(&options.contract_binary, device)?;
    command
        .args([
            "--json",
            "watch",
            "run",
            "--reconcile-interval-seconds",
            "1",
        ])
        .env(POINT_ENV, point)
        .env(READY_ENV, &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::from(File::create(&stdout).map_err(io)?))
        .stderr(Stdio::from(File::create(&stderr).map_err(io)?));
    let mut child = AbruptChild::new(command.spawn().map_err(io)?);
    wait_for_ready(&mut child, &ready, point, &stdout, &stderr)?;
    fs::write(device.root.join("queue.md"), b"queue after bytes\n").map_err(io)?;
    child.kill_and_wait()
}

fn json_value(bytes: &[u8]) -> Result<serde_json::Value, AcceptanceError> {
    serde_json::from_slice(bytes).map_err(|error| AcceptanceError::Json(error.to_string()))
}

fn run_release(
    options: &FaultOptions,
    device: &Device,
    args: &[&str],
) -> Result<Vec<u8>, AcceptanceError> {
    let output = base(&options.release_binary, device)?
        .args(args)
        .output()
        .map_err(io)?;
    if !output.status.success() {
        return Err(AcceptanceError::Command(format!(
            "release recovery command {} failed with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

struct Device {
    root: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
}

impl Device {
    fn create_named(work: &Path, name: &str) -> Result<Self, AcceptanceError> {
        let base = work.join(name);
        let home = base.join("owner-private-home");
        let root = base.join("scope");
        let tmp = home.join("tmp");
        for path in [
            &home,
            &root,
            &tmp,
            &home.join("config"),
            &home.join("data"),
            &home.join("cache"),
        ] {
            fs::create_dir_all(path).map_err(io)?;
            private_dir(path)?;
        }
        Ok(Self {
            root: root.canonicalize().map_err(io)?,
            home: home.canonicalize().map_err(io)?,
            tmp: tmp.canonicalize().map_err(io)?,
        })
    }
}

fn base(binary: &Path, device: &Device) -> Result<Command, AcceptanceError> {
    let mut command = Command::new(binary);
    IsolatedChildEnvironment::new(
        &device.home,
        device.home.join("config"),
        device.home.join("data"),
        device.home.join("cache"),
        &device.tmp,
    )
    .apply(&mut command)?;
    command.current_dir(&device.root);
    Ok(command)
}

struct AbruptChild(Option<Child>);

impl AbruptChild {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn child(&mut self) -> Result<&mut Child, AcceptanceError> {
        self.0
            .as_mut()
            .ok_or_else(|| AcceptanceError::Command("fault child was already reaped".into()))
    }

    fn kill_and_wait(&mut self) -> Result<(), AcceptanceError> {
        let mut child = self
            .0
            .take()
            .ok_or_else(|| AcceptanceError::Command("fault child was already reaped".into()))?;
        let _ = child.kill();
        child.wait().map_err(io)?;
        Ok(())
    }
}

impl Drop for AbruptChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_for_ready(
    child: &mut AbruptChild,
    ready: &Path,
    point: &str,
    stdout: &Path,
    stderr: &Path,
) -> Result<(), AcceptanceError> {
    let deadline = Instant::now() + FAULT_TIMEOUT;
    loop {
        check_output_budget(stdout)?;
        check_output_budget(stderr)?;
        if ready.exists() {
            let value = bounded_regular_bytes(ready, MAX_READY_BYTES)?;
            let expected = format!("point={point}\npid={}\n", child.child()?.id());
            if value == expected.as_bytes() {
                return Ok(());
            }
            return Err(AcceptanceError::Command(format!(
                "durability barrier readiness does not bind exact point and child pid for {point}"
            )));
        }
        if let Some(status) = child.child()?.try_wait().map_err(io)? {
            return Err(AcceptanceError::Command(format!(
                "contract child exited before durability barrier {point}: {status}"
            )));
        }
        if Instant::now() >= deadline {
            return Err(AcceptanceError::Command(format!(
                "timed out waiting for durability barrier {point}"
            )));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn check_output_budget(path: &Path) -> Result<(), AcceptanceError> {
    if fs::metadata(path).map_err(io)?.len() > MAX_CHILD_OUTPUT_BYTES {
        return Err(AcceptanceError::Command(
            "fault child exceeded bounded output budget".into(),
        ));
    }
    Ok(())
}

fn bounded_regular_bytes(path: &Path, maximum: u64) -> Result<Vec<u8>, AcceptanceError> {
    let metadata = fs::symlink_metadata(path).map_err(io)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > maximum {
        return Err(AcceptanceError::Invalid(
            "fault acceptance input must be a bounded regular file".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .map_err(io)?
        .read_to_end(&mut bytes)
        .map_err(io)?;
    Ok(bytes)
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

#[cfg(test)]
mod publication_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn publication_intent_rejects_wrong_authority_and_unknown_fields() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".kio")).unwrap();
        let old_head = hash_bytes(b"old commit");
        let new_head = hash_bytes(b"new commit");
        let journal = json!({
            "version": 1,
            "expected_head": old_head,
            "new_head": new_head,
            "manifest": {"files": []},
        });
        let path = temp.path().join(".kio/publication-v1.json");
        fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
        assert!(read_publication_intent(temp.path(), &old_head).is_ok());
        for (field, value) in [
            ("version", json!(2)),
            ("expected_head", json!(hash_bytes(b"unrelated parent"))),
            ("expected_head", Value::Null),
            ("new_head", json!(old_head)),
            ("new_head", json!("invalid")),
            ("unexpected", json!(true)),
        ] {
            let mut invalid = journal.clone();
            invalid[field] = value;
            fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(
                read_publication_intent(temp.path(), &old_head).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn publication_projection_rejects_wrong_head_or_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let kio = temp.path().join(".kio");
        fs::create_dir(&kio).unwrap();
        let old_head = hash_bytes(b"old");
        let new_head = hash_bytes(b"new");
        let old_manifest = json!({"files": [], "schema_version": 1});
        let new_manifest = json!({"files": [{"path": "added.md"}], "schema_version": 1});
        // Exercise all three checkpoints with their required old/new states.
        for (head, manifest) in [
            (&old_head, &old_manifest),
            (&new_head, &old_manifest),
            (&new_head, &new_manifest),
        ] {
            fs::write(kio.join("HEAD"), format!("{head}\n")).unwrap();
            fs::write(
                kio.join("manifest.json"),
                serde_json::to_vec(manifest).unwrap(),
            )
            .unwrap();
            assert!(assert_publication_projection(temp.path(), head, manifest).is_ok());
            let wrong_head = if head == &old_head {
                &new_head
            } else {
                &old_head
            };
            assert!(assert_publication_projection(temp.path(), wrong_head, manifest).is_err());
            let wrong_manifest = if manifest == &old_manifest {
                &new_manifest
            } else {
                &old_manifest
            };
            assert!(assert_publication_projection(temp.path(), head, wrong_manifest).is_err());
        }
    }

    #[test]
    fn publication_fixture_assertions_reject_wrong_hash_deleted_row_and_bytes() {
        let before = PublicationFiles::from([
            ("deleted.md".into(), b"deleted".to_vec()),
            ("modified.md".into(), b"before".to_vec()),
            ("unchanged.md".into(), b"unchanged".to_vec()),
        ]);
        let after = PublicationFiles::from([
            ("added.md".into(), b"added".to_vec()),
            ("modified.md".into(), b"after".to_vec()),
            ("unchanged.md".into(), b"unchanged".to_vec()),
        ]);
        let manifest = json!({"schema_version": 1, "files": [
            {"path": "added.md", "raw_hash": hash_bytes(b"added"), "status": "new"},
            {"path": "deleted.md", "raw_hash": hash_bytes(b"deleted"), "status": "deleted"},
            {"path": "modified.md", "raw_hash": hash_bytes(b"after"), "status": "modified"},
            {"path": "unchanged.md", "raw_hash": hash_bytes(b"unchanged"), "status": "unchanged"},
        ]});
        assert!(assert_publication_manifest(&manifest, &before, &after).is_ok());
        for index in 0..4 {
            let mut wrong = manifest.clone();
            wrong["files"][index]["raw_hash"] = json!(hash_bytes(b"wrong"));
            assert!(assert_publication_manifest(&wrong, &before, &after).is_err());
        }
        let mut missing_deleted = manifest.clone();
        missing_deleted["files"].as_array_mut().unwrap().remove(1);
        assert!(assert_publication_manifest(&missing_deleted, &before, &after).is_err());
        assert!(assert_publication_bytes(b"right", b"right").is_ok());
        assert!(assert_publication_bytes(b"wrong", b"right").is_err());
        assert!(assert_publication_repair(br#"{"repaired":["publication"]}"#, true).is_ok());
        assert!(assert_publication_repair(br#"{"repaired":[]}"#, true).is_err());
        assert!(assert_publication_repair(br#"{"repaired":[]}"#, false).is_ok());
        assert!(assert_publication_repair(br#"{"repaired":["publication"]}"#, false).is_err());
    }
}
