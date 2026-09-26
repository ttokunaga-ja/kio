//! Rust-owned, fail-closed reconstruction of the frozen history corpus.

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    env,
    path::{Path, PathBuf},
};

use kio_core::cas::hash_bytes;
use serde_json::Value;
use thiserror::Error;

use crate::{
    manifest::{self, CorpusManifest, HistoryOperation, VerifiedHistory},
    replay_boundary::{
        DirectEntry, DirectEntryKind, InitializedScopeAuthority, ReplayBoundary,
        ReplayBoundaryError, ReplayDevice, ReplayScope,
    },
    runner::{
        BoundedProcessError, BoundedProcessOptions, BoundedProcessOutput, run_bounded_command,
    },
};

const CORPUS_MANIFEST: &str = "corpus-manifest.json";
const MAX_OBSERVED_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REPLAY_SUBPROCESSES: usize = 62;

#[derive(Debug, Error)]
pub enum ReplayError {
    #[error(transparent)]
    Manifest(#[from] manifest::ManifestError),
    #[error(transparent)]
    Boundary(#[from] ReplayBoundaryError),
    #[error(transparent)]
    Process(#[from] BoundedProcessError),
    #[error("history replay rejected {command}: {reason}")]
    Result {
        command: &'static str,
        reason: String,
    },
    #[error("history replay input is unsafe: {0}")]
    Input(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaySummary {
    pub scopes: usize,
    pub commits: usize,
    pub manifest: PathBuf,
}

/// Reconstruct the only accepted history fixture. The plan is bundled; callers
/// cannot provide a plan or a destination manifest.
pub fn replay_history(corpus: &Path, bin: &Path) -> Result<ReplaySummary, ReplayError> {
    ReplayBoundary::preflight_platform()?;
    let plan = manifest::frozen_history_plan()?;
    let bin = absolute_path(bin)?;
    let boundary = ReplayBoundary::bind(corpus, &plan.scopes)?;
    let corpus_bytes = boundary.read_root_file(
        CORPUS_MANIFEST,
        sha_without_prefix(manifest::CORPUS_MANIFEST_SHA256),
    )?;
    let source = manifest::parse_corpus_manifest_bytes(&corpus_bytes)?;
    manifest::validate_corpus_manifest(&source)?;
    validate_plan_sources(&plan.operations, &source)?;
    let mut expected = expected_files(&source)?;
    verify_initial_tree(&boundary, &expected)?;
    validate_edit_derivability(&boundary, &plan.operations)?;
    // Resolve relative command paths once, before device creation. The boundary
    // rejects links and snapshots the exact regular executable afterwards.
    let device = boundary.prepare_device()?;
    let executable = device.snapshot_executable(&bin)?;
    let command_context = ReplayCommandContext {
        boundary: &boundary,
        device: &device,
        executable: &executable,
        subprocesses: Cell::new(0),
    };
    let mut verified = BTreeMap::new();
    let mut initialized = BTreeSet::new();
    let mut initialized_authorities = BTreeMap::new();
    for scope_name in &plan.scopes {
        let scope = boundary
            .scope(scope_name)
            .ok_or_else(|| ReplayError::Input(format!("missing bound scope: {scope_name}")))?;
        command_context.run(
            "init",
            scope,
            &["--json", "init", "."],
            &expected,
            &initialized,
            &initialized_authorities,
        )?;
        initialized.insert(scope_name.clone());
        validate_scope_tree(scope, &expected, scope_name, true)?;
        let authority = scope.bind_initialized_authority()?;
        authority.recheck()?;
        if initialized_authorities
            .insert(scope_name.clone(), authority)
            .is_some()
        {
            return Err(ReplayError::Input(
                "duplicate initialized scope authority".into(),
            ));
        }
        verify_active(
            &boundary,
            &device,
            &executable,
            &expected,
            &initialized,
            &initialized_authorities,
        )?;
        command_context.run(
            "index",
            scope,
            &["--json", "index", "--yes", "--offline"],
            &expected,
            &initialized,
            &initialized_authorities,
        )?;
        command_context.run(
            "snapshot",
            scope,
            &["--json", "snapshot", "create", "-m", "baseline"],
            &expected,
            &initialized,
            &initialized_authorities,
        )?;
        for (phase, command_label) in [(0_u8, "edit"), (1, "rename"), (2, "delete")] {
            let ops = plan
                .operations
                .iter()
                .filter(|operation| {
                    operation_scope(operation) == scope_name && operation_phase(operation) == phase
                })
                .collect::<Vec<_>>();
            if ops.is_empty() {
                continue;
            }
            for operation in &ops {
                apply_operation(operation, scope, &device, &mut expected)?;
            }
            verify_active(
                &boundary,
                &device,
                &executable,
                &expected,
                &initialized,
                &initialized_authorities,
            )?;
            let message = operation_message(command_label, &ops);
            command_context.run(
                "index",
                scope,
                &["--json", "index", "--yes", "--offline"],
                &expected,
                &initialized,
                &initialized_authorities,
            )?;
            command_context.run(
                "snapshot",
                scope,
                &["--json", "snapshot", "create", "-m", &message],
                &expected,
                &initialized,
                &initialized_authorities,
            )?;
        }
        let output = command_context.run(
            "log",
            scope,
            &["--json", "log"],
            &expected,
            &initialized,
            &initialized_authorities,
        )?;
        let history = validate_log(
            &output,
            scope_name,
            expected_commit_count(&plan.operations, scope_name),
        )?;
        verified.insert(scope_name.clone(), history);
    }
    let history = manifest::build_history_manifest(verified)?;
    let bytes = manifest::serialize_history_manifest(&history)?;
    // Parse our own result before the irreversible, create-only publication.
    manifest::parse_history_manifest_bytes(&bytes, &source)?;
    verify_active(
        &boundary,
        &device,
        &executable,
        &expected,
        &initialized,
        &initialized_authorities,
    )?;
    if command_context.subprocesses.get() != MAX_REPLAY_SUBPROCESSES {
        return Err(ReplayError::Input(
            "unexpected replay subprocess count".into(),
        ));
    }
    let staged = device.stage_history_manifest(&boundary, &bytes)?;
    verify_active(
        &boundary,
        &device,
        &executable,
        &expected,
        &initialized,
        &initialized_authorities,
    )?;
    staged.publish()?;
    Ok(ReplaySummary {
        scopes: plan.scopes.len(),
        commits: 48,
        manifest: boundary.public_root().join("history-manifest.json"),
    })
}

fn sha_without_prefix(value: &str) -> &str {
    value.strip_prefix("sha256:").unwrap_or(value)
}
fn absolute_path(path: &Path) -> Result<PathBuf, ReplayError> {
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    Ok(env::current_dir()
        .map_err(|e| ReplayError::Input(format!("cannot resolve --bin: {e}")))?
        .join(path))
}

fn validate_plan_sources(
    operations: &[HistoryOperation],
    source: &CorpusManifest,
) -> Result<(), ReplayError> {
    let originals = source
        .files
        .iter()
        .map(|file| ((file.scope.as_str(), file.file.as_str()), file))
        .collect::<BTreeMap<_, _>>();
    for operation in operations {
        let (scope, file, hash, sections) = match operation {
            HistoryOperation::Edit {
                scope,
                file,
                before_raw_sha256,
                sections,
                ..
            }
            | HistoryOperation::Delete {
                scope,
                file,
                before_raw_sha256,
                sections,
                ..
            } => (scope, file, before_raw_sha256, sections),
            HistoryOperation::Rename {
                scope,
                old_file,
                before_raw_sha256,
                sections,
                ..
            } => (scope, old_file, before_raw_sha256, sections),
        };
        let Some(source) = originals.get(&(scope.as_str(), file.as_str())) else {
            return Err(ReplayError::Input(format!(
                "plan source missing from corpus: {scope}/{file}"
            )));
        };
        if !source.anchor || source.raw_sha256 != *hash || source.sections != *sections {
            return Err(ReplayError::Input(format!(
                "plan source does not bind current anchor: {scope}/{file}"
            )));
        }
    }
    Ok(())
}

fn validate_edit_derivability(
    boundary: &ReplayBoundary,
    operations: &[HistoryOperation],
) -> Result<(), ReplayError> {
    for operation in operations {
        let HistoryOperation::Edit {
            scope,
            file,
            old_value,
            new_value,
            before_raw_sha256,
            after_raw_sha256,
            ..
        } = operation
        else {
            continue;
        };
        let scope = boundary
            .scope(scope)
            .ok_or_else(|| ReplayError::Input("edit scope unavailable".into()))?;
        let bytes = scope.read_file(file, before_raw_sha256)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| ReplayError::Input(format!("edit source is not UTF-8: {file}")))?;
        if text.matches(old_value).count() != 1 {
            return Err(ReplayError::Input(format!(
                "edit old value must occur once: {file}"
            )));
        }
        let replaced = text.replacen(old_value, new_value, 1).into_bytes();
        if sha_without_prefix(&hash_bytes(&replaced)) != after_raw_sha256 {
            return Err(ReplayError::Input(format!(
                "edit post-hash is not derivable: {file}"
            )));
        }
    }
    Ok(())
}

type ExpectedFiles = BTreeMap<(String, String), String>;
fn expected_files(source: &CorpusManifest) -> Result<ExpectedFiles, ReplayError> {
    let mut output = BTreeMap::new();
    for file in &source.files {
        if output
            .insert(
                (file.scope.clone(), file.file.clone()),
                file.raw_sha256.clone(),
            )
            .is_some()
        {
            return Err(ReplayError::Input("duplicate corpus source file".into()));
        }
    }
    Ok(output)
}
fn expected_entries(expected: &ExpectedFiles, scope: &str, initialized: bool) -> Vec<DirectEntry> {
    let mut entries = expected
        .iter()
        .filter(|((candidate, _), _)| candidate == scope)
        .map(|((_, name), hash)| DirectEntry {
            name: name.clone(),
            kind: DirectEntryKind::RegularFile,
            binding: Some(crate::replay_boundary::FileBinding {
                sha256: hash.clone(),
                bytes: 0,
            }),
        })
        .collect::<Vec<_>>();
    // Bytes are checked separately: DirectEntry includes byte length, while the
    // frozen source manifest deliberately only records its content hash.
    if initialized {
        entries.push(DirectEntry {
            name: ".kio".into(),
            kind: DirectEntryKind::Directory,
            binding: None,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}
fn validate_scope_tree(
    scope: &ReplayScope,
    expected: &ExpectedFiles,
    name: &str,
    initialized: bool,
) -> Result<u64, ReplayError> {
    let observed = scope.direct_entries()?;
    let bytes = observed
        .iter()
        .filter_map(|entry| entry.binding.as_ref().map(|binding| binding.bytes))
        .sum::<u64>();
    if bytes > MAX_OBSERVED_SOURCE_BYTES {
        return Err(ReplayError::Input(format!(
            "source byte bound exceeded in {name}"
        )));
    }
    let wanted = expected_entries(expected, name, initialized);
    if observed.len() != wanted.len() {
        return Err(ReplayError::Input(format!(
            "unexpected working-tree entries in {name}"
        )));
    }
    for (got, want) in observed.iter().zip(&wanted) {
        if got.name != want.name
            || got.kind != want.kind
            || (got.kind == DirectEntryKind::RegularFile
                && got.binding.as_ref().map(|b| &b.sha256)
                    != want.binding.as_ref().map(|b| &b.sha256))
        {
            return Err(ReplayError::Input(format!(
                "working tree identity mismatch in {name}/{}",
                got.name
            )));
        }
    }
    Ok(bytes)
}
fn verify_initial_tree(
    boundary: &ReplayBoundary,
    expected: &ExpectedFiles,
) -> Result<(), ReplayError> {
    let mut root = boundary.root_entries()?;
    root.sort_by(|a, b| a.name.cmp(&b.name));
    let mut names = boundary
        .scopes()
        .iter()
        .map(|scope| scope.name().to_owned())
        .collect::<BTreeSet<_>>();
    names.insert(CORPUS_MANIFEST.into());
    if root
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<BTreeSet<_>>()
        != names.iter().map(String::as_str).collect()
    {
        return Err(ReplayError::Input(
            "corpus root has unexpected direct entries".into(),
        ));
    }
    let mut total_bytes = 0_u64;
    for scope in boundary.scopes() {
        total_bytes = total_bytes
            .checked_add(validate_scope_tree(scope, expected, scope.name(), false)?)
            .ok_or_else(|| ReplayError::Input("source byte accounting overflow".into()))?;
        if total_bytes > MAX_OBSERVED_SOURCE_BYTES {
            return Err(ReplayError::Input(
                "corpus source byte bound exceeded".into(),
            ));
        }
    }
    Ok(())
}
fn verify_active(
    boundary: &ReplayBoundary,
    device: &ReplayDevice,
    executable: &crate::replay_boundary::BoundExecutable,
    expected: &ExpectedFiles,
    initialized: &BTreeSet<String>,
    initialized_authorities: &BTreeMap<String, InitializedScopeAuthority>,
) -> Result<(), ReplayError> {
    manifest::frozen_history_plan()?;
    boundary.recheck_active()?;
    device.recheck()?;
    executable.recheck_original()?;
    for authority in initialized_authorities.values() {
        authority.recheck()?;
    }
    let bytes = boundary.read_root_file(
        CORPUS_MANIFEST,
        sha_without_prefix(manifest::CORPUS_MANIFEST_SHA256),
    )?;
    let source = manifest::parse_corpus_manifest_bytes(&bytes)?;
    manifest::validate_corpus_manifest(&source)?;
    let expected_root = boundary
        .scopes()
        .iter()
        .map(|scope| scope.name())
        .chain([CORPUS_MANIFEST, ".kio-eval-device"])
        .collect::<BTreeSet<_>>();
    let root_entries = boundary.root_entries()?;
    let observed_root = root_entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<BTreeSet<_>>();
    if observed_root != expected_root {
        return Err(ReplayError::Input(
            "active corpus root has unexpected direct entries".into(),
        ));
    }
    let mut total_bytes = 0_u64;
    for scope in boundary.scopes() {
        total_bytes = total_bytes
            .checked_add(validate_scope_tree(
                scope,
                expected,
                scope.name(),
                initialized.contains(scope.name()),
            )?)
            .ok_or_else(|| ReplayError::Input("source byte accounting overflow".into()))?;
        if total_bytes > MAX_OBSERVED_SOURCE_BYTES {
            return Err(ReplayError::Input(
                "corpus source byte bound exceeded".into(),
            ));
        }
    }
    Ok(())
}

struct ReplayCommandContext<'a> {
    boundary: &'a ReplayBoundary,
    device: &'a ReplayDevice,
    executable: &'a crate::replay_boundary::BoundExecutable,
    subprocesses: Cell<usize>,
}
impl ReplayCommandContext<'_> {
    fn run(
        &self,
        command_name: &'static str,
        scope: &ReplayScope,
        args: &[&str],
        expected: &ExpectedFiles,
        initialized: &BTreeSet<String>,
        initialized_authorities: &BTreeMap<String, InitializedScopeAuthority>,
    ) -> Result<BoundedProcessOutput, ReplayError> {
        verify_active(
            self.boundary,
            self.device,
            self.executable,
            expected,
            initialized,
            initialized_authorities,
        )?;
        scope.recheck_after_command()?;
        let mut command = self.executable.command()?;
        command.args(args);
        self.device.configure_hermetic_environment(&mut command)?;
        scope.configure_command_cwd(&mut command)?;
        let next = self
            .subprocesses
            .get()
            .checked_add(1)
            .ok_or_else(|| ReplayError::Input("replay subprocess counter overflow".into()))?;
        if next > MAX_REPLAY_SUBPROCESSES {
            return Err(ReplayError::Input(
                "replay subprocess bound exceeded".into(),
            ));
        }
        self.subprocesses.set(next);
        let output = run_bounded_command(&mut command, BoundedProcessOptions::default(), None)?;
        if !output.status.success() {
            return Err(ReplayError::Result {
                command: command_name,
                reason: format!("exit status {}", output.status),
            });
        }
        validate_result(command_name, &output, args.last().copied())?;
        scope.recheck_after_command()?;
        self.boundary.recheck_after_command()?;
        let mut post_initialized = initialized.clone();
        if command_name == "init" {
            post_initialized.insert(scope.name().to_owned());
        }
        verify_active(
            self.boundary,
            self.device,
            self.executable,
            expected,
            &post_initialized,
            initialized_authorities,
        )?;
        Ok(output)
    }
}
fn object(output: &BoundedProcessOutput, command: &'static str) -> Result<Value, ReplayError> {
    let value: Value =
        serde_json::from_str(output.stdout.trim()).map_err(|e| ReplayError::Result {
            command,
            reason: format!("invalid JSON: {e}"),
        })?;
    if !value.is_object() {
        return Err(ReplayError::Result {
            command,
            reason: "JSON result must be an object".into(),
        });
    }
    Ok(value)
}
fn validate_result(
    command: &'static str,
    output: &BoundedProcessOutput,
    requested_message: Option<&str>,
) -> Result<(), ReplayError> {
    if !output.stderr.is_empty() {
        return Err(ReplayError::Result {
            command,
            reason: format!(
                "successful command emitted stderr: {}",
                output.stderr.escape_default()
            ),
        });
    }
    let value = object(output, command)?;
    let status = value.get("status").and_then(Value::as_str);
    match command {
        "init" if status == Some("initialized") => Ok(()),
        "index" if status == Some("indexed") => {
            if value.get("failed_files").and_then(Value::as_u64) != Some(0)
                || value.get("network_allowed").and_then(Value::as_bool) != Some(false)
                || value.get("network_opt_in").and_then(Value::as_bool) != Some(false)
                || value.pointer("/commit/commit_type").and_then(Value::as_str) != Some("auto")
                || value.pointer("/commit/message").and_then(Value::as_str)
                    != Some("kio index auto snapshot")
            {
                return Err(ReplayError::Result {
                    command,
                    reason: "index result is partial or network-enabled".into(),
                });
            }
            Ok(())
        }
        "snapshot"
            if status == Some("created")
                && value.pointer("/commit/commit_type").and_then(Value::as_str)
                    == Some("manual")
                && value.pointer("/commit/message").and_then(Value::as_str)
                    == requested_message =>
        {
            Ok(())
        }
        "log"
            if value.get("commits").and_then(Value::as_array).is_some()
                && value.get("truncated").and_then(Value::as_bool) == Some(false) =>
        {
            Ok(())
        }
        _ => Err(ReplayError::Result {
            command,
            reason: "unexpected JSON result".into(),
        }),
    }
}

fn operation_scope(operation: &HistoryOperation) -> &str {
    match operation {
        HistoryOperation::Edit { scope, .. }
        | HistoryOperation::Rename { scope, .. }
        | HistoryOperation::Delete { scope, .. } => scope,
    }
}
fn operation_phase(operation: &HistoryOperation) -> u8 {
    match operation {
        HistoryOperation::Edit { .. } => 0,
        HistoryOperation::Rename { .. } => 1,
        HistoryOperation::Delete { .. } => 2,
    }
}
fn operation_message(phase: &str, operations: &[&HistoryOperation]) -> String {
    format!(
        "{phase}: {}",
        operations
            .iter()
            .map(|operation| match operation {
                HistoryOperation::Edit { file, .. } | HistoryOperation::Delete { file, .. } =>
                    file.clone(),
                HistoryOperation::Rename {
                    old_file, new_file, ..
                } => format!("{old_file}->{new_file}"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    )
}
fn apply_operation(
    operation: &HistoryOperation,
    scope: &ReplayScope,
    device: &ReplayDevice,
    expected: &mut ExpectedFiles,
) -> Result<(), ReplayError> {
    match operation {
        HistoryOperation::Edit {
            scope: name,
            file,
            old_value,
            new_value,
            before_raw_sha256,
            after_raw_sha256,
            ..
        } => {
            scope.edit_text(
                file,
                old_value,
                new_value,
                before_raw_sha256,
                after_raw_sha256,
            )?;
            expected.insert((name.clone(), file.clone()), after_raw_sha256.clone());
        }
        HistoryOperation::Rename {
            scope: name,
            old_file,
            new_file,
            before_raw_sha256,
            ..
        } => {
            scope.rename(old_file, new_file, before_raw_sha256)?;
            expected.remove(&(name.clone(), old_file.clone()));
            expected.insert((name.clone(), new_file.clone()), before_raw_sha256.clone());
        }
        HistoryOperation::Delete {
            scope: name,
            file,
            before_raw_sha256,
            ..
        } => {
            device.delete_verified(scope, file, before_raw_sha256)?;
            expected.remove(&(name.clone(), file.clone()));
        }
    }
    Ok(())
}
fn expected_commit_count(operations: &[HistoryOperation], scope: &str) -> usize {
    2 + operations
        .iter()
        .filter(|item| operation_scope(item) == scope)
        .map(operation_phase)
        .collect::<BTreeSet<_>>()
        .len()
        * 2
}
fn validate_log(
    output: &BoundedProcessOutput,
    scope: &str,
    commit_count: usize,
) -> Result<VerifiedHistory, ReplayError> {
    let value = object(output, "log")?;
    let commits = value
        .get("commits")
        .and_then(Value::as_array)
        .ok_or_else(|| ReplayError::Result {
            command: "log",
            reason: "commits missing".into(),
        })?;
    let messages = commits
        .iter()
        .map(|commit| {
            commit
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| ReplayError::Result {
            command: "log",
            reason: "commit message missing".into(),
        })?;
    if commits.len() != commit_count {
        return Err(ReplayError::Result {
            command: "log",
            reason: format!("{scope} expected {commit_count} commits"),
        });
    }
    let plan = manifest::frozen_history_plan()?;
    let expected = expected_log_messages(&plan.operations, scope);
    if messages != expected {
        return Err(ReplayError::Result {
            command: "log",
            reason: "commit message sequence mismatch".into(),
        });
    }
    for (index, commit) in commits.iter().enumerate() {
        let expected_type = if index % 2 == 0 { "manual" } else { "auto" };
        if commit.get("commit_type").and_then(Value::as_str) != Some(expected_type) {
            return Err(ReplayError::Result {
                command: "log",
                reason: "commit type sequence mismatch".into(),
            });
        }
        if commit.get("object_type").and_then(Value::as_str) != Some("commit") {
            return Err(ReplayError::Result {
                command: "log",
                reason: "commit object type mismatch".into(),
            });
        }
        let hash = commit
            .get("commit_hash")
            .and_then(Value::as_str)
            .ok_or_else(|| ReplayError::Result {
                command: "log",
                reason: "commit hash missing".into(),
            })?;
        let tree =
            commit
                .get("tree")
                .and_then(Value::as_str)
                .ok_or_else(|| ReplayError::Result {
                    command: "log",
                    reason: "tree hash missing".into(),
                })?;
        if !canonical_sha256(hash) || !canonical_sha256(tree) {
            return Err(ReplayError::Result {
                command: "log",
                reason: "noncanonical commit or tree hash".into(),
            });
        }
        let expected_parent = if index + 1 == commits.len() {
            None
        } else {
            Some(
                commits[index + 1]
                    .get("commit_hash")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        };
        let parent = commit
            .get("parent")
            .and_then(|parent| {
                if parent.is_null() {
                    Some(None)
                } else {
                    parent.as_str().map(Some)
                }
            })
            .ok_or_else(|| ReplayError::Result {
                command: "log",
                reason: "parent missing".into(),
            })?;
        if parent != expected_parent {
            return Err(ReplayError::Result {
                command: "log",
                reason: "parent chain mismatch".into(),
            });
        }
    }
    let hashes = commits
        .iter()
        .filter_map(|commit| commit.get("commit_hash").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    if hashes.len() != commits.len() {
        return Err(ReplayError::Result {
            command: "log",
            reason: "duplicate commit hash".into(),
        });
    }
    let mut steps = vec!["baseline".to_owned()];
    for (phase, name) in [(0, "edit"), (1, "rename"), (2, "delete")] {
        if plan
            .operations
            .iter()
            .any(|op| operation_scope(op) == scope && operation_phase(op) == phase)
        {
            steps.push(name.into());
        }
    }
    Ok(VerifiedHistory {
        steps,
        commit_count,
        messages,
    })
}
fn canonical_sha256(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn expected_log_messages(operations: &[HistoryOperation], scope: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (phase, name) in [(2, "delete"), (1, "rename"), (0, "edit")] {
        let ops = operations
            .iter()
            .filter(|op| operation_scope(op) == scope && operation_phase(op) == phase)
            .collect::<Vec<_>>();
        if !ops.is_empty() {
            out.push(operation_message(name, &ops));
            out.push("kio index auto snapshot".into());
        }
    }
    out.push("baseline".into());
    out.push("kio index auto snapshot".into());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::fs;
    use std::process::Command;
    fn log_output(mut commits: Vec<Value>) -> BoundedProcessOutput {
        for index in 0..commits.len() {
            let parent =
                (index + 1 < commits.len()).then(|| commits[index + 1]["commit_hash"].clone());
            commits[index]["parent"] = parent.unwrap_or(serde_json::Value::Null);
        }
        BoundedProcessOutput {
            status: Command::new("true").status().unwrap(),
            stdout: serde_json::json!({"commits": commits, "truncated": false}).to_string(),
            stderr: String::new(),
            duration: std::time::Duration::ZERO,
        }
    }
    fn valid_research_log() -> BoundedProcessOutput {
        let plan = manifest::frozen_history_plan().unwrap();
        let messages = expected_log_messages(&plan.operations, "research");
        let commits = messages.into_iter().enumerate().map(|(index, message)| serde_json::json!({
            "object_type":"commit", "commit_hash":format!("sha256:{index:064x}"), "tree":format!("sha256:{:064x}", index + 100),
            "commit_type": if index % 2 == 0 { "manual" } else { "auto" }, "message":message
        })).collect();
        log_output(commits)
    }
    #[test]
    fn index_rejects_network_or_partial() {
        let status = Command::new("true").status().unwrap();
        let out = BoundedProcessOutput { status, stdout: r#"{"status":"indexed","failed_files":0,"network_allowed":false,"network_opt_in":false,"commit":{"commit_type":"auto","message":"kio index auto snapshot"}}"#.into(), stderr: String::new(), duration: std::time::Duration::ZERO };
        assert!(validate_result("index", &out, None).is_ok());
        let out = BoundedProcessOutput { stdout: r#"{"status":"indexed","failed_files":1,"network_allowed":false,"network_opt_in":false,"commit":{"commit_type":"auto","message":"kio index auto snapshot"}}"#.into(), ..out };
        assert!(validate_result("index", &out, None).is_err());
    }

    #[test]
    fn successful_replay_command_rejects_stderr() {
        let out = BoundedProcessOutput {
            status: Command::new("true").status().unwrap(),
            stdout: r#"{"status":"initialized"}"#.into(),
            stderr: "degraded cache write\n".into(),
            duration: std::time::Duration::ZERO,
        };
        assert!(matches!(
            validate_result("init", &out, None),
            Err(ReplayError::Result { reason, .. }) if reason.contains("degraded cache write\\n")
        ));
    }
    #[test]
    fn malformed_json_is_rejected() {
        let status = Command::new("true").status().unwrap();
        let out = BoundedProcessOutput {
            status,
            stdout: "not-json".into(),
            stderr: String::new(),
            duration: std::time::Duration::ZERO,
        };
        assert!(validate_result("init", &out, None).is_err());
    }
    #[test]
    fn raw_hash_comparison_uses_the_plan_representation() {
        let plan = manifest::frozen_history_plan().unwrap();
        let edit = plan
            .operations
            .iter()
            .find_map(|op| match op {
                HistoryOperation::Edit {
                    after_raw_sha256, ..
                } => Some(after_raw_sha256),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            sha_without_prefix(&hash_bytes(b"replay")),
            "ac203c9843b5bd8c883e07039ff82820c94422010be6108bb82403ca25376a22"
        );
        assert_eq!(edit.len(), 64);
    }
    #[test]
    fn log_rejects_broken_dag_and_wire_identity() {
        let good = valid_research_log();
        assert!(validate_log(&good, "research", 8).is_ok());
        let mut value: Value = serde_json::from_str(&good.stdout).unwrap();
        value["commits"][0]["parent"] = serde_json::Value::Null;
        let broken = BoundedProcessOutput {
            stdout: value.to_string(),
            ..good.clone()
        };
        assert!(validate_log(&broken, "research", 8).is_err());
        let mut value: Value = serde_json::from_str(&good.stdout).unwrap();
        value["commits"][0]["commit_hash"] = value["commits"][1]["commit_hash"].clone();
        let duplicate = BoundedProcessOutput {
            stdout: value.to_string(),
            ..good.clone()
        };
        assert!(validate_log(&duplicate, "research", 8).is_err());
        let mut value: Value = serde_json::from_str(&good.stdout).unwrap();
        value["commits"][0]["object_type"] = serde_json::json!("tree");
        let invalid_type = BoundedProcessOutput {
            stdout: value.to_string(),
            ..good
        };
        assert!(validate_log(&invalid_type, "research", 8).is_err());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn failing_child_never_publishes_a_history_manifest() {
        let temporary = tempfile::tempdir().unwrap();
        let corpus = temporary.path().join("corpus");
        crate::generator::generate_corpus(&corpus, false).unwrap();
        let false_bin = Path::new("/usr/bin/false");
        assert!(replay_history(&corpus, false_bin).is_err());
        assert!(!corpus.join("history-manifest.json").exists());
        let second = replay_history(&corpus, false_bin).unwrap_err().to_string();
        assert!(second.contains("fresh corpus already contains replay state"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn successful_child_stderr_never_publishes_a_history_manifest() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let corpus = temporary.path().join("corpus");
        crate::generator::generate_corpus(&corpus, false).unwrap();
        let fake_kio = temporary.path().join("fake-kio");
        fs::write(
            &fake_kio,
            br##"#!/bin/sh
set -eu
case "${2-}" in
  init)
    mkdir .kio
    : > .kio/config.toml
    printf '%s\n' '{"status":"initialized"}'
    printf '%s\n' 'degraded cache write' >&2
    ;;
  *) exit 10 ;;
esac
"##,
        )
        .unwrap();
        fs::set_permissions(&fake_kio, fs::Permissions::from_mode(0o700)).unwrap();

        let error = replay_history(&corpus, &fake_kio).unwrap_err().to_string();
        assert!(error.contains("successful command emitted stderr"));
        assert!(!corpus.join("history-manifest.json").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn late_child_failure_leaves_no_manifest_and_dirty_replay_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let corpus = temporary.path().join("corpus");
        crate::generator::generate_corpus(&corpus, false).unwrap();
        let fake_kio = temporary.path().join("fake-kio");
        fs::write(
            &fake_kio,
            br##"#!/bin/sh
set -eu
counter="$XDG_STATE_HOME/replay-count"
count=0
if [ -f "$counter" ]; then read -r count < "$counter"; fi
count=$((count + 1))
printf '%s\n' "$count" > "$counter"
if [ "$count" -eq 4 ]; then exit 9; fi
case "${2-}" in
  init)
    mkdir .kio
    : > .kio/config.toml
    printf '%s\n' '{"status":"initialized"}'
    ;;
  index)
    printf '%s\n' '{"status":"indexed","failed_files":0,"network_allowed":false,"network_opt_in":false,"commit":{"commit_type":"auto","message":"kio index auto snapshot"}}'
    ;;
  snapshot)
    printf '{"status":"created","commit":{"commit_type":"manual","message":"%s"}}\n' "${5-}"
    ;;
  *) exit 10 ;;
esac
"##,
        )
        .unwrap();
        fs::set_permissions(&fake_kio, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(replay_history(&corpus, &fake_kio).is_err());
        assert!(!corpus.join("history-manifest.json").exists());
        let plan = manifest::frozen_history_plan().unwrap();
        let (edited_scope, edited_file, edited_hash) = plan
            .operations
            .iter()
            .find_map(|operation| match operation {
                HistoryOperation::Edit {
                    scope,
                    file,
                    after_raw_sha256,
                    ..
                } => Some((scope, file, after_raw_sha256)),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            sha_without_prefix(&hash_bytes(
                &fs::read(corpus.join(edited_scope).join(edited_file)).unwrap()
            )),
            edited_hash
        );
        let second = replay_history(&corpus, &fake_kio).unwrap_err().to_string();
        assert!(second.contains("fresh corpus already contains replay state"));
        assert!(!corpus.join("history-manifest.json").exists());
    }
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn unsupported_platform_fails_before_touching_corpus() {
        let temporary = tempfile::tempdir().unwrap();
        let corpus = temporary.path().join("absent-corpus");
        let error = replay_history(&corpus, Path::new("/usr/bin/false"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported"));
        assert!(!corpus.exists());
    }
}
