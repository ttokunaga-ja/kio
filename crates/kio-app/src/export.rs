//! Step 4 destination-only export.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Seek};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cap_primitives::fs as cap_fs;
use kio_core::cas::{MAX_RAW_OBJECT_BYTES, ObjectKind, ObjectStore, hash_bytes, is_hash};
use kio_core::dag::{CommitType, TreeEntry};
use kio_core::gc::ensure_no_active_sweep;
use kio_core::history::HistoryReader;
use kio_core::portable::portable_collision_key;
use kio_core::purge::{EventKind, PurgeState, canonical_final_event};
use kio_core::scope::Repository;
use kio_core::{ExitCode, KioError, Result};
use serde_json::{Value, json};
use sha2::Digest;

use super::{ReadBarrierCheckpoint, ScopeTarget};
use crate::commands::ExportArgs;

const MAX_POINTER_INPUT_BYTES: u64 = 1024 * 1024;
const MAX_EXPORT_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_DESTINATION_ENTRIES: usize = 100_000;
const MAX_DESTINATION_NAME_BYTES: u64 = 16 * 1024 * 1024;
/// §E (U26) reserved evacuation/quarantine namespace suffixes (PA20-26).
const EXPORT_BACKUP_SUFFIX: &str = ".kio-export-bak";
const EXPORT_QUARANTINE_SUFFIX: &str = ".kio-export-quarantine";

#[derive(Debug, Clone)]
struct ExportItem {
    path_at_commit: String,
    raw_hash: String,
}

#[derive(Debug, Clone)]
struct ExportSource {
    source_kind: &'static str,
    source_commit: String,
    target: ScopeTarget,
    files: Vec<ExportItem>,
}

#[derive(Debug, Clone)]
struct PreflightFile {
    source: ExportItem,
    destination: PathBuf,
    size_bytes: u64,
    overwritten: bool,
}

#[derive(Debug)]
struct StagedFile {
    preflight: PreflightFile,
    temp: OsString,
    temp_file: File,
}

#[derive(Debug)]
struct DestinationDir {
    path: PathBuf,
    handle: File,
}

pub(super) fn run(args: ExportArgs) -> Result<Value> {
    if args.yes && !args.force {
        return Err(KioError::invalid_usage(
            "export --yes is valid only together with --force",
        ));
    }

    // (0) kio_format_version compatibility is already checked inside
    // `resolve_source` (both `resolve_evidence_source` and
    // `resolve_local_source` open a `Repository` before returning), so it
    // runs ahead of the shared (1)+(3) pair below — QB5 (step4b-contract-
    // tests-p3b.md §A): this ordering was already correct, unlike
    // open/view/evidence-verify (QB6).
    let source = resolve_source(&args.source)?;
    // A shallow sweep publishes its durable receipt before removing the tree.
    // Export must not stage bytes, prompt, or publish while that state machine
    // is active.  The helper also rejects a malformed marker rather than
    // treating it as absence.
    ensure_no_active_sweep(&source.target.kio_dir)?;
    // QB5/QB6/裁定1: shared (1)+(3) preflight pair, opened as soon as the
    // source scope is known. LC57: export's checkpoint 2 (below, in
    // `publish_all`) fires per file, immediately before that file's atomic
    // rename — after the private-temp staging completes and before the
    // irreversible publish, matching the spec's fixed "expand -> recheck ->
    // publish" order.
    let checkpoint = super::preflight_barrier_and_index(&source.target.kio_dir)?;
    let validated_destination = validate_destination(&args.to, &source.target)?;
    let _initial_preflight = preflight(&source, &validated_destination.path, args.force)?;

    if args.force && !args.yes {
        confirm_force()?;
    }

    let destination_dir = open_destination_dir(
        &validated_destination.path,
        true,
        validated_destination.identity.as_ref(),
    )?;
    // Capability-directory handles close destination path races but do not
    // serialize source authorization against purge. Export intentionally stays
    // off `.kio/.lock`; instead it shares this narrow publication lock with
    // purge, acquiring it only after confirmation and destination opening. Purge
    // takes store -> publication, while export takes publication only, so no
    // reverse lock order exists. Keep it across the authoritative recheck,
    // private staging, and every final publication.
    let publication_repo = Repository::open_for_recovery(&source.target.repo_root)?;
    let _purge_publication_lock = publication_repo.lock_purge_publication()?;
    // Recheck after the only lock export takes and before any private
    // destination staging.  GC deliberately does not share this lock.
    ensure_no_active_sweep(&source.target.kio_dir)?;
    // The directory may have appeared between the first preflight and creation.
    // Re-run the complete leaf check before staging any content.
    let preflight_files = preflight_in_dir(&source, &destination_dir, args.force)?;
    let staged = stage_all(&source, preflight_files, &destination_dir)?;
    publish_all(&source, &destination_dir, staged, &checkpoint)
}

fn resolve_source(operand: &str) -> Result<ExportSource> {
    if operand == "-" || operand.starts_with("kio://") {
        return resolve_evidence_source(operand);
    }
    if operand.trim_start().starts_with('{') {
        return resolve_evidence_source(operand.trim());
    }
    resolve_local_source(operand)
}

fn resolve_evidence_source(operand: &str) -> Result<ExportSource> {
    let text = read_pointer_operand(operand)?;
    let pointer = super::parse_pointer_text(&text)?;
    let target = super::resolve_scope_target(&pointer.scope_id, pointer.scope_path.as_deref())?;
    let repo = Repository::open(&target.repo_root)?;
    super::validate_repo_tool_lock(&repo)?;

    let commit = match repo.read_commit(&pointer.commit) {
        Ok(commit) => commit,
        Err(error) if super::is_store_not_found(&error) => {
            return Err(super::unresolvable_commit_pointer_error(&pointer));
        }
        Err(error) => return Err(error),
    };
    reject_purged_commit(&pointer.commit, commit.commit_type)?;
    if pointer
        .tree
        .as_deref()
        .is_some_and(|tree| tree != commit.tree)
    {
        return Err(super::invalid_pointer_identity_error(&pointer));
    }
    let tree = match repo.read_tree(&commit.tree) {
        Ok(tree) => tree,
        Err(error) if super::is_store_not_found(&error) => {
            kio_core::gc::validate_final_shallow_tree(
                &target.kio_dir,
                &pointer.commit,
                &commit.tree,
            )?;
            return Err(KioError::commit_shallow(
                "export requires the complete evidence commit tree; the tree object is missing or shallow",
                pointer.commit.clone(),
            ));
        }
        Err(error) => return Err(error),
    };
    // Dead-source visibility wins over derivative availability/corruption.
    // The evidence pointer's exact raw identity is known before any normalized
    // chunk is read, so apply the purge gates at this point. PA47-50: raw
    // presence is resolved FIRST so the canonical dispatch inside
    // `check_purge_state` can distinguish an ordinary `erased` absence
    // (PA48(a)) from a `retired`/unmarked absence (PA48(b)/(c), corruption).
    let store = ObjectStore::new(&target.kio_dir);
    let raw_present = raw_present_now(&target, &pointer.raw_hash)?;
    check_purge_state(&target, &pointer.raw_hash, raw_present)?;
    match store.inspect_object(ObjectKind::Raw, &pointer.raw_hash) {
        Ok(_) => {}
        Err(error) if super::is_store_not_found(&error) => {
            return Err(missing_live_raw_error(&target, &pointer.raw_hash));
        }
        Err(error) => return Err(error),
    }
    let chunk = match store.read_chunk(&pointer.chunk_hash) {
        Ok(chunk) => chunk,
        Err(error) if super::is_store_not_found(&error) => {
            return Err(KioError::new(
                "KIO-E-EVIDENCE-RETARGET-REQUIRED-001",
                "chunk not materialized for this tool_profile_hash; retarget required",
                json!({
                    "chunk_hash": pointer.chunk_hash,
                    "tool_profile_hash": pointer.tool_profile_hash,
                    "raw_hash": pointer.raw_hash,
                }),
                ExitCode::IncompatibleProfile,
            ));
        }
        Err(error) => return Err(error),
    };
    if chunk.raw_hash != pointer.raw_hash || chunk.tool_profile_hash != pointer.tool_profile_hash {
        return Err(super::invalid_pointer_identity_error(&pointer));
    }
    let mut matching_entries = tree.entries.iter().filter(|entry| {
        entry.raw_hash == pointer.raw_hash
            && entry.normalize.as_ref().is_some_and(|normalize| {
                normalize.tool_profile_hash == pointer.tool_profile_hash
                    && normalize.r#gen == chunk.r#gen
            })
    });
    let entry = match pointer.path_at_commit.as_deref() {
        Some(path) => matching_entries
            .find(|entry| entry.path == path)
            .ok_or_else(|| super::invalid_pointer_identity_error(&pointer))?,
        None => matching_entries
            .next()
            .ok_or_else(|| super::invalid_pointer_identity_error(&pointer))?,
    };

    Ok(ExportSource {
        source_kind: "evidence",
        source_commit: pointer.commit,
        target,
        files: vec![ExportItem {
            path_at_commit: entry.path.clone(),
            raw_hash: entry.raw_hash.clone(),
        }],
    })
}

fn read_pointer_operand(operand: &str) -> Result<String> {
    if operand != "-" {
        if operand.len() as u64 > MAX_POINTER_INPUT_BYTES {
            return Err(KioError::invalid_usage(
                "evidence pointer input exceeds the 1 MiB limit",
            ));
        }
        return Ok(operand.to_owned());
    }

    let input = crate::context::read_input(MAX_POINTER_INPUT_BYTES as usize)?;
    String::from_utf8(input)
        .map(|input| input.trim().to_owned())
        .map_err(|_| KioError::invalid_usage("evidence pointer input must be UTF-8"))
}

