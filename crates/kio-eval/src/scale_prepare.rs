//! Descriptor-bound preparation for a generated scale v3 corpus.
//!
//! This deliberately does not infer a fixture shape from the filesystem.  The
//! generator-bound manifest is the authority and every child is executed from
//! a retained scope descriptor with a sealed copy of the supplied binary.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use cap_primitives::fs as cap_fs;
#[cfg(windows)]
use cap_primitives::fs::_WindowsByHandle;
#[cfg(unix)]
use cap_primitives::fs::MetadataExt;
use kio_core::cas::canonical_json_bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    boundary::sync_retained_directory,
    process_boundary::{
        DescriptorExecutable, ProcessBoundaryError, configure_descriptor_environment,
        configure_retained_cwd,
    },
    runner::{BoundedProcessOptions, run_bounded_command},
    scale_fixture::{ScaleFixtureError, bind_ready, rename_noreplace},
    scale_spec::{self, DEVICE_DIR_NAME, PREPARE_REPORT_NAME, PREPARER_ID},
};

const DEVICE_SUBDIRS: [&str; 6] = ["home", "config", "cache", "data", "state", "runtime"];
const MAX_JSON_BYTES: usize = 1024 * 1024;
const PREPARE_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const OFFLINE_INDEX_ARGS: [&str; 4] = ["--json", "index", "--offline", "--yes"];
const PREPARE_REPORT_TEMP_NAME: &str = ".kio-scale-prepare-v3.tmp";

#[derive(Clone)]
enum IndexExpectation {
    Repair {
        files: usize,
        embeddings: usize,
    },
    Overlay {
        files: usize,
        embeddings: usize,
        parent: String,
    },
    RegistryNoop {
        files: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirectoryIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    volume: u32,
    #[cfg(windows)]
    index: u64,
}

#[derive(Debug)]
struct DeviceBoundary {
    root: fs::File,
    root_identity: DirectoryIdentity,
    dirs: Vec<(&'static str, fs::File, DirectoryIdentity)>,
}

#[derive(Debug)]
struct ScopeBoundary {
    name: String,
    scope: fs::File,
    scope_identity: DirectoryIdentity,
    kio: fs::File,
    kio_identity: DirectoryIdentity,
}

impl DeviceBoundary {
    fn bind_existing(root: &fs::File, corpus: &Path) -> Result<Self, ScalePrepareError> {
        let device = open_dir(root, DEVICE_DIR_NAME, corpus)?;
        let root_identity = directory_identity(&device)?;
        let mut dirs = Vec::with_capacity(DEVICE_SUBDIRS.len());
        for name in DEVICE_SUBDIRS {
            let handle = open_dir(&device, name, corpus)?;
            dirs.push((name, handle.try_clone()?, directory_identity(&handle)?));
        }
        let boundary = Self {
            root: device,
            root_identity,
            dirs,
        };
        boundary.recheck(corpus)?;
        Ok(boundary)
    }

    fn bind_or_create(root: &fs::File, corpus: &Path) -> Result<Self, ScalePrepareError> {
        let device = create_or_open_dir(root, DEVICE_DIR_NAME, corpus)?;
        let root_identity = directory_identity(&device)?;
        let mut dirs = Vec::with_capacity(DEVICE_SUBDIRS.len());
        for name in DEVICE_SUBDIRS {
            let handle = create_or_open_dir(&device, name, corpus)?;
            dirs.push((name, handle.try_clone()?, directory_identity(&handle)?));
        }
        let boundary = Self {
            root: device,
            root_identity,
            dirs,
        };
        boundary.recheck(corpus)?;
        Ok(boundary)
    }

    fn dir(&self, name: &str) -> Result<&fs::File, ScalePrepareError> {
        self.dirs
            .iter()
            .find(|(candidate, _, _)| *candidate == name)
            .map(|(_, handle, _)| handle)
            .ok_or_else(|| ScalePrepareError::Input("private device layout is incomplete".into()))
    }

    fn recheck(&self, corpus: &Path) -> Result<(), ScalePrepareError> {
        if directory_identity(&self.root)? != self.root_identity {
            return Err(ScalePrepareError::Input(
                "private device descriptor changed".into(),
            ));
        }
        let entries = cap_fs::read_dir(&self.root, Path::new("."))
            .map_err(|error| {
                ScalePrepareError::Input(format!("cannot enumerate private device: {error}"))
            })?
            .take(7)
            .map(|entry| entry.map(|entry| entry.file_name().to_str().map(str::to_owned)))
            .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
        let entries = entries
            .into_iter()
            .collect::<Option<std::collections::BTreeSet<_>>>()
            .ok_or_else(|| ScalePrepareError::Input("private device entry is not UTF-8".into()))?;
        let expected = DEVICE_SUBDIRS
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        if entries != expected {
            return Err(ScalePrepareError::Input(
                "private device layout has unknown or missing entries".into(),
            ));
        }
        for (name, handle, identity) in &self.dirs {
            let named = open_dir(&self.root, name, corpus)?;
            if directory_identity(handle)? != *identity || directory_identity(&named)? != *identity
            {
                return Err(ScalePrepareError::Input(
                    "private device directory was replaced".into(),
                ));
            }
        }
        Ok(())
    }
}

impl ScopeBoundary {
    fn bind(root: &fs::File, name: &str, corpus: &Path) -> Result<Self, ScalePrepareError> {
        let scope = open_dir(root, name, corpus)?;
        let kio = open_dir(&scope, ".kio", corpus)?;
        Ok(Self {
            name: name.to_owned(),
            scope_identity: directory_identity(&scope)?,
            kio_identity: directory_identity(&kio)?,
            scope,
            kio,
        })
    }