fn resolve_local_source(operand: &str) -> Result<ExportSource> {
    let repo = crate::open_current_recovery_repository()?;
    super::validate_repo_tool_lock(&repo)?;
    let target = super::scope_target(repo.root())?;

    if operand == "HEAD" {
        let commit_hash = repo
            .head_commit_hash()?
            .ok_or_else(|| export_source_not_found("HEAD"))?;
        repo.read_commit(&commit_hash).map_err(|error| {
            if super::is_store_not_found(&error) {
                export_source_not_found(&commit_hash)
            } else {
                error
            }
        })?;
        return resolve_commit_source(target, commit_hash);
    }

    if is_hash(operand) {
        let store = ObjectStore::new(repo.kio_dir());
        match store.inspect_object(ObjectKind::Commit, operand) {
            Ok(_) => return resolve_commit_source(target, operand.to_owned()),
            Err(error) if super::is_store_not_found(&error) => {}
            Err(error) => return Err(error),
        }
        if store.inspect_object(ObjectKind::Raw, operand).is_ok() {
            return Err(KioError::invalid_usage(
                "raw-hash shorthand is not an export source; use evidence, path, or commit",
            ));
        }
        return Err(export_source_not_found(operand));
    }

    match repo.resolve_commit(operand) {
        Ok(commit_hash) => resolve_commit_source(target, commit_hash),
        Err(error) if super::is_store_not_found(&error) => resolve_path_source(target, operand),
        Err(error) => Err(error),
    }
}

fn resolve_commit_source(target: ScopeTarget, commit_hash: String) -> Result<ExportSource> {
    let reader = HistoryReader::new(&target.kio_dir);
    let snapshot = reader.snapshot(&commit_hash)?;
    reject_purged_commit(&commit_hash, snapshot.commit.commit_type)?;
    let files = snapshot
        .tree
        .entries
        .iter()
        .map(|entry| ExportItem {
            path_at_commit: entry.path.clone(),
            raw_hash: entry.raw_hash.clone(),
        })
        .collect();
    Ok(ExportSource {
        source_kind: "commit",
        source_commit: commit_hash,
        target,
        files,
    })
}

fn resolve_path_source(target: ScopeTarget, path: &str) -> Result<ExportSource> {
    validate_export_name(path, &hash_bytes(b"export-path-validation"))?;
    let repo = Repository::open(&target.repo_root)?;
    let head = repo
        .head_commit_hash()?
        .ok_or_else(|| export_source_not_found(path))?;
    let history = HistoryReader::new(&target.kio_dir).walk(&head)?;
    let binding = history
        .newest_binding_for_path(path)
        .ok_or_else(|| export_source_not_found(path))?;
    let node = history
        .node(&binding.commit_hash)
        .expect("binding commit belongs to first-parent history");
    reject_purged_commit(&binding.commit_hash, node.commit.commit_type)?;
    Ok(ExportSource {
        source_kind: "path",
        source_commit: binding.commit_hash,
        target,
        files: vec![ExportItem {
            path_at_commit: binding.binding.path,
            raw_hash: binding.binding.raw_hash,
        }],
    })
}

fn reject_purged_commit(commit_hash: &str, commit_type: CommitType) -> Result<()> {
    if commit_type == CommitType::Purged {
        return Err(KioError::new(
            "KIO-E-PURGE-NOT-FOUND-001",
            "export source is a purged commit",
            json!({ "source_commit": commit_hash }),
            ExitCode::PermanentFailure,
        ));
    }
    Ok(())
}

fn export_source_not_found(source: &str) -> KioError {
    KioError::new(
        "KIO-E-COMMIT-EXPORT-SOURCE-NOT-FOUND-001",
        "export source is not reachable",
        json!({ "source": source }),
        ExitCode::PermanentFailure,
    )
}

fn preflight(source: &ExportSource, destination: &Path, force: bool) -> Result<Vec<PreflightFile>> {
    validate_source_names(&source.files)?;
    let existing = if destination.exists() {
        scan_destination(destination)?
    } else {
        BTreeMap::new()
    };
    let store = ObjectStore::new(&source.target.kio_dir);
    let mut total_bytes = 0_u64;
    let mut verified_raws = BTreeMap::<String, u64>::new();
    let mut files = Vec::with_capacity(source.files.len());

    for item in &source.files {
        // PA47-50: raw presence resolved first, fed to the canonical
        // dispatch (replacing the old "check_purge_state then separately
        // fall back to a generic not-found on any absence" 2-stage shape).
        let (size_bytes, raw_present) = match verified_raws.get(&item.raw_hash) {
            Some(size) => (*size, true),
            None => match store.inspect_object(ObjectKind::Raw, &item.raw_hash) {
                Ok(metadata) => {
                    verified_raws.insert(item.raw_hash.clone(), metadata.size_bytes);
                    (metadata.size_bytes, true)
                }
                Err(error) if super::is_store_not_found(&error) => (0, false),
                Err(error) => return Err(error),
            },
        };
        check_purge_state(&source.target, &item.raw_hash, raw_present)?;
        // PA21: same-name evacuation/quarantine residue is checked before any
        // mutation, independent of `--force`/destination existence.
        check_no_stale_evacuation_namespace_std(destination, &item.path_at_commit)?;
        total_bytes = total_bytes.saturating_add(size_bytes);
        if total_bytes > MAX_EXPORT_TOTAL_BYTES {
            return Err(KioError::new(
                "KIO-E-COMMIT-EXPORT-LIMIT-001",
                "export source bytes exceed the 4 GiB aggregate limit",
                json!({
                    "max_bytes": MAX_EXPORT_TOTAL_BYTES,
                    "attempted_bytes": total_bytes,
                }),
                ExitCode::PermanentFailure,
            ));
        }

        let collision_key = portable_collision_key(&item.path_at_commit);
        let existing_names = existing.get(&collision_key);
        if existing_names
            .is_some_and(|names| names.len() != 1 || names.first() != Some(&item.path_at_commit))
        {
            return Err(unsafe_export_error(
                destination,
                "destination contains a case/normalization-colliding leaf",
            ));
        }
        let target = destination.join(&item.path_at_commit);
        let overwritten = match fs::symlink_metadata(&target) {
            Ok(_) => {
                validate_replaceable_file(&target)?;
                if !force {
                    return Err(KioError::new(
                        "KIO-E-COMMIT-EXPORT-CONFLICT-001",
                        "destination file already exists; use --force to replace it",
                        json!({
                            "path": target,
                            // R23-26 (06 §5 L282-285): EXPORT-CONFLICT-001 is
                            // documented as always-retryable exit 3 -- this
                            // preflight rejection (no mutation attempted yet)
                            // shares the code with the publish/rollback race
                            // terminations `export_conflict_error` classifies
                            // below, but "no --force given" is not itself one
                            // of that constructor's 7 closed `conflict_kind`
                            // race/residue values (spec-closed enum: docs/05
                            // §3.5 L883-885 name the same 7 verbatim) --
                            // fabricating an 8th value, or a wrong existing
                            // one, would misclassify a deterministic
                            // usage gap as a transient/residue race.
                            // `retry_disposition` alone is still accurate and
                            // actionable without it: passing --force is
                            // unambiguously a manual action, never resolved by
                            // an identical retry.
                            "retry_disposition": "manual_action",
                        }),
                        ExitCode::PartialFailure,
                    ));
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(KioError::io(
                    error.to_string(),
                    target.display().to_string(),
                ));
            }
        };
        files.push(PreflightFile {
            source: item.clone(),
            destination: target,
            size_bytes,
            overwritten,
        });
    }
    Ok(files)
}

fn preflight_in_dir(
    source: &ExportSource,
    destination: &DestinationDir,
    force: bool,
) -> Result<Vec<PreflightFile>> {
    validate_source_names(&source.files)?;
    let existing = scan_destination_handle(destination)?;
    let store = ObjectStore::new(&source.target.kio_dir);
    let mut total_bytes = 0_u64;
    let mut verified_raws = BTreeMap::<String, u64>::new();
    let mut files = Vec::with_capacity(source.files.len());

    for item in &source.files {
        // PA47-50: raw presence resolved first, fed to the canonical
        // dispatch.
        let (size_bytes, raw_present) = match verified_raws.get(&item.raw_hash) {
            Some(size) => (*size, true),
            None => match store.inspect_object(ObjectKind::Raw, &item.raw_hash) {
                Ok(metadata) => {
                    verified_raws.insert(item.raw_hash.clone(), metadata.size_bytes);
                    (metadata.size_bytes, true)
                }
                Err(error) if super::is_store_not_found(&error) => (0, false),
                Err(error) => return Err(error),
            },
        };
        check_purge_state(&source.target, &item.raw_hash, raw_present)?;
        // PA21: same-name evacuation/quarantine residue is checked before any
        // mutation, independent of `--force`/destination existence.
        check_no_stale_evacuation_namespace(destination, &item.path_at_commit)?;
        total_bytes = total_bytes.saturating_add(size_bytes);
        if total_bytes > MAX_EXPORT_TOTAL_BYTES {
            return Err(KioError::new(
                "KIO-E-COMMIT-EXPORT-LIMIT-001",
                "export source bytes exceed the 4 GiB aggregate limit",
                json!({
                    "max_bytes": MAX_EXPORT_TOTAL_BYTES,
                    "attempted_bytes": total_bytes,
                }),
                ExitCode::PermanentFailure,
            ));
        }

        let collision_key = portable_collision_key(&item.path_at_commit);
        let existing_names = existing.get(&collision_key);
        if existing_names
            .is_some_and(|names| names.len() != 1 || names.first() != Some(&item.path_at_commit))
        {
            return Err(unsafe_export_error(
                &destination.path,
                "destination contains a case/normalization-colliding leaf",
            ));
        }
        let target = destination.path.join(&item.path_at_commit);
        let overwritten = match cap_fs::stat(
            &destination.handle,
            Path::new(&item.path_at_commit),
            cap_fs::FollowSymlinks::No,
        ) {
            Ok(metadata) => {
                validate_replaceable_metadata(&target, &metadata)?;
                if !force {
                    return Err(KioError::new(
                        "KIO-E-COMMIT-EXPORT-CONFLICT-001",
                        "destination file already exists; use --force to replace it",
                        json!({
                            "path": target,
                            // R23-26 (06 §5 L282-285): EXPORT-CONFLICT-001 is
                            // documented as always-retryable exit 3 -- this
                            // preflight rejection (no mutation attempted yet)
                            // shares the code with the publish/rollback race
                            // terminations `export_conflict_error` classifies
                            // below, but "no --force given" is not itself one
                            // of that constructor's 7 closed `conflict_kind`
                            // race/residue values (spec-closed enum: docs/05
                            // §3.5 L883-885 name the same 7 verbatim) --
                            // fabricating an 8th value, or a wrong existing
                            // one, would misclassify a deterministic
                            // usage gap as a transient/residue race.
                            // `retry_disposition` alone is still accurate and
                            // actionable without it: passing --force is
                            // unambiguously a manual action, never resolved by
                            // an identical retry.
                            "retry_disposition": "manual_action",
                        }),
                        ExitCode::PartialFailure,
                    ));
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(KioError::io(
                    error.to_string(),
                    target.display().to_string(),
                ));
            }
        };
        files.push(PreflightFile {
            source: item.clone(),
            destination: target,
            size_bytes,
            overwritten,
        });
    }
    Ok(files)
}

fn validate_source_names(files: &[ExportItem]) -> Result<()> {
    let mut collision_keys = BTreeSet::new();
    for item in files {
        validate_export_name(&item.path_at_commit, &item.raw_hash)?;
        if !collision_keys.insert(portable_collision_key(&item.path_at_commit)) {
            return Err(KioError::new(
                "KIO-E-COMMIT-EXPORT-UNSAFE-001",
                "export source contains case/normalization-colliding paths",
                json!({ "path_at_commit": item.path_at_commit }),
                ExitCode::Failure,
            ));
        }
    }
    Ok(())
}

fn validate_export_name(path: &str, raw_hash: &str) -> Result<()> {
    // PA20 (§E, U26): the evacuation/quarantine namespace is reserved —
    // refuse before any expansion (not even a private temp) rather than let
    // a historically-legitimate file with this literal suffix collide with
    // the bak/quarantine protocol below.
    if path.ends_with(EXPORT_BACKUP_SUFFIX) || path.ends_with(EXPORT_QUARANTINE_SUFFIX) {
        return Err(KioError::new(
            "KIO-E-COMMIT-EXPORT-UNSAFE-001",
            "export source name uses the reserved evacuation/quarantine namespace; \
             export it under a different name instead",
            json!({ "path_at_commit": path }),
            ExitCode::Failure,
        ));
    }
    let entry = TreeEntry {
        path: path.to_owned(),
        entry_type: "file".to_owned(),
        raw_hash: raw_hash.to_owned(),
        normalize: None,
    };
    entry.validate_materialization_path().map_err(|_| {
        KioError::new(
            "KIO-E-COMMIT-EXPORT-UNSAFE-001",
            "historical path cannot be materialized safely on this platform",
            json!({ "path_at_commit": path }),
            ExitCode::Failure,
        )
    })
}

/// PA16/17 (§D, U25): the canonically-resolved `--to` destination, plus the
/// `lstat` (dev/inode) identity captured at THIS containment-check moment —
/// PA18 requires `open_destination_dir` compare against this exact value
/// (never re-fetched), so a TOCTOU component swap between this check and the
/// open below is caught rather than silently trusted.
struct ValidatedDestination {
    path: PathBuf,
    /// `None` when the canonical destination does not exist yet (a normal
    /// case — `effective_destination` canonicalizes only the deepest
    /// EXISTING ancestor and re-appends the still-missing suffix;
    /// `open_destination_dir` creates the rest). PA18's containment binding
    /// only applies once there is a pre-existing entity to bind to; a
    /// freshly-created leaf has no prior identity to have been swapped away
    /// from.
    identity: Option<DestinationIdentity>,
}

fn validate_destination(input: &Path, target: &ScopeTarget) -> Result<ValidatedDestination> {
    let destination = normalize_absolute(input)?;
    verify_destination_input_chain(&destination, target)?;
    let destination = effective_destination(&destination)?;
    verify_existing_directory_chain(&destination)?;
    let scope_root = target
        .repo_root
        .canonicalize()
        .map_err(|error| KioError::io(error.to_string(), target.repo_root.display().to_string()))?;
    // PA16(d): "scope root 配下" is checked in full — not just the narrower
    // `.kio` descendant test the prior implementation used — so an ordinary
    // subdirectory of the scope root (not `.kio` at all) is caught too.
    // PA16/17: `KIO-E-CONFIG-USAGE-001` (exit 2), not the generic
    // `KIO-E-COMMIT-EXPORT-UNSAFE-001` (exit 1) `unsafe_export_error`
    // gives every other structural-safety rejection in this module — the new
    // rule specifically wants the usage-error family so automation can
    // recognize "this destination is categorically forbidden" distinctly
    // from "this destination had a transient/OS-level problem".
    if destination == scope_root || destination.starts_with(&scope_root) {
        return Err(KioError::new(
            "KIO-E-CONFIG-USAGE-001",
            "export destination must not be the scope root or a scope-root descendant (.kio included)",
            json!({ "path": destination }),
            ExitCode::InvalidUsage,
        ));
    }
    let identity = match fs::symlink_metadata(&destination) {
        Ok(_) => Some(destination_lstat_identity(&destination)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(KioError::io(
                error.to_string(),
                destination.display().to_string(),
            ));
        }
    };
    Ok(ValidatedDestination {
        path: destination,
        identity,
    })
}

/// macOS commonly exposes a canonical scope under `/private/var/...` through
/// the system `/var` alias. When an existing ancestor resolves exactly to the
/// already-validated source scope root, use that real directory as the trust
/// anchor and reject reparse components below it. Destinations without such an
/// anchor retain the strict full-chain check.
fn verify_destination_input_chain(path: &Path, target: &ScopeTarget) -> Result<()> {
    let anchor = path.ancestors().find(|ancestor| {
        fs::symlink_metadata(ancestor).is_ok()
            && ancestor
                .canonicalize()
                .is_ok_and(|canonical| canonical == target.repo_root)
    });
    let Some(anchor) = anchor else {
        return verify_existing_directory_chain(path);
    };
    validate_real_directory(anchor)?;
    let relative = path.strip_prefix(anchor).map_err(|_| {
        unsafe_export_error(
            path,
            "destination cannot be made relative to its scope anchor",
        )
    })?;
    let mut current = anchor.to_path_buf();
    let mut missing = false;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(unsafe_export_error(
                path,
                "destination contains a non-local component below its scope anchor",
            ));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(_) if missing => {
                return Err(unsafe_export_error(
                    &current,
                    "destination ancestry changed while it was inspected",
                ));
            }
            Ok(_) => validate_real_directory(&current)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = true,
            Err(error) => {
                return Err(KioError::io(
                    error.to_string(),
                    current.display().to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn normalize_absolute(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        crate::context_working_directory()
            .map_err(|error| KioError::io(error.to_string(), "."))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    if !normalized.is_absolute() {
        return Err(KioError::invalid_usage(
            "export destination must resolve to an absolute path",
        ));
    }
    Ok(normalized)
}

fn verify_existing_directory_chain(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    let mut missing = false;
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(_) if is_platform_root_alias(&current) => {}
            Ok(_) if missing => {
                return Err(unsafe_export_error(
                    &current,
                    "destination ancestry changed while it was inspected",
                ));
            }
            Ok(_) => validate_real_directory(&current)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = true,
            Err(error) => {
                return Err(KioError::io(
                    error.to_string(),
                    current.display().to_string(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn is_platform_root_alias(path: &Path) -> bool {
    matches!(path.to_str(), Some("/var" | "/tmp" | "/etc"))
        && path
            .canonicalize()
            .is_ok_and(|canonical| canonical.starts_with("/private"))
}

#[cfg(not(target_os = "macos"))]
fn is_platform_root_alias(_path: &Path) -> bool {
    false
}

fn effective_destination(path: &Path) -> Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut suffix = Vec::<OsString>::new();
    loop {
        match fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let leaf = existing
                    .file_name()
                    .ok_or_else(|| unsafe_export_error(path, "destination has no existing root"))?;
                suffix.push(leaf.to_os_string());
                if !existing.pop() {
                    return Err(unsafe_export_error(
                        path,
                        "destination has no existing root",
                    ));
                }
            }
            Err(error) => {
                return Err(KioError::io(
                    error.to_string(),
                    existing.display().to_string(),
                ));
            }
        }
    }
    let mut canonical = existing
        .canonicalize()
        .map_err(|error| KioError::io(error.to_string(), existing.display().to_string()))?;
    for component in suffix.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

/// `expected_identity`: PA18 (§D, U25) — when set, the identity of the
/// directory this call actually opens (dev/inode on Unix, volume serial plus
/// file index on Windows) must match the value
/// [`destination_lstat_identity`] captured at `validate_destination`'s
/// containment-check moment (never re-fetched here — re-fetching would defeat
/// the point, since it would just re-observe whatever a TOCTOU swap left
/// behind). `None` skips the check (used by call sites, and tests, that never
/// went through `validate_destination`).
fn open_destination_dir(
    path: &Path,
    create_missing: bool,
    expected_identity: Option<&DestinationIdentity>,
) -> Result<DestinationDir> {
    let mut root = PathBuf::new();
    let mut descendants = Vec::<OsString>::new();
    let mut saw_root = false;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => root.push(prefix.as_os_str()),
            Component::RootDir => {
                root.push(component.as_os_str());
                saw_root = true;
            }
            Component::Normal(name) if saw_root => descendants.push(name.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::Normal(_) => {
                return Err(unsafe_export_error(
                    path,
                    "export destination does not have a stable filesystem root",
                ));
            }
        }
    }
    if !saw_root {
        return Err(unsafe_export_error(
            path,
            "export destination does not have a stable filesystem root",
        ));
    }

    let mut current = cap_fs::open_ambient_dir(&root, cap_primitives::ambient_authority())
        .map_err(|error| KioError::io(error.to_string(), root.display().to_string()))?;
    let mut display = root;
    for component in descendants {
        display.push(&component);
        match cap_fs::open_dir_nofollow(&current, Path::new(&component)) {
            Ok(next) => current = next,
            Err(error) if create_missing && error.kind() == std::io::ErrorKind::NotFound => {
                #[cfg(unix)]
                let options = {
                    use cap_fs::DirBuilderExt;
                    let mut options = cap_fs::DirOptions::new();
                    options.mode(0o700);
                    options
                };
                #[cfg(not(unix))]
                let options = cap_fs::DirOptions::new();
                match cap_fs::create_dir(&current, Path::new(&component), &options) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(KioError::io(
                            error.to_string(),
                            display.display().to_string(),
                        ));
                    }
                }
                current =
                    cap_fs::open_dir_nofollow(&current, Path::new(&component)).map_err(|_| {
                        unsafe_export_error(
                            &display,
                            "destination ancestor is not a real non-reparse directory",
                        )
                    })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(KioError::io(
                    error.to_string(),
                    display.display().to_string(),
                ));
            }
            Err(_) => {
                return Err(unsafe_export_error(
                    &display,
                    "destination ancestor is not a real non-reparse directory",
                ));
            }
        }
    }
    let metadata = current
        .metadata()
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    if !metadata.is_dir() {
        return Err(unsafe_export_error(
            path,
            "destination ancestor is not a real non-reparse directory",
        ));
    }
    if let Some(expected) = expected_identity
        && !expected.matches_opened(&current)
    {
        return Err(KioError::new(
            "KIO-E-CONFIG-USAGE-001",
            "export destination identity changed between validation and open",
            json!({ "path": path }),
            ExitCode::InvalidUsage,
        ));
    }
    Ok(DestinationDir {
        path: path.to_path_buf(),
        handle: current,
    })
}

fn validate_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(unsafe_export_error(
            path,
            "destination ancestor is not a real non-reparse directory",
        ));
    }
    #[cfg(windows)]
    if !kio_core::cas::windows_directory_is_real(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
    {
        return Err(unsafe_export_error(
            path,
            "destination ancestor is a reparse point",
        ));
    }
    Ok(())
}

fn scan_destination(path: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    validate_real_directory(path)?;
    let mut entries = BTreeMap::new();
    let mut count = 0_usize;
    let mut name_bytes = 0_u64;
    for entry in fs::read_dir(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
    {
        let entry =
            entry.map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
        count += 1;
        if count > MAX_DESTINATION_ENTRIES {
            return Err(unsafe_export_error(
                path,
                "destination directory exceeds the 100000-entry inspection limit",
            ));
        }
        let name = entry.file_name();
        name_bytes = name_bytes.saturating_add(name.as_encoded_bytes().len() as u64);
        if name_bytes > MAX_DESTINATION_NAME_BYTES {
            return Err(unsafe_export_error(
                path,
                "destination names exceed the 16 MiB inspection limit",
            ));
        }
        if let Some(name) = name.to_str() {
            entries
                .entry(portable_collision_key(name))
                .or_insert_with(Vec::new)
                .push(name.to_owned());
        }
    }
    Ok(entries)
}

fn scan_destination_handle(destination: &DestinationDir) -> Result<BTreeMap<String, Vec<String>>> {
    let mut entries = BTreeMap::new();
    let mut count = 0_usize;
    let mut name_bytes = 0_u64;
    for entry in cap_fs::read_base_dir(&destination.handle)
        .map_err(|error| KioError::io(error.to_string(), destination.path.display().to_string()))?
    {
        let entry = entry.map_err(|error| {
            KioError::io(error.to_string(), destination.path.display().to_string())
        })?;
        count += 1;
        if count > MAX_DESTINATION_ENTRIES {
            return Err(unsafe_export_error(
                &destination.path,
                "destination directory exceeds the 100000-entry inspection limit",
            ));
        }
        let name = entry.file_name();
        name_bytes = name_bytes.saturating_add(name.as_encoded_bytes().len() as u64);
        if name_bytes > MAX_DESTINATION_NAME_BYTES {
            return Err(unsafe_export_error(
                &destination.path,
                "destination names exceed the 16 MiB inspection limit",
            ));
        }
        if let Some(name) = name.to_str() {
            entries
                .entry(portable_collision_key(name))
                .or_insert_with(Vec::new)
                .push(name.to_owned());
        }
    }
    Ok(entries)
}

fn validate_replaceable_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(unsafe_export_error(
            path,
            "export may replace only a real regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(unsafe_export_error(
                path,
                "export refuses a hard-linked destination file",
            ));
        }
    }
    #[cfg(windows)]
    if !kio_core::cas::windows_regular_file_is_safe(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
    {
        return Err(unsafe_export_error(
            path,
            "export refuses a reparse or hard-linked destination file",
        ));
    }
    Ok(())
}

fn validate_replaceable_metadata(path: &Path, metadata: &cap_fs::Metadata) -> Result<()> {
    if !metadata.is_file() || metadata.is_symlink() {
        return Err(unsafe_export_error(
            path,
            "export may replace only a real regular file",
        ));
    }
    #[cfg(unix)]
    {
        use cap_fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(unsafe_export_error(
                path,
                "export refuses a hard-linked destination file",
            ));
        }
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || metadata.number_of_links() != Some(1)
        {
            return Err(unsafe_export_error(
                path,
                "export refuses a reparse or hard-linked destination file",
            ));
        }
    }
    Ok(())
}

fn unsafe_export_error(path: &Path, message: &str) -> KioError {
    KioError::new(
        "KIO-E-COMMIT-EXPORT-UNSAFE-001",
        message,
        json!({ "path": path }),
        ExitCode::Failure,
    )
}

/// PA18 (§D, U25): the identity captured by `lstat` at
/// `validate_destination`'s containment-check moment. Compared, never
/// re-derived, against the directory `open_destination_dir` actually opens.
///
/// On Windows the same (volume serial, file index) pair `lstat` would expose is
/// taken through `GetFileInformationByHandle` instead: `std::os::windows::fs::
/// MetadataExt::{volume_serial_number, file_index}` are still unstable behind
/// `windows_by_handle` (rust-lang/rust#63010), so they cannot be used on stable
/// Rust. `kio_core::cas` already wraps that call for the purge/CAS paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DestinationIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    identity: kio_core::cas::WindowsDirectoryIdentity,
    #[cfg(not(any(unix, windows)))]
    len: u64,
}

impl DestinationIdentity {
    /// Compare against the directory handle `open_destination_dir` actually
    /// opened — the handle, never its path, so this stays the `fstat` half of
    /// PA18's binding. A handle that cannot be interrogated fails closed
    /// (reported as a mismatch), since an unverifiable destination is exactly
    /// the case the guard exists to refuse.
    #[cfg(unix)]
    fn matches_opened(&self, opened: &File) -> bool {
        use std::os::unix::fs::MetadataExt;
        opened
            .metadata()
            .is_ok_and(|opened| self.dev == opened.dev() && self.ino == opened.ino())
    }

    #[cfg(windows)]
    fn matches_opened(&self, opened: &File) -> bool {
        kio_core::cas::windows_directory_handle_identity(opened) == Some(self.identity)
    }

    #[cfg(not(any(unix, windows)))]
    fn matches_opened(&self, opened: &File) -> bool {
        opened
            .metadata()
            .is_ok_and(|opened| self.len == opened.len())
    }
}

fn destination_lstat_identity(path: &Path) -> Result<DestinationIdentity> {
    #[cfg(windows)]
    {
        // Opens the leaf without following a final reparse point (the `lstat`
        // half of PA18) and reads the identity off that handle. `None` means
        // the leaf is not a real directory — a reparse point or a non-directory
        // that `verify_existing_directory_chain` rejected moments ago, so its
        // appearance here is itself a mid-check swap. Fail closed.
        let Some(identity) = kio_core::cas::windows_real_directory_identity(path)
            .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
        else {
            return Err(unsafe_export_error(
                path,
                "destination identity is unavailable for this filesystem",
            ));
        };
        Ok(DestinationIdentity { identity })
    }
    #[cfg(not(windows))]
    {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(DestinationIdentity {
                dev: metadata.dev(),
                ino: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(DestinationIdentity {
                len: metadata.len(),
            })
        }
    }
}

fn confirm_force() -> Result<()> {
    if !crate::context::is_interactive()? {
        return Err(confirmation_rejected(
            "export --force requires --yes in non-interactive mode",
        ));
    }
    if crate::context::confirm("Export may replace existing files. Continue? [y/N]")? {
        Ok(())
    } else {
        Err(confirmation_rejected("export confirmation was rejected"))
    }
}

fn confirmation_rejected(message: &str) -> KioError {
    KioError::new(
        "KIO-E-CONFIRM-REJECTED-001",
        message,
        json!({}),
        ExitCode::ConfirmationRejected,
    )
}

/// PA27-29 (§F, U27): the closed 7-value `conflict_kind` enum every export
/// conflict termination carries. `retry_disposition` follows mechanically:
/// `transient` iff `PublishRace` (a competing-write race that leaves no
/// residue blocking the next preflight), `manual_action` for every other
/// value (PA29).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportConflictKind {
    PublishRace,
    QuarantineRenameRace,
    QuarantineMismatch,
    BackupMismatch,
    ExportRenameRace,
    StaleBackup,
    StaleQuarantine,
}

impl ExportConflictKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::PublishRace => "publish_race",
            Self::QuarantineRenameRace => "quarantine_rename_race",
            Self::QuarantineMismatch => "quarantine_mismatch",
            Self::BackupMismatch => "backup_mismatch",
            Self::ExportRenameRace => "export_rename_race",
            Self::StaleBackup => "stale_backup",
            Self::StaleQuarantine => "stale_quarantine",
        }
    }

    const fn retry_disposition(self) -> &'static str {
        match self {
            Self::PublishRace => "transient",
            _ => "manual_action",
        }
    }
}