    fn recheck(&self, root: &fs::File, corpus: &Path) -> Result<(), ScalePrepareError> {
        let named = open_dir(root, &self.name, corpus)?;
        if directory_identity(&self.scope)? != self.scope_identity
            || directory_identity(&named)? != self.scope_identity
        {
            return Err(ScalePrepareError::Input(
                "scope directory was replaced".into(),
            ));
        }
        let named_kio = open_dir(&self.scope, ".kio", corpus)?;
        if directory_identity(&self.kio)? != self.kio_identity
            || directory_identity(&named_kio)? != self.kio_identity
        {
            return Err(ScalePrepareError::Input(
                "scope .kio directory was replaced".into(),
            ));
        }
        Ok(())
    }
}

fn directory_identity(file: &fs::File) -> Result<DirectoryIdentity, ScalePrepareError> {
    let metadata = cap_fs::Metadata::from_file(file)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ScalePrepareError::Input("expected a real directory".into()));
    }
    #[cfg(unix)]
    {
        Ok(DirectoryIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        Ok(DirectoryIdentity {
            volume: metadata.volume_serial_number().unwrap_or(0),
            index: metadata.file_index().unwrap_or(0),
        })
    }
}

#[derive(Debug, Error)]
pub enum ScalePrepareError {
    #[error("invalid scale prepare input: {0}")]
    Input(String),
    #[error(transparent)]
    Fixture(#[from] ScaleFixtureError),
    #[error(transparent)]
    Spec(#[from] scale_spec::ScaleSpecError),
    #[error("unsafe scale prepare process boundary: {0}")]
    ProcessBoundary(String),
    #[error(transparent)]
    Process(#[from] crate::runner::BoundedProcessError),
    #[error("scale prepare filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("scale prepare serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct PrepareSummary {
    pub corpus: PathBuf,
    pub initialized_scopes: usize,
    pub indexed_scopes: usize,
    pub report: PathBuf,
}

/// Read-only descriptor view of the private device created by `prepare`.
/// It deliberately exposes handles rather than paths so benchmark subprocesses
/// retain the same pathname-replacement boundary as preparation.
pub(crate) struct BenchmarkDevice {
    boundary: DeviceBoundary,
}
impl BenchmarkDevice {
    pub(crate) fn bind(
        fixture: &crate::scale_fixture::ValidatedFixture,
    ) -> Result<Self, ScalePrepareError> {
        Ok(Self {
            boundary: DeviceBoundary::bind_existing(&fixture.try_clone_root()?, fixture.root())?,
        })
    }
    pub(crate) fn environment(&self) -> Result<Vec<(&'static str, &fs::File)>, ScalePrepareError> {
        Ok(vec![
            ("HOME", self.boundary.dir("home")?),
            ("XDG_CONFIG_HOME", self.boundary.dir("config")?),
            ("XDG_CACHE_HOME", self.boundary.dir("cache")?),
            ("XDG_DATA_HOME", self.boundary.dir("data")?),
            ("XDG_STATE_HOME", self.boundary.dir("state")?),
            ("XDG_RUNTIME_DIR", self.boundary.dir("runtime")?),
        ])
    }
    pub(crate) fn recheck(&self, corpus: &Path) -> Result<(), ScalePrepareError> {
        self.boundary.recheck(corpus)
    }

    pub(crate) fn metrics_directory(&self, corpus: &Path) -> Result<fs::File, ScalePrepareError> {
        let kio = open_dir(self.boundary.dir("data")?, "kio", corpus)?;
        open_dir(&kio, "logs", corpus)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareReport {
    schema_version: u64,
    preparer: String,
    fixture_id: String,
    profile: scale_spec::ScaleProfile,
    lane: scale_spec::ScaleLane,
    manifest_hash: String,
    corpus: String,
    binary: BinaryBinding,
    scopes: Vec<PreparedScopeReceipt>,
    registry_rows: usize,
    current_chunks: u64,
    historical_only_chunks: u64,
    deleted_chunks: u64,
    physical_chunks: u64,
    embedded_chunks: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BinaryBinding {
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedScopeReceipt {
    name: String,
    scope_id: String,
    base_head: Option<String>,
    final_head: String,
    base_tree: Option<String>,
    final_tree: String,
    source_files: usize,
    current_chunks: u64,
    physical_chunks: u64,
    embedded_chunks: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitOutput {
    status: String,
    repaired: Vec<String>,
    path: String,
    kio_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexOutput {
    status: String,
    approval_method: String,
    network_allowed: bool,
    network_opt_in: bool,
    pending_online_tasks: u64,
    pending_files: u64,
    paused_tasks: u64,
    failed_files: u64,
    normalized_files: u64,
    skipped_oversized_files: u64,
    skipped_unrecognized_binary_files: u64,
    embedding_tasks_executed: u64,
    embedding_tasks_failed: u64,
    tree_hash: Option<String>,
    commit_hash: Option<String>,
    #[allow(dead_code)]
    commit: Option<Value>,
    budget_warning: Option<Value>,
    skipped_units: Vec<Value>,
    child_scopes: Vec<Value>,
    child_scope_discovery: ChildScopeDiscoveryOutput,
    #[serde(default)]
    skipped_units_guidance: Option<String>,
    gc: GcReceipt,
}

/// Every preparer invocation is a public, mutating index command, including
/// registry no-ops. It must finish discovery even when the fixture is flat.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildScopeDiscoveryOutput {
    page_size: usize,
    has_more: bool,
    continuation_committed: bool,
    // A stale frontier can legitimately restart and then finish an empty scan.
    // Both boolean values are valid; completion is checked independently below.
    #[allow(dead_code)]
    restarted_from_root: bool,
    rows_total: u64,
    rows_omitted: u64,
}

impl ChildScopeDiscoveryOutput {
    fn is_completed_empty(&self) -> bool {
        self.page_size == kio_pipeline::scan::CHILD_SCOPE_DISCOVERY_PAGE_SIZE
            && !self.has_more
            && self.continuation_committed
            && self.rows_total == 0
            && self.rows_omitted == 0
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GcReceipt {
    mode: String,
    reason: String,
    status: String,
    trigger: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AutoCommitWire {
    commit_type: String,
    created_at: String,
    message: String,
    object_type: String,
    parent: Option<String>,
    stats: AutoCommitStatsWire,
    tool_lock_hash: String,
    tree: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AutoCommitStatsWire {
    files_added: u64,
    files_modified: u64,
    files_deleted: u64,
}

/// Initialize and index the exact manifest scopes.  The platform preflight is
/// intentionally first: unsupported systems never create the private device
/// directory or mutate a fixture.
pub fn prepare(corpus: &Path, bin: &Path) -> Result<PrepareSummary, ScalePrepareError> {
    DescriptorExecutable::preflight_platform().map_err(process_error)?;
    let bin = absolutize_binary(bin)?;
    let executable = DescriptorExecutable::bind_build_artifact(&bin).map_err(process_error)?;
    let mut fixture = bind_ready(corpus)?;
    let lock = fixture.lock()?;
    fixture.recheck()?;
    executable.recheck_original().map_err(process_error)?;

    // A history fixture is deliberately prepared in two explicit snapshots.
    // The source overlay is applied only after every base scope has gone
    // through the ordinary CLI init/index path; neither the index nor CAS is
    // written by this evaluator.
    if fixture.lane() == scale_spec::ScaleLane::HistoryOverlay {
        return prepare_history_overlay(corpus, &executable, &mut fixture, &lock);
    }

    let root = fixture.try_clone_root()?;
    // Classify every declared scope before creating the device or spawning a
    // writer. An unsafe late scope must never leave earlier scopes repaired.
    let mut ready_scopes = Vec::new();
    let mut has_incomplete_scope = false;
    for scope in &fixture.manifest().scopes {
        match crate::scale_attest::attest_scope(&fixture, &scope.name) {
            Ok(evidence) => ready_scopes.push(evidence),
            Err(crate::scale_attest::AttestError::Incomplete(_)) => has_incomplete_scope = true,
            Err(error) => {
                return Err(ScalePrepareError::Input(format!(
                    "scope {} is unsafe: {error}",
                    scope.name
                )));
            }
        }
    }
    let initial_scopes = if has_incomplete_scope {
        None
    } else {
        Some(ready_scopes)
    };
    if let Some(scopes) = &initial_scopes {
        match crate::scale_attest::attest_registry(&fixture, scopes) {
            Ok(registry_rows) => {
                let device = DeviceBoundary::bind_existing(&root, corpus)?;
                let expected = expected_report(&fixture, &executable, scopes, registry_rows)?;
                match cap_fs::stat(
                    &root,
                    Path::new(PREPARE_REPORT_NAME),
                    cap_fs::FollowSymlinks::No,
                ) {
                    Ok(_) => verify_exact_report(&read_existing_report(&root)?, &expected)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        fixture.recheck()?;
                        device.recheck(corpus)?;
                        executable.recheck_original().map_err(process_error)?;
                        return publish_report(
                            &fixture,
                            &executable,
                            &root,
                            &device,
                            corpus,
                            expected,
                            (0, 0),
                        );
                    }
                    Err(error) => return Err(error.into()),
                }
                fixture.recheck()?;
                device.recheck(corpus)?;
                executable.recheck_original().map_err(process_error)?;
                return Ok(PrepareSummary {
                    corpus: corpus.to_owned(),
                    initialized_scopes: 0,
                    indexed_scopes: 0,
                    report: corpus.join(PREPARE_REPORT_NAME),
                });
            }
            Err(crate::scale_attest::AttestError::Incomplete(_)) => {
                // Registry-only recovery: do not rebuild ready scopes. Each
                // no-op index invocation re-registers its retained scope.
                let device = DeviceBoundary::bind_or_create(&root, corpus)?;
                for scope in scopes {
                    let boundary = ScopeBoundary::bind(&root, &scope.name, corpus)?;
                    let head = read_head(&boundary.kio)?;
                    let output = run_scope(
                        &executable,
                        &fixture,
                        &boundary.scope,
                        &device,
                        &OFFLINE_INDEX_ARGS,
                    )?;
                    let expected_files = fixture
                        .manifest()
                        .scopes
                        .iter()
                        .find(|candidate| candidate.name == scope.name)
                        .map(|candidate| candidate.expected_files)
                        .ok_or_else(|| {
                            ScalePrepareError::Input(
                                "attested scope is absent from manifest".into(),
                            )
                        })?;
                    validate_index_output(
                        &output,
                        IndexExpectation::RegistryNoop {
                            files: expected_files,
                        },
                    )?;
                    if output.get("status").and_then(Value::as_str) != Some("noop")
                        || read_head(&boundary.kio)? != head
                    {
                        return Err(ScalePrepareError::Input(
                            "registry recovery changed a ready scope HEAD".into(),
                        ));
                    }
                    boundary.recheck(&root, corpus)?;
                }
                let registry_rows = crate::scale_attest::attest_registry(&fixture, scopes)
                    .map_err(|error| {
                        ScalePrepareError::Input(format!(
                            "registry recovery did not attest: {error}"
                        ))
                    })?;
                let evidence = crate::scale_attest::attest_ready(&fixture).map_err(|error| {
                    ScalePrepareError::Input(format!("registry recovery is incomplete: {error}"))
                })?;
                let report =
                    expected_report(&fixture, &executable, &evidence.scopes, registry_rows)?;
                return publish_report(
                    &fixture,
                    &executable,
                    &root,
                    &device,
                    corpus,
                    report,
                    (0, 0),
                );
            }
            Err(error) => {
                return Err(ScalePrepareError::Input(format!(
                    "prepared registry is unsafe: {error}"
                )));
            }
        }
    }
    debug_assert!(has_incomplete_scope);
    let device = DeviceBoundary::bind_or_create(&root, corpus)?;
    let mut initialized = Vec::new();
    let mut indexed = Vec::new();
    for scope in &fixture.manifest().scopes {
        fixture.recheck()?;
        executable.recheck_original().map_err(process_error)?;
        match crate::scale_attest::attest_scope(&fixture, &scope.name) {
            Ok(_) => continue,
            Err(crate::scale_attest::AttestError::Incomplete(_)) => {}
            Err(error) => {
                return Err(ScalePrepareError::Input(format!(
                    "scope {} is unsafe: {error}",
                    scope.name
                )));
            }
        }
        let scope_handle = open_dir(&root, &scope.name, corpus)?;
        let has_kio =
            match cap_fs::stat(&scope_handle, Path::new(".kio"), cap_fs::FollowSymlinks::No) {
                Ok(metadata)
                    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() =>
                {
                    true
                }
                Ok(_) => {
                    return Err(ScalePrepareError::Input(format!(
                        "scope {} has unsafe .kio",
                        scope.name
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            };
        if !has_kio {
            let init = run_scope(
                &executable,
                &fixture,
                &scope_handle,
                &device,
                &["--json", "init", "."],
            )?;
            validate_init_output(&init, corpus.join(&scope.name))?;
            initialized.push(scope.name.clone());
        }
        let scope_boundary = ScopeBoundary::bind(&root, &scope.name, corpus)?;
        scope_boundary.recheck(&root, corpus)?;
        let output = run_scope(
            &executable,
            &fixture,
            &scope_boundary.scope,
            &device,
            &OFFLINE_INDEX_ARGS,
        )?;
        validate_index_output(
            &output,
            IndexExpectation::Repair {
                files: scope.expected_files,
                embeddings: scope.expected_current_chunks,
            },
        )?;
        indexed.push(scope.name.clone());
        fixture.recheck()?;
        executable.recheck_original().map_err(process_error)?;
        // The child can only receive the retained descriptor, but check the
        // named scope again before accepting its output.
        scope_boundary.recheck(&root, corpus)?;
        device.recheck(corpus)?;
    }

    fixture.recheck()?;
    device.recheck(corpus)?;
    executable.recheck_original().map_err(process_error)?;
    let evidence = crate::scale_attest::attest_ready(&fixture).map_err(|error| {
        ScalePrepareError::Input(format!("repair did not produce a ready corpus: {error}"))
    })?;
    let report = expected_report(
        &fixture,
        &executable,
        &evidence.scopes,
        evidence.registry_rows,
    )?;
    publish_report(
        &fixture,
        &executable,
        &root,
        &device,
        corpus,
        report,
        (initialized.len(), indexed.len()),
    )
}

fn run_scope(
    executable: &DescriptorExecutable,
    fixture: &crate::scale_fixture::ValidatedFixture,
    scope: &fs::File,
    device: &DeviceBoundary,
    args: &[&str],
) -> Result<Value, ScalePrepareError> {
    fixture.recheck()?;
    executable.recheck_original().map_err(process_error)?;
    device.recheck(fixture.root())?;
    let mut command = executable.command().map_err(process_error)?;
    command.args(args);
    configure_retained_cwd(&mut command, scope).map_err(process_error)?;
    configure_descriptor_environment(
        &mut command,
        &[
            ("HOME", device.dir("home")?),
            ("XDG_CONFIG_HOME", device.dir("config")?),
            ("XDG_CACHE_HOME", device.dir("cache")?),
            ("XDG_DATA_HOME", device.dir("data")?),
            ("XDG_STATE_HOME", device.dir("state")?),
            ("XDG_RUNTIME_DIR", device.dir("runtime")?),
        ],
    )
    .map_err(process_error)?;
    // Environment sealing above clears ambient variables. Add the evaluator's
    // exact deterministic adapter selector only after that boundary is fixed.
    // It still travels through the production CLI adapter wire.
    command.env("KIO_EVAL_DETERMINISTIC_EMBED", "scale-v3");
    let output = run_bounded_command(
        &mut command,
        BoundedProcessOptions {
            timeout: PREPARE_TIMEOUT,
            max_stdout_bytes: MAX_JSON_BYTES,
            max_stderr_bytes: MAX_JSON_BYTES,
        },
        None,
    )?;
    fixture.recheck()?;
    executable.recheck_original().map_err(process_error)?;
    device.recheck(fixture.root())?;
    if !output.status.success() {
        return Err(ScalePrepareError::Input(format!(
            "Kio command failed: {}",
            output.stderr.trim()
        )));
    }
    let value: Value = serde_json::from_str(&output.stdout)
        .map_err(|e| ScalePrepareError::Input(format!("Kio output is not JSON: {e}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| ScalePrepareError::Input("Kio output must be an object".into()))?;
    if object.get("error_code").is_some()
        || object.get("__exit_code").is_some()
        || object.get("status").and_then(Value::as_str).is_none()
    {
        return Err(ScalePrepareError::Input(
            "Kio output contains an error or lacks status".into(),
        ));
    }
    Ok(value)
}

/// The history lane has no resumable half-overlay state: a bound fixture is
/// either still the exact base sources or is fully overlaid.  In the latter
/// case the independent attestor decides whether both snapshots are present.
fn prepare_history_overlay(
    corpus: &Path,
    executable: &DescriptorExecutable,
    fixture: &mut crate::scale_fixture::ValidatedFixture,
    lock: &crate::scale_fixture::FixtureLock,
) -> Result<PrepareSummary, ScalePrepareError> {
    if fixture.source_state() != scale_spec::SourceState::Base {
        return Err(ScalePrepareError::Input(
            "history fixture is already overlaid; refuse to infer a resumable snapshot".into(),
        ));
    }
    let root = fixture.try_clone_root()?;
    let device = DeviceBoundary::bind_or_create(&root, corpus)?;
    let mut initialized = 0usize;
    let mut base = Vec::with_capacity(fixture.manifest().scopes.len());
    for scope in &fixture.manifest().scopes {
        fixture.recheck()?;
        let scope_handle = open_dir(&root, &scope.name, corpus)?;
        if cap_fs::stat(&scope_handle, Path::new(".kio"), cap_fs::FollowSymlinks::No).is_ok() {
            return Err(ScalePrepareError::Input(
                "history base scope already has .kio; refuse partial recovery".into(),
            ));
        }
        let init = run_scope(
            executable,
            fixture,
            &scope_handle,
            &device,
            &["--json", "init", "."],
        )?;
        validate_init_output(&init, corpus.join(&scope.name))?;
        initialized += 1;
        let boundary = ScopeBoundary::bind(&root, &scope.name, corpus)?;
        let index = run_scope(
            executable,
            fixture,
            &boundary.scope,
            &device,
            &OFFLINE_INDEX_ARGS,
        )?;
        validate_index_output(
            &index,
            IndexExpectation::Repair {
                files: fixture.profile().files_per_scope(),
                embeddings: fixture.profile().files_per_scope()
                    * fixture.profile().sections_per_file(),
            },
        )?;
        let output: IndexOutput = serde_json::from_value(index)
            .map_err(|e| ScalePrepareError::Input(format!("base index schema: {e}")))?;
        base.push((
            scope.name.clone(),
            output
                .commit_hash
                .ok_or_else(|| ScalePrepareError::Input("base index lacks commit".into()))?,
            output
                .tree_hash
                .ok_or_else(|| ScalePrepareError::Input("base index lacks tree".into()))?,
        ));
        boundary.recheck(&root, corpus)?;
    }
    fixture.apply_history_overlay(lock)?;
    fixture.recheck()?;
    let mut final_receipts = Vec::with_capacity(fixture.manifest().scopes.len());
    for scope in &fixture.manifest().scopes {
        let boundary = ScopeBoundary::bind(&root, &scope.name, corpus)?;
        let index = run_scope(
            executable,
            fixture,
            &boundary.scope,
            &device,
            &OFFLINE_INDEX_ARGS,
        )?;
        let (_, base_head, _) = base
            .iter()
            .find(|(name, _, _)| name == &scope.name)
            .ok_or_else(|| ScalePrepareError::Input("missing base scope receipt".into()))?;
        validate_index_output(
            &index,
            IndexExpectation::Overlay {
                files: scope.expected_files,
                embeddings: fixture.profile().sections_per_file() + 1,
                parent: base_head.clone(),
            },
        )?;
        let output: IndexOutput = serde_json::from_value(index)
            .map_err(|e| ScalePrepareError::Input(format!("overlay index schema: {e}")))?;
        let (base_name, base_head, base_tree) = base
            .iter()
            .find(|(name, _, _)| name == &scope.name)
            .ok_or_else(|| ScalePrepareError::Input("missing base scope receipt".into()))?;
        if base_name != &scope.name
            || output
                .commit_hash
                .as_deref()
                .is_none_or(|head| !valid_hash(Some(head)))
            || output
                .tree_hash
                .as_deref()
                .is_none_or(|tree| !valid_hash(Some(tree)))
        {
            return Err(ScalePrepareError::Input(
                "overlay index lacks canonical bindings".into(),
            ));
        }
        final_receipts.push(PreparedScopeReceipt {
            name: scope.name.clone(),
            scope_id: String::new(),
            base_head: Some(base_head.clone()),
            final_head: output.commit_hash.unwrap(),
            base_tree: Some(base_tree.clone()),
            final_tree: output.tree_hash.unwrap(),
            source_files: scope.files.len(),
            current_chunks: 0,
            physical_chunks: 0,
            embedded_chunks: 0,
        });
        boundary.recheck(&root, corpus)?;
    }
    let evidence = crate::scale_attest::attest_ready(fixture)
        .map_err(|e| ScalePrepareError::Input(format!("history overlay did not attest: {e}")))?;
    for receipt in &mut final_receipts {
        let scope = evidence
            .scopes
            .iter()
            .find(|s| s.name == receipt.name)
            .ok_or_else(|| ScalePrepareError::Input("attestor omitted history scope".into()))?;
        receipt.scope_id = scope.scope_id.clone();
        receipt.current_chunks = scope.current_chunks;
        receipt.physical_chunks = scope.physical_chunks;
        receipt.embedded_chunks = scope.embedded_chunks;
        if receipt.final_head != scope.head {
            return Err(ScalePrepareError::Input(
                "final index/attestor HEAD differs".into(),
            ));
        }
    }
    let report = expected_history_report(
        fixture,
        executable,
        final_receipts,
        evidence.registry_rows,
        &evidence,
    )?;
    publish_report(
        fixture,
        executable,
        &root,
        &device,
        corpus,
        report,
        (initialized, fixture.manifest().scopes.len() * 2),
    )
}

fn validate_init_output(value: &Value, scope: PathBuf) -> Result<(), ScalePrepareError> {
    let output: InitOutput = serde_json::from_value(value.clone()).map_err(|error| {
        ScalePrepareError::Input(format!("init output violates canonical schema: {error}"))
    })?;
    if output.status != "initialized"
        || !output.repaired.is_empty()
        || output.path != scope.to_string_lossy()
        || output.kio_path != scope.join(".kio").to_string_lossy()
    {
        return Err(ScalePrepareError::Input(
            "init output does not bind the fresh retained scope".into(),
        ));
    }
    Ok(())
}

fn validate_index_output(
    value: &Value,
    expected: IndexExpectation,
) -> Result<(), ScalePrepareError> {
    let output: IndexOutput = serde_json::from_value(value.clone()).map_err(|error| {
        ScalePrepareError::Input(format!("index output violates canonical schema: {error}"))
    })?;
    if output.approval_method != "yes"
        || output.network_allowed
        || output.network_opt_in
        || output.pending_online_tasks != 0
        || output.pending_files != 0
        || output.paused_tasks != 0
        || output.failed_files != 0
        || output.embedding_tasks_failed != 0
        || output.skipped_oversized_files != 0
        || output.skipped_unrecognized_binary_files != 0
        || !output.skipped_units.is_empty()
        || !output.child_scopes.is_empty()
        || !output.child_scope_discovery.is_completed_empty()
        || output.budget_warning.is_some()
        || output.skipped_units_guidance.is_some()
        || output.gc.mode != "manual_only"
        || output.gc.reason != "manual_only"
        || output.gc.status != "disabled"
        || output.gc.trigger != "index"
    {
        return Err(ScalePrepareError::Input(
            "offline index output reports a pending, skipped, network, budget, discovery, or GC failure state"
                .into(),
        ));
    }
    match expected {
        IndexExpectation::Repair { files, embeddings }
            if output.status == "indexed"
                && output.normalized_files == files as u64
                && output.embedding_tasks_executed == embeddings as u64
                && valid_hash(output.commit_hash.as_deref())
                && valid_hash(output.tree_hash.as_deref())
                && strict_auto_commit(
                    output.commit.as_ref(),
                    output.commit_hash.as_deref(),
                    output.tree_hash.as_deref(),
                    files,
                )? => {}
        IndexExpectation::RegistryNoop { files }
            if output.status == "noop"
                && output.normalized_files == files as u64
                && output.embedding_tasks_executed == 0
                && output.commit_hash.is_none()
                && output.commit.is_none()
                && valid_hash(output.tree_hash.as_deref()) => {}
        IndexExpectation::Overlay {
            files,
            embeddings,
            parent,
        } if output.status == "indexed"
            && output.normalized_files == files as u64
            && output.embedding_tasks_executed == embeddings as u64
            && valid_hash(output.commit_hash.as_deref())
            && valid_hash(output.tree_hash.as_deref())
            && strict_history_overlay_commit(
                output.commit.as_ref(),
                output.commit_hash.as_deref(),
                output.tree_hash.as_deref(),
                &parent,
            )? => {}
        _ => {
            return Err(ScalePrepareError::Input(format!(
                "index output violates the requested repair/noop semantics: {}",
                serde_json::to_string(value).unwrap_or_else(|_| "<invalid-json>".into())
            )));
        }
    }
    Ok(())
}

fn strict_history_overlay_commit(
    commit: Option<&Value>,
    commit_hash: Option<&str>,
    tree_hash: Option<&str>,
    parent: &str,
) -> Result<bool, ScalePrepareError> {
    let Some(value) = commit else {
        return Ok(false);
    };
    let parsed: AutoCommitWire = serde_json::from_value(value.clone())
        .map_err(|e| ScalePrepareError::Input(format!("history commit schema: {e}")))?;
    Ok(parsed.commit_type == "auto"
        && parsed.object_type == "commit"
        && parsed.message == "kio index auto snapshot"
        && scale_spec::is_canonical_utc_second(&parsed.created_at)
        && parsed.parent.as_deref() == Some(parent)
        && parsed.stats.files_added == 1
        && parsed.stats.files_modified == 1
        && parsed.stats.files_deleted == 2
        && parsed.tree == tree_hash.unwrap_or_default()
        && canonical_value_hash(value)? == commit_hash.unwrap_or_default())
}

fn strict_auto_commit(
    commit: Option<&Value>,
    commit_hash: Option<&str>,
    tree_hash: Option<&str>,
    expected_files: usize,
) -> Result<bool, ScalePrepareError> {
    let Some(value) = commit else {
        return Ok(false);
    };
    let parsed: AutoCommitWire = serde_json::from_value(value.clone())
        .map_err(|e| ScalePrepareError::Input(format!("index commit schema: {e}")))?;
    let computed_hash = canonical_value_hash(value)?;
    Ok(parsed.commit_type == "auto"
        && parsed.object_type == "commit"
        && parsed.message == "kio index auto snapshot"
        && scale_spec::is_canonical_utc_second(&parsed.created_at)
        && parsed
            .parent
            .as_deref()
            .is_none_or(|parent| valid_hash(Some(parent)))
        && valid_hash(Some(&parsed.tool_lock_hash))
        && parsed.stats.files_added == expected_files as u64
        && parsed.stats.files_modified == 0
        && parsed.stats.files_deleted == 0
        && parsed.tree == tree_hash.unwrap_or_default()
        && computed_hash == commit_hash.unwrap_or_default())
}

fn canonical_value_hash(value: &Value) -> Result<String, ScalePrepareError> {
    let canonical = canonical_json_bytes(value).map_err(|error| {
        ScalePrepareError::Input(format!("cannot canonicalize index commit: {error}"))
    })?;
    let digest = Sha256::digest(&canonical);
    Ok(format!("sha256:{}", kio_core::cas::lower_hex(&digest)))
}

fn valid_hash(value: Option<&str>) -> bool {
    value.is_some_and(kio_core::cas::is_hash)
}

fn process_error(error: ProcessBoundaryError) -> ScalePrepareError {
    ScalePrepareError::ProcessBoundary(error.to_string())
}

fn absolutize_binary(bin: &Path) -> Result<PathBuf, ScalePrepareError> {
    if bin.is_absolute() {
        return Ok(bin.to_owned());
    }
    if bin.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(ScalePrepareError::Input(
            "relative binary path must be lexical normal components".into(),
        ));
    }
    let cwd = std::env::current_dir().map_err(|error| {
        ScalePrepareError::Input(format!("cannot resolve current directory: {error}"))
    })?;
    let absolute = cwd.join(bin);
    if !absolute.is_absolute() {
        return Err(ScalePrepareError::Input(
            "binary path cannot be made absolute".into(),
        ));
    }
    Ok(absolute)
}

fn open_dir(parent: &fs::File, name: &str, label: &Path) -> Result<fs::File, ScalePrepareError> {
    cap_fs::open_dir_nofollow(parent, Path::new(name)).map_err(|_| {
        ScalePrepareError::Input(format!("unsafe or missing scope under {}", label.display()))
    })
}

fn create_or_open_dir(
    parent: &fs::File,
    name: &str,
    label: &Path,
) -> Result<fs::File, ScalePrepareError> {
    match open_dir(parent, name, label) {
        Ok(dir) => Ok(dir),
        Err(_) => match cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut options = cap_fs::DirOptions::new();
                #[cfg(unix)]
                {
                    use cap_fs::DirBuilderExt;
                    options.mode(0o700);
                }
                match cap_fs::create_dir(parent, Path::new(name), &options) {
                    Ok(()) => open_dir(parent, name, label),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        open_dir(parent, name, label)
                    }
                    Err(error) => Err(error.into()),
                }
            }
            Ok(_) => Err(ScalePrepareError::Input(format!(
                "unsafe private device entry {name}"
            ))),
            Err(error) => Err(error.into()),
        },
    }
}

fn publish_new_report(
    root: &fs::File,
    state: &fs::File,
    bytes: &[u8],
    label: &Path,
) -> Result<(), ScalePrepareError> {
    let state_metadata = state.metadata()?;
    let root_metadata = root.metadata()?;
    let temp_path = Path::new(PREPARE_REPORT_TEMP_NAME);
    let opened = match cap_fs::stat(state, temp_path, cap_fs::FollowSymlinks::No) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut options = cap_fs::OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
            let mut file = cap_fs::open(state, temp_path, &options).map_err(|error| {
                ScalePrepareError::Input(format!(
                    "cannot create private prepare report staging file: {error}"
                ))
            })?;
            use io::Write;
            file.write_all(bytes)?;
            file.sync_all()?;
            cap_fs::Metadata::from_file(&file)?
        }
        Ok(metadata) => {
            let actual = read_regular_file(
                state,
                PREPARE_REPORT_TEMP_NAME,
                MAX_JSON_BYTES,
                "private prepare report staging file",
            )?;
            if actual != bytes {
                return Err(ScalePrepareError::Input(
                    "private prepare report staging file is stale or torn; preserve it and recreate the owned fixture"
                        .into(),
                ));
            }
            metadata
        }
        Err(error) => return Err(error.into()),
    };

    let staged = read_regular_file(
        state,
        PREPARE_REPORT_TEMP_NAME,
        MAX_JSON_BYTES,
        "private prepare report staging file",
    )?;
    if staged != bytes {
        return Err(ScalePrepareError::Input(
            "private prepare report changed before publication".into(),
        ));
    }
    let named = cap_fs::stat(state, temp_path, cap_fs::FollowSymlinks::No)?;
    if !same_directory_file(&opened, &named) || named.len() != bytes.len() as u64 {
        return Err(ScalePrepareError::Input(
            "private prepare report identity changed before publication".into(),
        ));
    }
    match cap_fs::stat(
        root,
        Path::new(PREPARE_REPORT_NAME),
        cap_fs::FollowSymlinks::No,
    ) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(ScalePrepareError::Input(
                "canonical prepare report appeared before create-only publication".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    rename_noreplace(state, PREPARE_REPORT_TEMP_NAME, root, PREPARE_REPORT_NAME)?;
    sync_retained_directory(
        state,
        &state_metadata,
        &label.join(DEVICE_DIR_NAME).join("state"),
    )
    .map_err(|error| {
        ScalePrepareError::Input(format!(
            "cannot sync private prepare report staging directory: {error}"
        ))
    })?;
    sync_retained_directory(root, &root_metadata, label).map_err(|error| {
        ScalePrepareError::Input(format!("cannot sync prepare report parent: {error}"))
    })?;
    let published = cap_fs::stat(
        root,
        Path::new(PREPARE_REPORT_NAME),
        cap_fs::FollowSymlinks::No,
    )?;
    if !same_directory_file(&opened, &published) || published.len() != bytes.len() as u64 {
        return Err(ScalePrepareError::Input(
            "published prepare report identity differs from staging".into(),
        ));
    }
    if read_existing_report(root)? != bytes {
        return Err(ScalePrepareError::Input(
            "published prepare report bytes differ from staging".into(),
        ));
    }
    Ok(())
}

fn ensure_prepare_temp_absent(state: &fs::File) -> Result<(), ScalePrepareError> {
    match cap_fs::stat(
        state,
        Path::new(PREPARE_REPORT_TEMP_NAME),
        cap_fs::FollowSymlinks::No,
    ) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(ScalePrepareError::Input(
            "private prepare report staging file remains beside a published report".into(),
        )),
        Err(error) => Err(error.into()),
    }
}

fn read_regular_file(
    parent: &fs::File,
    name: &str,
    max_bytes: usize,
    label: &str,
) -> Result<Vec<u8>, ScalePrepareError> {
    let before = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if !before.is_file() || before.file_type().is_symlink() || before.len() > max_bytes as u64 {
        return Err(ScalePrepareError::Input(format!(
            "{label} must be a bounded regular file"
        )));
    }
    #[cfg(unix)]
    if before.nlink() != 1 {
        return Err(ScalePrepareError::Input(format!(
            "{label} must have exactly one hard link"
        )));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(parent, Path::new(name), &options)?;
    let opened = cap_fs::Metadata::from_file(&file)?;
    let mut actual = Vec::with_capacity(usize::try_from(opened.len()).unwrap_or(0));
    use io::Read;
    file.take(max_bytes as u64 + 1).read_to_end(&mut actual)?;
    let after = cap_fs::stat(parent, Path::new(name), cap_fs::FollowSymlinks::No)?;
    if !opened.is_file()
        || opened.len() != actual.len() as u64
        || !same_directory_file(&before, &opened)
        || !same_directory_file(&opened, &after)
    {
        return Err(ScalePrepareError::Input(format!(
            "{label} changed while reading"
        )));
    }
    Ok(actual)
}

fn expected_report(
    fixture: &crate::scale_fixture::ValidatedFixture,
    executable: &DescriptorExecutable,
    scopes: &[crate::scale_attest::ScopeEvidence],
    registry_rows: usize,
) -> Result<PrepareReport, ScalePrepareError> {
    Ok(PrepareReport {
        schema_version: scale_spec::SCHEMA_VERSION,
        preparer: PREPARER_ID.to_owned(),
        fixture_id: scale_spec::FIXTURE_ID.to_owned(),
        profile: fixture.profile(),
        lane: fixture.lane(),
        manifest_hash: scale_spec::manifest_hash(fixture.manifest())?,
        corpus: fixture.root().to_string_lossy().into_owned(),
        binary: BinaryBinding {
            sha256: format!("sha256:{}", executable.immutable_binding().sha256),
            bytes: executable.immutable_binding().bytes,
        },
        scopes: scopes
            .iter()
            .map(|scope| PreparedScopeReceipt {
                name: scope.name.clone(),
                scope_id: scope.scope_id.clone(),
                base_head: scope.base_head.clone(),
                final_head: scope.head.clone(),
                base_tree: scope.base_tree.clone(),
                final_tree: scope.tree.clone(),
                source_files: scope.source_files,
                current_chunks: scope.current_chunks,
                physical_chunks: scope.physical_chunks,
                embedded_chunks: scope.embedded_chunks,
            })
            .collect(),
        registry_rows,
        current_chunks: scopes.iter().map(|scope| scope.current_chunks).sum(),
        historical_only_chunks: scopes
            .iter()
            .map(|scope| scope.historical_only_chunks)
            .sum(),
        deleted_chunks: scopes.iter().map(|scope| scope.deleted_chunks).sum(),
        physical_chunks: scopes.iter().map(|scope| scope.physical_chunks).sum(),
        embedded_chunks: scopes.iter().map(|scope| scope.embedded_chunks).sum(),
    })
}

fn expected_history_report(
    fixture: &crate::scale_fixture::ValidatedFixture,
    executable: &DescriptorExecutable,
    scopes: Vec<PreparedScopeReceipt>,
    registry_rows: usize,
    evidence: &crate::scale_attest::CorpusEvidence,
) -> Result<PrepareReport, ScalePrepareError> {
    Ok(PrepareReport {
        schema_version: scale_spec::SCHEMA_VERSION,
        preparer: PREPARER_ID.to_owned(),
        fixture_id: scale_spec::FIXTURE_ID.to_owned(),
        profile: fixture.profile(),
        lane: fixture.lane(),
        manifest_hash: scale_spec::manifest_hash(fixture.manifest())?,
        corpus: fixture.root().to_string_lossy().into_owned(),
        binary: BinaryBinding {
            sha256: format!("sha256:{}", executable.immutable_binding().sha256),
            bytes: executable.immutable_binding().bytes,
        },
        scopes,
        registry_rows,
        current_chunks: evidence.current_chunks,
        historical_only_chunks: evidence.historical_only_chunks,
        deleted_chunks: evidence.deleted_chunks,
        physical_chunks: evidence.physical_chunks,
        embedded_chunks: evidence.embedded_chunks,
    })
}

fn publish_report(
    fixture: &crate::scale_fixture::ValidatedFixture,
    executable: &DescriptorExecutable,
    root: &fs::File,
    device: &DeviceBoundary,
    corpus: &Path,
    report: PrepareReport,
    counts: (usize, usize),
) -> Result<PrepareSummary, ScalePrepareError> {
    let (initialized_scopes, indexed_scopes) = counts;
    let mut bytes = canonical_json_bytes(&serde_json::to_value(&report)?).map_err(|error| {
        ScalePrepareError::Input(format!("cannot canonicalize prepare report: {error}"))
    })?;
    bytes.push(b'\n');
    verify_exact_report(&bytes, &report)?;
    match cap_fs::stat(
        root,
        Path::new(PREPARE_REPORT_NAME),
        cap_fs::FollowSymlinks::No,
    ) {
        Ok(_) => {
            verify_exact_report(&read_existing_report(root)?, &report)?;
            ensure_prepare_temp_absent(device.dir("state")?)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            publish_new_report(root, device.dir("state")?, &bytes, corpus)?
        }
        Err(error) => return Err(error.into()),
    }
    let reread = read_existing_report(root)?;
    verify_exact_report(&reread, &report)?;
    fixture.recheck()?;
    device.recheck(corpus)?;
    executable.recheck_original().map_err(process_error)?;
    Ok(PrepareSummary {
        corpus: corpus.to_owned(),
        initialized_scopes,
        indexed_scopes,
        report: corpus.join(PREPARE_REPORT_NAME),
    })
}

fn read_head(kio: &fs::File) -> Result<String, ScalePrepareError> {
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let file = cap_fs::open(kio, Path::new("HEAD"), &options)?;
    let mut bytes = Vec::with_capacity(128);
    use io::Read;
    file.take(257).read_to_end(&mut bytes)?;
    let head = std::str::from_utf8(&bytes)
        .map_err(|_| ScalePrepareError::Input("scope HEAD is not UTF-8".into()))?
        .trim();
    if !kio_core::cas::is_hash(head) {
        return Err(ScalePrepareError::Input(
            "scope HEAD is not a canonical hash".into(),
        ));
    }
    Ok(head.to_owned())
}

fn verify_prepare_report(bytes: &[u8]) -> Result<(), ScalePrepareError> {
    if !bytes.ends_with(b"\n") || bytes.len() > MAX_JSON_BYTES {
        return Err(ScalePrepareError::Input(
            "prepare report is not bounded canonical LF JSON".into(),
        ));
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        ScalePrepareError::Input(format!("prepare report is invalid JSON: {error}"))
    })?;
    let mut canonical = canonical_json_bytes(&value).map_err(|error| {
        ScalePrepareError::Input(format!("cannot canonicalize prepare report: {error}"))
    })?;
    canonical.push(b'\n');
    if canonical != bytes {
        return Err(ScalePrepareError::Input(
            "prepare report is not canonical JCS plus LF".into(),
        ));
    }
    Ok(())
}

fn verify_exact_report(bytes: &[u8], expected: &PrepareReport) -> Result<(), ScalePrepareError> {
    verify_prepare_report(bytes)?;
    let actual: PrepareReport = serde_json::from_slice(bytes).map_err(|error| {
        ScalePrepareError::Input(format!("prepare report violates schema: {error}"))
    })?;
    if &actual != expected {
        return Err(ScalePrepareError::Input(
            "prepare report does not bind current prepared evidence".into(),
        ));
    }
    Ok(())
}

/// Reuse the preparer's own exact receipt validator for a benchmark consumer.
/// Generic JSON parsing is deliberately not an alternative authority here.
pub(crate) fn validate_benchmark_prepare_report(
    fixture: &crate::scale_fixture::ValidatedFixture,
    executable: &DescriptorExecutable,
    scopes: &[crate::scale_attest::ScopeEvidence],
    registry_rows: usize,
) -> Result<Vec<u8>, ScalePrepareError> {
    let root = fixture.try_clone_root()?;
    let expected = expected_report(fixture, executable, scopes, registry_rows)?;
    let bytes = read_existing_report(&root)?;
    verify_exact_report(&bytes, &expected)?;
    Ok(bytes)
}

fn read_existing_report(root: &fs::File) -> Result<Vec<u8>, ScalePrepareError> {
    let metadata = cap_fs::stat(
        root,
        Path::new(PREPARE_REPORT_NAME),
        cap_fs::FollowSymlinks::No,
    )
    .map_err(|error| {
        ScalePrepareError::Input(format!("prepared corpus lacks canonical report: {error}"))
    })?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() as usize > MAX_JSON_BYTES
        || {
            #[cfg(unix)]
            {
                metadata.nlink() != 1
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
    {
        return Err(ScalePrepareError::Input(
            "canonical prepare report is unsafe".into(),
        ));
    }
    let mut options = cap_fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_fs::FollowSymlinks::No);
    let mut file = cap_fs::open(root, Path::new(PREPARE_REPORT_NAME), &options)?;
    let opened = cap_fs::Metadata::from_file(&file)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    use io::Read;
    file.read_to_end(&mut bytes)?;
    let after = cap_fs::stat(
        root,
        Path::new(PREPARE_REPORT_NAME),
        cap_fs::FollowSymlinks::No,
    )?;
    if !opened.is_file()
        || opened.len() != bytes.len() as u64
        || {
            #[cfg(unix)]
            {
                opened.nlink() != 1 || after.nlink() != 1
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
        || !same_directory_file(&metadata, &opened)
        || !same_directory_file(&opened, &after)
    {
        return Err(ScalePrepareError::Input(
            "canonical prepare report changed while reading".into(),
        ));
    }
    Ok(bytes)
}

fn same_directory_file(a: &cap_fs::Metadata, b: &cap_fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(windows)]
    {
        a.volume_serial_number() == b.volume_serial_number() && a.file_index() == b.file_index()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indexed_output() -> Value {
        let tree = format!("sha256:{}", "a".repeat(64));
        let commit = serde_json::json!({
            "commit_type":"auto",
            "created_at":"2026-08-15T00:00:00Z",
            "message":"kio index auto snapshot",
            "object_type":"commit",
            "parent":null,
            "stats":{"files_added":1,"files_modified":0,"files_deleted":0},
            "tool_lock_hash":format!("sha256:{}", "c".repeat(64)),
            "tree":tree,
        });
        let commit_hash = canonical_value_hash(&commit).unwrap();
        serde_json::json!({
            "status":"indexed", "approval_method":"yes", "network_allowed":false,
            "network_opt_in":false, "pending_online_tasks":0, "paused_tasks":0,
            "pending_files":0, "failed_files":0, "normalized_files":1,
            "skipped_oversized_files":0,
            "skipped_unrecognized_binary_files":0, "embedding_tasks_executed":0,
            "embedding_tasks_failed":0,
            "tree_hash":tree,
            "commit_hash":commit_hash,
            "commit":commit, "budget_warning":null, "skipped_units":[],
            "child_scopes":[],
            "child_scope_discovery": {
                "page_size": kio_pipeline::scan::CHILD_SCOPE_DISCOVERY_PAGE_SIZE,
                "has_more": false,
                "continuation_committed": true,
                "restarted_from_root": false,
                "rows_total": 0,
                "rows_omitted": 0,
            },
            "gc": {"mode":"manual_only","reason":"manual_only","status":"disabled","trigger":"index"}
        })
    }

    fn noop_output() -> Value {
        let mut output = indexed_output();
        output["status"] = serde_json::json!("noop");
        output["commit_hash"] = Value::Null;
        output["commit"] = Value::Null;
        output
    }

    #[cfg(unix)]
    #[test]
    fn existing_report_rejects_hardlink_alias() {
        let temp = tempfile::tempdir().unwrap();
        let report = temp.path().join(PREPARE_REPORT_NAME);
        std::fs::write(&report, b"{}\n").unwrap();
        std::fs::hard_link(&report, temp.path().join("alias")).unwrap();
        let root = std::fs::File::open(temp.path()).unwrap();
        assert!(read_existing_report(&root).is_err());
    }

    #[test]
    fn index_parser_rejects_network_and_missing_counts() {
        assert!(
            validate_index_output(
                &indexed_output(),
                IndexExpectation::Repair {
                    files: 1,
                    embeddings: 0,
                },
            )
            .is_ok()
        );
        assert!(
            validate_index_output(&noop_output(), IndexExpectation::RegistryNoop { files: 1 })
                .is_ok()
        );
        assert!(
            validate_index_output(
                &serde_json::json!({"status":"indexed","network_allowed":true}),
                IndexExpectation::Repair {
                    files: 1,
                    embeddings: 0,
                }
            )
            .is_err()
        );
        assert!(validate_index_output(&serde_json::json!({"status":"failed","network_allowed":false,"failed_files":0,"pending_files":0,"pending_online_tasks":0,"paused_tasks":0,"embedding_tasks_failed":0,"normalized_files":1,"commit_hash":"x"}), IndexExpectation::Repair { files: 1, embeddings: 0 }).is_err());
    }

    #[test]
    fn index_parser_accepts_completed_empty_discovery_after_restart_or_noop() {
        for restarted in [false, true] {
            for (mut output, expected) in [
                (
                    indexed_output(),
                    IndexExpectation::Repair {
                        files: 1,
                        embeddings: 0,
                    },
                ),
                (noop_output(), IndexExpectation::RegistryNoop { files: 1 }),
            ] {
                output["child_scope_discovery"]["restarted_from_root"] =
                    serde_json::json!(restarted);
                assert!(validate_index_output(&output, expected).is_ok());
            }
        }
    }

    #[test]
    fn index_parser_rejects_incomplete_or_malformed_discovery() {
        let expected = IndexExpectation::Repair {
            files: 1,
            embeddings: 0,
        };
        for (field, value) in [
            ("page_size", serde_json::json!(0)),
            ("page_size", serde_json::json!(129)),
            ("page_size", serde_json::json!("128")),
            ("has_more", serde_json::json!(true)),
            ("has_more", serde_json::json!(0)),
            ("continuation_committed", serde_json::json!(false)),
            ("restarted_from_root", serde_json::json!(null)),
            ("restarted_from_root", serde_json::json!("false")),
            ("rows_total", serde_json::json!(1)),
            ("rows_omitted", serde_json::json!(1)),
            ("rows_total", serde_json::json!(-1)),
            ("rows_omitted", serde_json::json!(0.5)),
            ("preview", serde_json::json!(true)),
            ("unknown", serde_json::json!(false)),
        ] {
            let mut output = indexed_output();
            output["child_scope_discovery"][field] = value;
            assert!(
                validate_index_output(&output, expected.clone()).is_err(),
                "{field}"
            );
        }
        for missing in [
            "page_size",
            "has_more",
            "continuation_committed",
            "restarted_from_root",
            "rows_total",
            "rows_omitted",
        ] {
            let mut output = indexed_output();
            output["child_scope_discovery"]
                .as_object_mut()
                .unwrap()
                .remove(missing);
            assert!(
                validate_index_output(&output, expected.clone()).is_err(),
                "missing {missing}"
            );
        }
        let mut missing = indexed_output();
        missing
            .as_object_mut()
            .unwrap()
            .remove("child_scope_discovery");
        assert!(validate_index_output(&missing, expected.clone()).is_err());
        let mut null = indexed_output();
        null["child_scope_discovery"] = Value::Null;
        assert!(validate_index_output(&null, expected).is_err());
    }

    #[test]
    fn index_parser_rejects_partial_auth_budget_and_unknown_output() {
        for (key, value) in [
            ("failed_files", serde_json::json!(1)),
            ("pending_online_tasks", serde_json::json!(1)),
            ("budget_warning", serde_json::json!({"kind":"budget"})),
            ("error_code", serde_json::json!("KIO-E-ADAPTER-AUTH-001")),
        ] {
            let mut output = indexed_output();
            output[key] = value;
            assert!(
                validate_index_output(
                    &output,
                    IndexExpectation::Repair {
                        files: 1,
                        embeddings: 0,
                    },
                )
                .is_err(),
                "{key}"
            );
        }
    }

    #[test]
    fn offline_index_arguments_are_frozen() {
        assert_eq!(
            OFFLINE_INDEX_ARGS,
            ["--json", "index", "--offline", "--yes"]
        );
    }

    #[test]
    fn prepare_report_verifier_rejects_noncanonical_and_non_lf() {
        assert!(verify_prepare_report(br#"{}"#).is_err());
        assert!(verify_prepare_report(b"{\"z\":1,\"a\":2}\n").is_err());
        assert!(verify_prepare_report(b"{\"a\":2,\"z\":1}\n").is_ok());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn prepare_report_publication_is_atomic_create_only_and_recovers_exact_temp() {
        let temp = tempfile::tempdir().unwrap();
        let corpus = temp.path().join("corpus");
        let state_path = temp.path().join("state");
        std::fs::create_dir(&corpus).unwrap();
        std::fs::create_dir(&state_path).unwrap();
        let root = std::fs::File::open(&corpus).unwrap();
        let state = std::fs::File::open(&state_path).unwrap();
        let bytes = b"{\"schema_version\":2}\n";

        publish_new_report(&root, &state, bytes, &corpus).unwrap();
        assert_eq!(
            std::fs::read(corpus.join(PREPARE_REPORT_NAME)).unwrap(),
            bytes
        );
        assert!(!state_path.join(PREPARE_REPORT_TEMP_NAME).exists());

        let second = temp.path().join("second");
        let second_state = temp.path().join("second-state");
        std::fs::create_dir(&second).unwrap();
        std::fs::create_dir(&second_state).unwrap();
        std::fs::write(second_state.join(PREPARE_REPORT_TEMP_NAME), bytes).unwrap();
        let second_root = std::fs::File::open(&second).unwrap();
        let second_state_handle = std::fs::File::open(&second_state).unwrap();
        publish_new_report(&second_root, &second_state_handle, bytes, &second).unwrap();
        assert_eq!(
            std::fs::read(second.join(PREPARE_REPORT_NAME)).unwrap(),
            bytes
        );

        let third = temp.path().join("third");
        let third_state = temp.path().join("third-state");
        std::fs::create_dir(&third).unwrap();
        std::fs::create_dir(&third_state).unwrap();
        std::fs::write(third_state.join(PREPARE_REPORT_TEMP_NAME), b"torn").unwrap();
        let third_root = std::fs::File::open(&third).unwrap();
        let third_state_handle = std::fs::File::open(&third_state).unwrap();
        assert!(publish_new_report(&third_root, &third_state_handle, bytes, &third).is_err());
        assert_eq!(
            std::fs::read(third_state.join(PREPARE_REPORT_TEMP_NAME)).unwrap(),
            b"torn"
        );
        assert!(!third.join(PREPARE_REPORT_NAME).exists());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn unsupported_platform_does_not_create_private_device() {
        let temp = tempfile::tempdir().unwrap();
        let corpus = temp.path().join("corpus");
        std::fs::create_dir(&corpus).unwrap();
        assert!(prepare(&corpus, &temp.path().join("missing-bin")).is_err());
        assert!(!corpus.join(DEVICE_DIR_NAME).exists());
    }
}