/// PA27 (§F, U27): every export competing-write/evacuation/quarantine
/// termination uses this single error code + retryable exit 3, distinguished
/// only by `context.conflict_kind` (PA28) — never a scenario-specific code.
fn export_conflict_error(kind: ExportConflictKind, path: &Path) -> KioError {
    KioError::new(
        "KIO-E-COMMIT-EXPORT-CONFLICT-001",
        "export encountered a conflicting destination state",
        json!({
            "path": path,
            "conflict_kind": kind.as_str(),
            "retry_disposition": kind.retry_disposition(),
        }),
        ExitCode::PartialFailure,
    )
}

/// PA20/PA21 reserved-namespace suffix helpers.
fn backup_name(path_at_commit: &str) -> String {
    format!("{path_at_commit}{EXPORT_BACKUP_SUFFIX}")
}

fn quarantine_name(path_at_commit: &str) -> String {
    format!("{path_at_commit}{EXPORT_QUARANTINE_SUFFIX}")
}

/// PA21 (§E, U26): a same-name evacuation/quarantine residue is checked
/// before ANY mutation, regardless of `--force` or destination existence
/// (limiting the check to `--force` would let a non-`--force` retry, after
/// the destination itself disappeared, silently walk past a still-present
/// stale backup/quarantine from an earlier crashed attempt).
fn check_no_stale_evacuation_namespace_std(destination: &Path, path_at_commit: &str) -> Result<()> {
    for (suffix, kind) in [
        (EXPORT_BACKUP_SUFFIX, ExportConflictKind::StaleBackup),
        (
            EXPORT_QUARANTINE_SUFFIX,
            ExportConflictKind::StaleQuarantine,
        ),
    ] {
        let candidate = destination.join(format!("{path_at_commit}{suffix}"));
        match fs::symlink_metadata(&candidate) {
            Ok(_) => return Err(export_conflict_error(kind, &candidate)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(KioError::io(
                    error.to_string(),
                    candidate.display().to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn check_no_stale_evacuation_namespace(
    destination: &DestinationDir,
    path_at_commit: &str,
) -> Result<()> {
    for (suffix, kind) in [
        (EXPORT_BACKUP_SUFFIX, ExportConflictKind::StaleBackup),
        (
            EXPORT_QUARANTINE_SUFFIX,
            ExportConflictKind::StaleQuarantine,
        ),
    ] {
        let name = format!("{path_at_commit}{suffix}");
        match cap_fs::stat(
            &destination.handle,
            Path::new(&name),
            cap_fs::FollowSymlinks::No,
        ) {
            Ok(_) => {
                return Err(export_conflict_error(kind, &destination.path.join(&name)));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(KioError::io(
                    error.to_string(),
                    destination.path.join(&name).display().to_string(),
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
enum MoveOutcome {
    AlreadyExists,
    Io(KioError),
}

/// PA22/23/25/26: a portable no-replace rename, built from the same
/// hard-link-then-unlink idiom `publish_one`'s non-overwrite path already
/// used (`cap_fs` has no `RENAME_NOREPLACE`/`renamex_np(RENAME_EXCL)`
/// wrapper — this achieves the same atomicity: `hard_link` fails
/// `AlreadyExists` without touching `from` if `to` already exists, so `from`
/// is only unlinked once the new name is confirmed exclusively claimed).
fn no_replace_move(
    destination: &DestinationDir,
    from: &Path,
    to: &Path,
) -> std::result::Result<(), MoveOutcome> {
    match cap_fs::hard_link(&destination.handle, from, &destination.handle, to) {
        Ok(()) => cap_fs::remove_file(&destination.handle, from).map_err(|error| {
            MoveOutcome::Io(KioError::io(
                error.to_string(),
                destination.path.join(from).display().to_string(),
            ))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(MoveOutcome::AlreadyExists)
        }
        Err(error) => Err(MoveOutcome::Io(KioError::io(
            error.to_string(),
            destination.path.join(to).display().to_string(),
        ))),
    }
}

fn missing_live_raw_error(target: &ScopeTarget, raw_hash: &str) -> KioError {
    KioError::new(
        "KIO-E-PURGE-NOT-FOUND-001",
        "export source is no longer available",
        json!({
            "raw_hash": raw_hash,
            "scope_path": target.kio_dir,
        }),
        ExitCode::PermanentFailure,
    )
}

/// LC8-LC10/item 3: the tombstone gate uses the cross-marker canonical final
/// event, not this marker's own tail — a tombstone whose own tail is
/// `purged` can be superseded by a later-`lifecycle_epoch` `retired` erase
/// receipt (LC10's worked example), so checking the tombstone alone can
/// wrongly reject an export source canonical dispatch would allow, and the
/// reverse (a tombstone's own tail already `retired`, but a later-epoch
/// receipt is canonically `purged`) can wrongly let one through.
///
/// PA47-50 (§O): all four branches (i)-(iv) of `main.rs`'s
/// `enforce_canonical_marker_barrier` — the dispatch `open`/`view` use — are
/// now replicated here (not just branch (i)/LC11 as before), fixing PA48's
/// gap: branches (ii)/(iii)/(iv) used to collapse into each call site's own
/// generic "raw absent -> KIO-E-PURGE-NOT-FOUND-001" fallback, giving a
/// `retired`-with-raw-missing or an unmarked (no marker at all) absence the
/// SAME error/exit as a perfectly ordinary `erased` absence — hiding real
/// store corruption from any automation watching exit_code/error_code to
/// tell "purge did this on purpose" apart from "something is broken, run
/// `kio repair verify-objects`". This is an INDEPENDENT implementation with
/// the SAME decision table (05 §3.5 L907's "入口を問わず...同じ" requirement)
/// — reusing `main.rs`'s own error constructors via `super::` for
/// byte-identical error codes/bodies — rather than a call-through to
/// `enforce_canonical_marker_barrier` itself, specifically so this keeps its
/// erase-receipt-parse-failure leniency below (erase receipts are
/// fsck-only/non-public; `enforce_canonical_marker_barrier`'s own version
/// does not have this leniency and must not gain a NEW way to break export
/// on a malformed record it has no business inspecting —
/// `missing_raw_is_dead_source_without_receipt_disclosure` below pins this).
///
/// `raw_present` is the caller's own already-resolved answer (mirroring
/// `enforce_canonical_marker_barrier`'s own parameter) — export determines
/// it differently at each of its 3 canonical call sites (PA50:
/// `preflight`/`preflight_in_dir`/`publish_all`'s per-file loop), so this
/// takes it rather than re-deriving it a second way. `barrier_blocks` below
/// keeps its existing in-progress-journal behavior unchanged — it is a
/// DIFFERENT check (an active-but-not-yet-terminal transaction) from the
/// canonical-marker dispatch above it.
fn check_purge_state(target: &ScopeTarget, raw_hash: &str, raw_present: bool) -> Result<()> {
    let state = PurgeState::open(&target.kio_dir)?;
    let tombstone_tail = state
        .read_tombstone(raw_hash)?
        .map(|record| record.tail().clone());
    // Erase receipts are fsck-only/non-public (unlike tombstones, whose parse
    // failure above still propagates): a receipt that fails to read or parse
    // simply does not participate in the canonical computation, rather than
    // breaking export on a malformed record it has no business inspecting.
    let receipt_tail = state
        .read_erase_receipt(raw_hash)
        .ok()
        .flatten()
        .map(|receipt| receipt.tail().clone());
    let canonical_event =
        canonical_final_event(tombstone_tail.as_ref(), receipt_tail.as_ref())?.map(|c| c.event);
    match canonical_event {
        Some(event) if event.kind == EventKind::Purged => {
            return Err(super::tombstone_error(json!({
                "raw_hash": raw_hash,
                "purged_at": event.at,
                "purged_reason": event.reason,
                "purged_in_commit": event.in_commit,
                "scope_path": target.kio_dir.display().to_string(),
            })));
        }
        Some(event) if event.kind == EventKind::Erased && !raw_present => {
            return Err(super::purge_not_found_error(target, raw_hash));
        }
        Some(event) if event.kind == EventKind::Retired && !raw_present => {
            return Err(super::retired_raw_missing_error(target, raw_hash));
        }
        None if !raw_present => {
            return Err(super::unmarked_missing_raw_error(target, raw_hash));
        }
        _ => {}
    }
    if state.barrier_blocks(raw_hash)? {
        return Err(KioError::new(
            "KIO-E-PURGE-NOT-FOUND-001",
            "export source is hidden by an in-progress purge barrier",
            json!({
                "raw_hash": raw_hash,
                "scope_path": target.kio_dir,
                "purge_state": "in_progress",
            }),
            ExitCode::PermanentFailure,
        ));
    }
    Ok(())
}

/// PA47-50 helper: the caller's own fresh answer to "does the raw CAS object
/// currently exist", fed to [`check_purge_state`].
fn raw_present_now(target: &ScopeTarget, raw_hash: &str) -> Result<bool> {
    match ObjectStore::new(&target.kio_dir).inspect_object(ObjectKind::Raw, raw_hash) {
        Ok(_) => Ok(true),
        Err(error) if super::is_store_not_found(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn stage_all(
    source: &ExportSource,
    files: Vec<PreflightFile>,
    destination: &DestinationDir,
) -> Result<Vec<StagedFile>> {
    let store = ObjectStore::new(&source.target.kio_dir);
    let mut staged = Vec::with_capacity(files.len());
    let result = (|| -> Result<()> {
        for file in files {
            validate_destination_handle(destination)?;
            // Not one of PA50's 3 named canonical-dispatch call sites — raw
            // presence was already verified moments ago by
            // `preflight_in_dir`, and `copy_object_to`'s own not-found
            // handling below independently covers a "went absent right now"
            // race; this recheck's purpose is the tombstone/journal-barrier
            // gate, which does not depend on `raw_present`.
            check_purge_state(&source.target, &file.source.raw_hash, true)?;
            let (temp, mut output) = create_private_temp(destination)?;
            let copied =
                match store.copy_object_to(ObjectKind::Raw, &file.source.raw_hash, &mut output) {
                    Ok(metadata) => metadata,
                    Err(error) if super::is_store_not_found(&error) => {
                        cleanup_one(destination, &temp);
                        return Err(missing_live_raw_error(
                            &source.target,
                            &file.source.raw_hash,
                        ));
                    }
                    Err(error) => {
                        cleanup_one(destination, &temp);
                        return Err(error);
                    }
                };
            if copied.size_bytes != file.size_bytes {
                cleanup_one(destination, &temp);
                return Err(KioError::new(
                    "KIO-E-STORE-CORRUPT-001",
                    "raw object changed after export preflight",
                    json!({ "raw_hash": file.source.raw_hash }),
                    ExitCode::PermanentFailure,
                ));
            }
            if let Err(error) = output.sync_all() {
                cleanup_one(destination, &temp);
                return Err(KioError::io(
                    error.to_string(),
                    destination.path.join(&temp).display().to_string(),
                ));
            }
            staged.push(StagedFile {
                preflight: file,
                temp,
                temp_file: output,
            });
        }
        Ok(())
    })();
    if let Err(error) = result {
        cleanup_staged(destination, &staged);
        return Err(error);
    }

    // A race that creates a conflicting case variant or changes a target after
    // staging is rejected before the first final publication.
    let rescanned = match preflight_in_dir(source, destination, true) {
        Ok(rescanned) => rescanned,
        Err(error) => {
            cleanup_staged(destination, &staged);
            return Err(error);
        }
    };
    if rescanned.len() != staged.len()
        || rescanned.iter().zip(&staged).any(|(fresh, staged)| {
            fresh.destination != staged.preflight.destination
                || fresh.overwritten != staged.preflight.overwritten
                || fresh.size_bytes != staged.preflight.size_bytes
        })
    {
        cleanup_staged(destination, &staged);
        return Err(unsafe_export_error(
            &destination.path,
            "destination changed after export staging",
        ));
    }
    Ok(staged)
}

fn create_private_temp(destination: &DestinationDir) -> Result<(OsString, File)> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    for attempt in 0..32_u8 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = OsString::from(format!(
            ".kio-export-tmp-{}-{nanos}-{sequence}-{attempt}",
            std::process::id()
        ));
        let mut options = cap_fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(windows)]
        {
            use cap_fs::OpenOptionsExt;
            use windows_sys::Win32::Foundation::GENERIC_READ;
            use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_GENERIC_WRITE};
            options.access_mode(GENERIC_READ | FILE_GENERIC_WRITE | DELETE);
        }
        match cap_fs::open(&destination.handle, Path::new(&name), &options) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(KioError::io(
                    error.to_string(),
                    destination.path.join(&name).display().to_string(),
                ));
            }
        }
    }
    Err(KioError::io(
        "could not allocate a unique export staging file",
        destination.path.display().to_string(),
    ))
}

fn publish_all(
    source: &ExportSource,
    destination: &DestinationDir,
    staged: Vec<StagedFile>,
    checkpoint: &ReadBarrierCheckpoint,
) -> Result<Value> {
    let mut published = Vec::new();
    let mut overwritten_count = 0_u64;
    let mut remaining = staged.into_iter();
    while let Some(file) = remaining.next() {
        // §I checkpoint 2 (LC54/LC55), combined with the per-raw_hash
        // canonical-marker recheck (LC8-14/item 3, PA50) immediately before
        // this file's atomic, irreversible publish (LC57).
        let raw_present = raw_present_now(&source.target, &file.preflight.source.raw_hash)?;
        if let Err(error) = ensure_no_active_sweep(&source.target.kio_dir).and_then(|()| {
            check_purge_state(&source.target, &file.preflight.source.raw_hash, raw_present)
                .and_then(|()| checkpoint.recheck())
        }) {
            cleanup_one(destination, &file.temp);
            for pending in remaining {
                cleanup_one(destination, &pending.temp);
            }
            if published.is_empty() {
                return Err(error);
            }
            return Ok(partial_output(
                source,
                &destination.path,
                published,
                overwritten_count,
                &file.preflight,
                error,
            ));
        }
        match publish_one(destination, &file) {
            Ok(backup) => {
                // PA24 (§E, U26): rename completed — re-resolve the target
                // raw's canonical state one more time (a second, independent
                // check from the pre-publish one above). A change detected
                // here (e.g. the purge that just completed on another
                // process, serialized against this one only by the
                // publication lock) rolls the publication back (PA25/26)
                // rather than leaving purged/hidden bytes sitting published.
                let post_publish_raw_present =
                    raw_present_now(&source.target, &file.preflight.source.raw_hash)?;
                if let Err(recheck_error) = ensure_no_active_sweep(&source.target.kio_dir)
                    .and_then(|()| {
                        check_purge_state(
                            &source.target,
                            &file.preflight.source.raw_hash,
                            post_publish_raw_present,
                        )
                    })
                    .and_then(|()| checkpoint.recheck())
                {
                    let error = rollback_published_file(
                        destination,
                        &file.preflight,
                        backup.as_deref(),
                        recheck_error,
                    );
                    for pending in remaining {
                        cleanup_one(destination, &pending.temp);
                    }
                    if published.is_empty() {
                        return Err(error);
                    }
                    return Ok(partial_output(
                        source,
                        &destination.path,
                        published,
                        overwritten_count,
                        &file.preflight,
                        error,
                    ));
                }
                overwritten_count += u64::from(file.preflight.overwritten);
                published.push(file.preflight);
            }
            Err((error, destination_may_have_changed)) => {
                cleanup_one(destination, &file.temp);
                for pending in remaining {
                    cleanup_one(destination, &pending.temp);
                }
                if published.is_empty() && !destination_may_have_changed {
                    return Err(error);
                }
                return Ok(partial_output(
                    source,
                    &destination.path,
                    published,
                    overwritten_count,
                    &file.preflight,
                    error,
                ));
            }
        }
    }
    if let Err(error) = sync_directory_handle(destination) {
        if published.is_empty() {
            return Err(error);
        }
        let failed = published
            .last()
            .expect("non-empty publications checked above")
            .clone();
        return Ok(partial_output(
            source,
            &destination.path,
            published,
            overwritten_count,
            &failed,
            error,
        ));
    }
    Ok(success_output(
        source,
        &destination.path,
        published,
        overwritten_count,
    ))
}

/// PA22/23: `publish_one`'s outcome on success — `Some(bak_name)` when an
/// existing file was evacuated first (so `publish_all`'s post-publish
/// recheck, PA24-26, knows there is a backup to export on rollback).
type PublishSuccess = Option<String>;

fn publish_one(
    destination: &DestinationDir,
    file: &StagedFile,
) -> std::result::Result<PublishSuccess, (KioError, bool)> {
    verify_open_file(
        &file.temp_file,
        &destination.path.join(&file.temp),
        &file.preflight.source.raw_hash,
        file.preflight.size_bytes,
    )
    .map_err(|error| (error, false))?;
    let path_at_commit = Path::new(&file.preflight.source.path_at_commit);
    let mut backup_name_opt: PublishSuccess = None;
    if file.preflight.overwritten {
        let metadata = cap_fs::stat(
            &destination.handle,
            path_at_commit,
            cap_fs::FollowSymlinks::No,
        )
        .map_err(|error| {
            (
                KioError::io(
                    error.to_string(),
                    file.preflight.destination.display().to_string(),
                ),
                false,
            )
        })?;
        validate_replaceable_metadata(&file.preflight.destination, &metadata)
            .map_err(|error| (error, false))?;
        // PA22: evacuate the existing file to `<name>.kio-export-bak` via a
        // no-replace move BEFORE publish — the intentional replacement is
        // performed only by this evacuation rename; the publish step right
        // below is always a no-replace claim of a (now-)empty name, uniform
        // with the non-overwrite branch.
        let bak_name = backup_name(&file.preflight.source.path_at_commit);
        match no_replace_move(destination, path_at_commit, Path::new(&bak_name)) {
            Ok(()) => {}
            Err(MoveOutcome::AlreadyExists) => {
                return Err((
                    export_conflict_error(
                        ExportConflictKind::StaleBackup,
                        &destination.path.join(&bak_name),
                    ),
                    false,
                ));
            }
            Err(MoveOutcome::Io(error)) => return Err((error, false)),
        }
        eprintln!(
            "export: evacuated existing '{}' to '{bak_name}'",
            file.preflight.source.path_at_commit
        );
        backup_name_opt = Some(bak_name);
    }
    // PA23: no-replace publish (uniform for both branches now — overwrite
    // publishes into the name evacuation just emptied, non-overwrite
    // publishes into a name preflight found absent). A competing third-party
    // file that appeared since is detected here, never silently replaced.
    match cap_fs::hard_link(
        &destination.handle,
        Path::new(&file.temp),
        &destination.handle,
        path_at_commit,
    ) {
        Ok(()) => {
            cap_fs::remove_file(&destination.handle, Path::new(&file.temp)).map_err(|error| {
                (
                    KioError::io(
                        error.to_string(),
                        destination.path.join(&file.temp).display().to_string(),
                    ),
                    true,
                )
            })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // PA23(b): a prior evacuation emptied the name; put it back
            // (best-effort — either outcome still terminates as a conflict,
            // 05 §3.5 L843-846's "この競合時は退避を元 path へ復帰").
            if let Some(bak_name) = &backup_name_opt {
                let _ = no_replace_move(destination, Path::new(bak_name.as_str()), path_at_commit);
            }
            return Err((
                export_conflict_error(ExportConflictKind::PublishRace, &file.preflight.destination),
                false,
            ));
        }
        Err(error) => {
            return Err((
                KioError::io(
                    error.to_string(),
                    file.preflight.destination.display().to_string(),
                ),
                backup_name_opt.is_some(),
            ));
        }
    }
    verify_exported_entry(
        destination,
        path_at_commit,
        &file.temp_file,
        &file.preflight.source.raw_hash,
        file.preflight.size_bytes,
    )
    .map_err(|error| (error, true))?;
    Ok(backup_name_opt)
}

/// PA24/25 (§E, U26): the just-published file is renamed (not unlinked) to
/// the decided quarantine name `<basename>.kio-export-quarantine`, and the
/// entity that landed under that quarantine name is verified by dev/inode
/// against the publication this function itself just observed (never a
/// separately re-fetched value — a check-then-delete without re-verifying on
/// the renamed entity would leave a TOCTOU window for a third party to swap
/// in between the check and the delete). A match means the quarantined
/// entity really is what this export just published, so it is safe to
/// delete outright and (PA26) export any evacuated backup; a mismatch means
/// a third party's file got quarantined by accident, so it is put back
/// (best-effort) and reported as a fresh conflict instead of the original
/// recheck failure.
fn rollback_published_file(
    destination: &DestinationDir,
    preflight: &PreflightFile,
    backup: Option<&str>,
    recheck_error: KioError,
) -> KioError {
    let path_at_commit = Path::new(&preflight.source.path_at_commit);
    let published_identity = match cap_fs::stat(
        &destination.handle,
        path_at_commit,
        cap_fs::FollowSymlinks::No,
    ) {
        Ok(metadata) => metadata,
        // Already gone by some other means — nothing left to roll back;
        // surface the original cause rather than inventing a new one.
        Err(_) => return recheck_error,
    };
    let quarantine = quarantine_name(&preflight.source.path_at_commit);
    match no_replace_move(destination, path_at_commit, Path::new(&quarantine)) {
        Ok(()) => {}
        Err(MoveOutcome::AlreadyExists) => {
            return export_conflict_error(
                ExportConflictKind::QuarantineRenameRace,
                &preflight.destination,
            );
        }
        Err(MoveOutcome::Io(_)) => return recheck_error,
    }
    let quarantined_identity = match cap_fs::stat(
        &destination.handle,
        Path::new(&quarantine),
        cap_fs::FollowSymlinks::No,
    ) {
        Ok(metadata) => metadata,
        Err(_) => {
            return export_conflict_error(
                ExportConflictKind::QuarantineRenameRace,
                &preflight.destination,
            );
        }
    };
    if !same_cap_file_identity(&published_identity, &quarantined_identity) {
        // Third-party swap between the stat above and this rename: put the
        // quarantined entity (whatever it now is) back, and do not touch it
        // further either way.
        let _ = no_replace_move(destination, Path::new(&quarantine), path_at_commit);
        return export_conflict_error(
            ExportConflictKind::QuarantineMismatch,
            &preflight.destination,
        );
    }
    eprintln!(
        "export: rolled back publish of '{}' via quarantine '{quarantine}' ({})",
        preflight.source.path_at_commit,
        recheck_error.error_code()
    );
    let _ = cap_fs::remove_file(&destination.handle, Path::new(&quarantine));
    if let Some(bak_name) = backup
        && let Err(error) =
            export_evacuated_backup(destination, bak_name, &preflight.source.path_at_commit)
    {
        return error;
    }
    // PA26: terminate with the SAME response a preflight encountering this
    // canonical state from the start would have given (tombstone / not-found
    // / journal-active) — not a fresh conflict, now that the rollback itself
    // succeeded.
    recheck_error
}

/// PA26 (§E, U26): export an evacuated `.kio-export-bak` file back to its
/// original name, via the identical quarantine-then-verify dance
/// [`rollback_published_file`] uses for the publication itself.
fn export_evacuated_backup(
    destination: &DestinationDir,
    bak_name: &str,
    path_at_commit: &str,
) -> Result<()> {
    let backup_identity = cap_fs::stat(
        &destination.handle,
        Path::new(bak_name),
        cap_fs::FollowSymlinks::No,
    )
    .map_err(|error| {
        KioError::io(
            error.to_string(),
            destination.path.join(bak_name).display().to_string(),
        )
    })?;
    let quarantine = quarantine_name(path_at_commit);
    match no_replace_move(destination, Path::new(bak_name), Path::new(&quarantine)) {
        Ok(()) => {}
        Err(MoveOutcome::AlreadyExists) => {
            return Err(export_conflict_error(
                ExportConflictKind::QuarantineRenameRace,
                &destination.path.join(bak_name),
            ));
        }
        Err(MoveOutcome::Io(error)) => return Err(error),
    }
    let quarantined_identity = cap_fs::stat(
        &destination.handle,
        Path::new(&quarantine),
        cap_fs::FollowSymlinks::No,
    )
    .map_err(|error| {
        KioError::io(
            error.to_string(),
            destination.path.join(&quarantine).display().to_string(),
        )
    })?;
    if !same_cap_file_identity(&backup_identity, &quarantined_identity) {
        let _ = no_replace_move(destination, Path::new(&quarantine), Path::new(bak_name));
        return Err(export_conflict_error(
            ExportConflictKind::BackupMismatch,
            &destination.path.join(bak_name),
        ));
    }
    match no_replace_move(
        destination,
        Path::new(&quarantine),
        Path::new(path_at_commit),
    ) {
        Ok(()) => Ok(()),
        Err(MoveOutcome::AlreadyExists) => Err(export_conflict_error(
            ExportConflictKind::ExportRenameRace,
            &destination.path.join(path_at_commit),
        )),
        Err(MoveOutcome::Io(error)) => Err(error),
    }
}

fn validate_destination_handle(destination: &DestinationDir) -> Result<()> {
    let metadata = cap_fs::Metadata::from_file(&destination.handle)
        .map_err(|error| KioError::io(error.to_string(), destination.path.display().to_string()))?;
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err(unsafe_export_error(
            &destination.path,
            "destination ancestor is not a real non-reparse directory",
        ));
    }
    #[cfg(windows)]
    {
        use cap_fs::_WindowsByHandle;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(unsafe_export_error(
                &destination.path,
                "destination ancestor is a reparse point",
            ));
        }
    }
    Ok(())
}

fn verify_exported_entry(
    destination: &DestinationDir,
    name: &Path,
    expected_file: &File,
    expected_hash: &str,
    expected_size: u64,
) -> Result<()> {
    let display = destination.path.join(name);
    let listed = cap_fs::stat(&destination.handle, name, cap_fs::FollowSymlinks::No)
        .map_err(|error| KioError::io(error.to_string(), display.display().to_string()))?;
    validate_replaceable_metadata(&display, &listed)?;

    let mut options = cap_fs::OpenOptions::new();
    options.read(true);
    configure_cap_no_follow(&mut options);
    let file = cap_fs::open(&destination.handle, name, &options)
        .map_err(|error| KioError::io(error.to_string(), display.display().to_string()))?;
    let opened = cap_fs::Metadata::from_file(&file)
        .map_err(|error| KioError::io(error.to_string(), display.display().to_string()))?;
    let expected = cap_fs::Metadata::from_file(expected_file)
        .map_err(|error| KioError::io(error.to_string(), display.display().to_string()))?;
    let after = cap_fs::stat(&destination.handle, name, cap_fs::FollowSymlinks::No)
        .map_err(|error| KioError::io(error.to_string(), display.display().to_string()))?;
    if !same_cap_file_identity(&listed, &opened)
        || !same_cap_file_identity(&opened, &after)
        || !same_cap_file_identity(&opened, &expected)
    {
        return Err(unsafe_export_error(
            &display,
            "exported file changed identity while it was verified",
        ));
    }
    verify_open_file(&file, &display, expected_hash, expected_size)
}

fn verify_open_file(
    file: &File,
    display: &Path,
    expected_hash: &str,
    expected_size: u64,
) -> Result<()> {
    let metadata = cap_fs::Metadata::from_file(file)
        .map_err(|error| KioError::io(error.to_string(), display.display().to_string()))?;
    validate_replaceable_metadata(display, &metadata)?;
    if metadata.len() != expected_size || metadata.len() > MAX_RAW_OBJECT_BYTES {
        return Err(KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "exported file size does not match its raw identity",
            json!({ "path": display, "raw_hash": expected_hash }),
            ExitCode::PermanentFailure,
        ));
    }

    let mut reader = file
        .try_clone()
        .map_err(|error| KioError::io(error.to_string(), display.display().to_string()))?;
    reader
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|error| KioError::io(error.to_string(), display.display().to_string()))?;
    let mut hasher = sha2::Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| KioError::io(error.to_string(), display.display().to_string()))?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > MAX_RAW_OBJECT_BYTES {
            return Err(KioError::new(
                "KIO-E-STORE-CORRUPT-001",
                "exported file exceeds the raw object limit",
                json!({ "path": display }),
                ExitCode::PermanentFailure,
            ));
        }
        hasher.update(&buffer[..count]);
    }
    let actual = format!("sha256:{}", kio_core::cas::lower_hex(&hasher.finalize()));
    if actual != expected_hash {
        return Err(KioError::new(
            "KIO-E-STORE-CORRUPT-001",
            "exported file bytes do not match their raw identity",
            json!({ "path": display, "expected": expected_hash, "actual": actual }),
            ExitCode::PermanentFailure,
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn same_cap_file_identity(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    use cap_fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_cap_file_identity(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    use cap_fs::_WindowsByHandle;
    left.volume_serial_number() == right.volume_serial_number()
        && left.file_index() == right.file_index()
}

#[cfg(not(any(unix, windows)))]
fn same_cap_file_identity(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

#[cfg(unix)]
fn configure_cap_no_follow(options: &mut cap_fs::OpenOptions) {
    use cap_fs::OpenOptionsExt;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    options.custom_flags(0x20_800);
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    options.custom_flags(0x104);
    let _ = options;
}

#[cfg(windows)]
fn configure_cap_no_follow(options: &mut cap_fs::OpenOptions) {
    use cap_fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(unix, windows)))]
fn configure_cap_no_follow(_options: &mut cap_fs::OpenOptions) {}

#[cfg(unix)]
fn sync_directory_handle(destination: &DestinationDir) -> Result<()> {
    // `cap_primitives` deliberately opens capability directories with
    // `O_PATH` on Linux. That descriptor is suitable for the relative
    // operations above, but `fsync(O_PATH)` fails with `EBADF`. Re-open `.`
    // relative to the held capability so durability is requested for the
    // same directory without falling back to its ambient pathname.
    let mut options = cap_fs::OpenOptions::new();
    options.read(true);
    let syncable = cap_fs::open(&destination.handle, Path::new("."), &options)
        .map_err(|error| KioError::io(error.to_string(), destination.path.display().to_string()))?;
    syncable
        .sync_all()
        .map_err(|error| KioError::io(error.to_string(), destination.path.display().to_string()))
}

#[cfg(not(unix))]
fn sync_directory_handle(_destination: &DestinationDir) -> Result<()> {
    Ok(())
}

fn cleanup_staged(destination: &DestinationDir, staged: &[StagedFile]) {
    for file in staged {
        cleanup_one(destination, &file.temp);
    }
}

fn cleanup_one(destination: &DestinationDir, name: &OsString) {
    let _ = cap_fs::remove_file(&destination.handle, Path::new(name));
}

fn success_output(
    source: &ExportSource,
    destination: &Path,
    files: Vec<PreflightFile>,
    overwritten_count: u64,
) -> Value {
    json!({
        "status": "exported",
        "source_kind": source.source_kind,
        "source_commit": source.source_commit,
        "destination": destination,
        "exported_count": files.len(),
        "overwritten_count": overwritten_count,
        "files": files.iter().map(exported_file_json).collect::<Vec<_>>(),
    })
}

fn partial_output(
    source: &ExportSource,
    destination: &Path,
    files: Vec<PreflightFile>,
    overwritten_count: u64,
    failed: &PreflightFile,
    error: KioError,
) -> Value {
    let mut output = json!({
        "status": "partial",
        "error_code": "KIO-E-COMMIT-EXPORT-PARTIAL-001",
        "source_kind": source.source_kind,
        "source_commit": source.source_commit,
        "destination": destination,
        "exported_count": files.len(),
        "overwritten_count": overwritten_count,
        "files": files.iter().map(exported_file_json).collect::<Vec<_>>(),
        "failed": [{
            "path": failed.destination,
            "path_at_commit": failed.source.path_at_commit,
            "raw_hash": failed.source.raw_hash,
            "error_code": error.error_code(),
            "message": error.message(),
        }],
    });
    super::set_exit_override(&mut output, ExitCode::PartialFailure);
    output
}

fn exported_file_json(file: &PreflightFile) -> Value {
    json!({
        "path": file.destination,
        "path_at_commit": file.source.path_at_commit,
        "raw_hash": file.source.raw_hash,
        "overwritten": file.overwritten,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use super::{
        ExportItem, ScopeTarget, check_purge_state, create_private_temp,
        destination_lstat_identity, missing_live_raw_error, no_replace_move, normalize_absolute,
        open_destination_dir, sync_directory_handle, validate_source_names,
    };

    fn entry_names(path: &std::path::Path) -> Vec<String> {
        let mut names = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn source_name_collision_is_case_and_normalization_insensitive() {
        let raw_hash = kio_core::cas::hash_bytes(b"same");
        let error = validate_source_names(&[
            ExportItem {
                path_at_commit: "Report.md".to_owned(),
                raw_hash: raw_hash.clone(),
            },
            ExportItem {
                path_at_commit: "report.md".to_owned(),
                raw_hash,
            },
        ])
        .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-COMMIT-EXPORT-UNSAFE-001");
    }

    #[test]
    fn destination_normalization_removes_parent_components() {
        struct NonInteractive;
        impl crate::context::Interaction for NonInteractive {
            fn confirm(&self, _: &str) -> kio_core::Result<bool> {
                Ok(false)
            }
            fn read_input(&self, _: usize) -> kio_core::Result<Vec<u8>> {
                Ok(Vec::new())
            }
            fn is_interactive(&self) -> bool {
                false
            }
            fn diagnostic(&self, _: &str) {}
        }
        let directory = tempfile::tempdir().unwrap();
        let context = crate::context::AppContext {
            working_directory: directory.path().to_path_buf(),
            interaction: std::sync::Arc::new(NonInteractive),
        };
        let normalized = crate::context::with_context(&context, || {
            normalize_absolute(std::path::Path::new("a/../b"))
        })
        .unwrap();
        assert_eq!(normalized, directory.path().join("b"));
    }

    #[cfg(unix)]
    #[test]
    fn destination_directory_sync_uses_a_syncable_capability_descriptor() {
        let temp = tempfile::TempDir::new().unwrap();
        let destination =
            open_destination_dir(&temp.path().canonicalize().unwrap(), false, None).unwrap();

        sync_directory_handle(&destination).unwrap();
    }

    #[test]
    fn missing_raw_is_dead_source_without_receipt_disclosure() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = kio_core::scope::Repository::init(temp.path()).unwrap();
        let kio_dir = repo.kio_dir().to_path_buf();
        let raw_hash = kio_core::cas::hash_bytes(b"erased");
        let state = kio_core::purge::PurgeState::open(&kio_dir).unwrap();
        let receipt = state.erase_receipt_path(&raw_hash).unwrap();
        fs::create_dir_all(receipt.parent().unwrap()).unwrap();
        // Deliberately invalid: export must not parse this fsck-only record.
        fs::write(&receipt, b"private receipt contents").unwrap();
        let target = ScopeTarget {
            repo_root: temp.path().to_path_buf(),
            kio_dir,
            scope_id: "scope-test".to_owned(),
        };

        // PA47-50/PA48(c): raw absent + no VALID marker (the garbage erase
        // receipt fails to parse and is swallowed, exactly like "no marker
        // at all") is LC14(a)'s unmarked-missing corruption suspicion, not a
        // silent pass-through — this is the corrected behavior the old
        // LC11-only `check_purge_state` could not distinguish from a normal
        // absence. The still-load-bearing property this test pins: the
        // resulting error must not disclose the fsck-private receipt's
        // existence or contents either way.
        let error = check_purge_state(&target, &raw_hash, false).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
        assert_eq!(error.exit_code(), kio_core::ExitCode::PermanentFailure);
        assert!(!error.context().to_string().contains("receipt"));
        assert!(
            !error
                .context()
                .to_string()
                .contains("private receipt contents")
        );

        let error = missing_live_raw_error(&target, &raw_hash);
        assert_eq!(error.error_code(), "KIO-E-PURGE-NOT-FOUND-001");
        assert_eq!(error.exit_code(), kio_core::ExitCode::PermanentFailure);
        assert!(!error.context().to_string().contains("receipt"));
    }

    // PA18 (§D, U25), `tasks/step4b-contract-tests-p2a.md` "PA18 dirfd
    // containment": when a `--to` whose identity was bound at
    // `validate_destination`'s containment check is swapped for a different
    // real directory before the open, the mismatch is rejected with
    // `KIO-E-CONFIG-USAGE-001` (exit 2) and nothing is written first.
    //
    // Deliberately not `#[cfg(unix)]`: `matches_opened`/
    // `destination_lstat_identity` have separate Windows bodies comparing a
    // different field pair (volume serial + file index) sourced from two
    // distinct OS queries, and `windows-security-r23` runs the workspace
    // suite. Swapping by rename rather than by symlink is what keeps the case
    // expressible on both — Windows symlink creation needs privileges.
    #[test]
    fn swapped_destination_identity_is_rejected_before_the_open_mutates_anything() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let destination_path = root.join("destination");
        let moved_path = root.join("moved-destination");
        let decoy_path = root.join("decoy");
        fs::create_dir(&destination_path).unwrap();
        fs::write(destination_path.join("original.md"), b"original").unwrap();
        fs::create_dir(&decoy_path).unwrap();
        fs::write(decoy_path.join("decoy.md"), b"decoy sentinel").unwrap();

        // Exactly the value `validate_destination` captures at its
        // containment-check moment and hands to the open below. Reaching it
        // (and `DestinationIdentity`) is why this lives in the module's own
        // `mod tests` rather than in `tests/` — both are private.
        let identity = destination_lstat_identity(&destination_path).unwrap();

        // Positive control: an unswapped destination must still open. Without
        // it, an identity that never compares equal — the lstat here and the
        // opened handle's fstat there are two different OS queries — would
        // satisfy the rejection assertion below for the wrong reason, while
        // rejecting every legitimate export.
        open_destination_dir(&destination_path, false, Some(&identity)).unwrap();

        // The swap PA18 exists for: a different REAL directory now answers to
        // the bound name. Two renames rather than one because Windows will
        // not rename onto an existing directory.
        fs::rename(&destination_path, &moved_path).unwrap();
        fs::rename(&decoy_path, &destination_path).unwrap();

        // The swap really did change the identity behind the name, so the
        // rejection below cannot be an artifact of the setup (a vanished
        // path, an unreadable one) landing on the same assertion by accident.
        assert_ne!(
            destination_lstat_identity(&destination_path).unwrap(),
            identity
        );

        // `create_missing: true` gives the call every opportunity to create
        // before it compares — the rejection must still come first.
        let error = open_destination_dir(&destination_path, true, Some(&identity)).unwrap_err();

        // Not the generic `unsafe_export_error` family (exit 1) every other
        // structural rejection in this module uses: PA18 wants the usage
        // error specifically, so automation can tell "this destination is not
        // the one that was validated" from a transient OS-level problem. The
        // message is pinned too, so a future unrelated usage error raised
        // from this same function cannot quietly satisfy this test.
        assert_eq!(error.error_code(), "KIO-E-CONFIG-USAGE-001");
        assert_eq!(error.exit_code(), kio_core::ExitCode::InvalidUsage);
        assert_eq!(
            error.message(),
            "export destination identity changed between validation and open"
        );

        // No mutation before the rejection, on either side of the swap.
        assert_eq!(entry_names(&destination_path), vec!["decoy.md".to_owned()]);
        assert_eq!(entry_names(&moved_path), vec!["original.md".to_owned()]);
    }

    #[cfg(unix)]
    #[test]
    fn held_destination_directory_prevents_parent_symlink_swap_escape() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let destination_path = root.join("destination");
        let moved_path = root.join("moved-destination");
        let outside_path = root.join("outside");
        fs::create_dir(&destination_path).unwrap();
        fs::create_dir(&outside_path).unwrap();
        fs::write(outside_path.join("doc.md"), b"outside sentinel").unwrap();

        let destination = open_destination_dir(&destination_path, false, None).unwrap();
        fs::rename(&destination_path, &moved_path).unwrap();
        symlink(&outside_path, &destination_path).unwrap();

        // PA23: publish is a no-replace move (`no_replace_move`), not a
        // replace-capable rename — `doc.md` must be absent under the held
        // directory for this to succeed, matching real publish's use (the
        // name was either never occupied or just evacuated to a `.kio-
        // export-bak` name by the same primitive).
        let (temp_name, mut staged) = create_private_temp(&destination).unwrap();
        staged.write_all(b"exported bytes").unwrap();
        staged.sync_all().unwrap();
        no_replace_move(
            &destination,
            std::path::Path::new(&temp_name),
            std::path::Path::new("doc.md"),
        )
        .unwrap();

        assert_eq!(
            fs::read(moved_path.join("doc.md")).unwrap(),
            b"exported bytes"
        );
        assert_eq!(
            fs::read(outside_path.join("doc.md")).unwrap(),
            b"outside sentinel"
        );
    }
}
