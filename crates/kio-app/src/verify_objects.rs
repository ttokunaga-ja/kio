//! Bounded object-store verification for `repair verify-objects`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use crate::commands::{EvidenceArgs, EvidenceCommand, EvidenceRetargetArgs, EvidenceVerifyArgs};

use kio_core::cas::{
    AccountedReadError, ChunkObject, ContentObjectKind, EmbeddingObject, MAX_RAW_OBJECT_BYTES,
    ObjectKind, ObjectStore, canonical_json_bytes, hash_bytes, is_hash, read_bounded_regular_file,
};
use kio_core::dag::{CommitObject, NormalizeRef, TreeObject};
use kio_core::gc::{GcSweepSession, read_active_marker, read_shallow_receipts};
use kio_core::portable::{portable_tag_digest64, portable_tag_name_has_unassigned};
use kio_core::purge::{
    CanonicalFinalEvent, EventKind, LifecycleEvent, MAX_PURGE_RECORD_BYTES, PurgeState,
    TombstoneMode, canonical_final_event,
};
use kio_core::scope::{Repository, names_jsonl_path, now_utc_seconds};
use kio_core::{KioError, Result};
use kio_index::fts::{RetargetCandidates, SourceIndexConnection};
use kio_pipeline::markdownize::{
    NormalizedInstanceIdentity, NormalizedInstanceManifest, ValidatedNormalizedInstance,
    load_validated_normalized_units_from_manifest, validate_normalized_instance,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::*;

const MAX_OBJECTS: usize = 1_000_000;
const MAX_VERIFIED_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const MAX_FINDINGS: usize = 1_024;
const MAX_AFFECTED_COMMITS: usize = 4_096;

/// A normalized unit authenticated by a reachable manifest, indexed by the
/// hash of its exact Markdown bytes. The manifest's immutable CAS pin is
/// verified before this index is populated; the content hash is deliberately
/// stable across non-content metadata changes such as `generated_at`.
#[derive(Clone)]
struct ValidatedNormalizedInstanceUnit {
    raw_hash: String,
    tool_profile_hash: String,
    r#gen: u64,
    unit: kio_pipeline::markdownize::NormalizedUnitObject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkPinnedUnitError {
    Unreachable,
    IdentityMismatch,
}

fn exact_pinned_content_unit_for_chunk<'a>(
    chunk: &ChunkObject,
    reachable_units: &'a BTreeMap<String, Vec<ValidatedNormalizedInstanceUnit>>,
) -> std::result::Result<&'a ValidatedNormalizedInstanceUnit, ChunkPinnedUnitError> {
    let candidates = reachable_units
        .get(&chunk.unit_content_hash)
        .ok_or(ChunkPinnedUnitError::Unreachable)?;
    candidates
        .iter()
        .find(|pinned_unit| {
            pinned_unit.raw_hash == chunk.raw_hash
                && pinned_unit.tool_profile_hash == chunk.tool_profile_hash
                && pinned_unit.r#gen == chunk.r#gen
                && pinned_unit.unit.unit_key == chunk.unit_key
        })
        .ok_or(ChunkPinnedUnitError::IdentityMismatch)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceStatus {
    Alive,
    Tombstoned,
    NotFound,
    ScopeUnreachable,
    Unverifiable,
    RegistryDuplicate,
}

impl EvidenceStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Alive => "alive",
            Self::Tombstoned => "tombstoned",
            Self::NotFound => "not_found",
            Self::ScopeUnreachable => "scope_unreachable",
            Self::Unverifiable => "unverifiable",
            Self::RegistryDuplicate => "registry_duplicate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct EvidenceVerifyResult {
    status: EvidenceStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    details: Value,
}

#[derive(Debug, Clone)]
struct EvidenceVerification {
    result: EvidenceVerifyResult,
    exit: ExitCode,
}

impl EvidenceVerification {
    fn new(
        status: EvidenceStatus,
        error_code: Option<&str>,
        details: Value,
        exit: ExitCode,
    ) -> Self {
        Self {
            result: EvidenceVerifyResult {
                status,
                error_code: error_code.map(str::to_owned),
                details,
            },
            exit,
        }
    }

    fn value(&self) -> Result<Value> {
        serde_json::to_value(&self.result).map_err(|error| KioError::schema(error.to_string()))
    }
}

const MAX_BATCH_INPUT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_BATCH_LINE_BYTES: usize = 64 * 1024;
const MAX_BATCH_RECORDS: usize = 4096;
const MAX_BATCH_SCOPES: usize = 256;
const MAX_BATCH_VERIFIED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub(super) fn run_evidence(args: EvidenceArgs) -> Result<Value> {
    match args.command {
        EvidenceCommand::Verify(args) => run_evidence_verify(args),
        EvidenceCommand::Retarget(args) => run_evidence_retarget(args),
    }
}

fn run_evidence_verify(args: EvidenceVerifyArgs) -> Result<Value> {
    if let Some(batch) = args.batch {
        return run_evidence_batch(&batch, args.strict);
    }
    let pointer_operand = args.pointer.expect("clap requires pointer or --batch");
    let strict = args.strict;
    let raw = read_pointer_input(vec![pointer_operand])?;
    if raw.starts_with("sha256:") || parse_object_uri(&raw)?.is_some() {
        return Err(KioError::invalid_usage(
            "evidence verify accepts only a pointer URI, inline JSON, or '-' stdin",
        ));
    }
    let pointer = parse_pointer_text(&raw)?;
    let verification = verify_pointer_for_cli(&pointer, strict)?;
    let mut output = verification.value()?;
    if verification.exit != ExitCode::Success {
        set_exit_override(&mut output, verification.exit);
    }
    Ok(output)
}

const MAX_RETARGET_VERIFIED_BYTES: u64 = 4 * 1024 * 1024 * 1024;

fn retarget_error(code: &'static str, message: &'static str) -> KioError {
    KioError::new(code, message, json!({}), ExitCode::PermanentFailure)
}

fn retarget_limit_error() -> KioError {
    retarget_error(
        "KIO-E-EVIDENCE-RETARGET-LIMIT-001",
        "evidence retarget exceeded a bounded read or candidate limit",
    )
}

fn retarget_not_found_error() -> KioError {
    retarget_error(
        "KIO-E-EVIDENCE-RETARGET-NOT-FOUND-001",
        "no exact Evidence retarget match exists at the requested commit",
    )
}

fn retarget_ambiguous_error() -> KioError {
    retarget_error(
        "KIO-E-EVIDENCE-RETARGET-AMBIG-001",
        "more than one exact Evidence retarget match exists at the requested commit",
    )
}

fn retarget_shallow_error(message: &'static str, commit_hash: &str) -> KioError {
    KioError::new(
        "KIO-E-COMMIT-SHALLOW-001",
        message,
        json!({"commit_hash": commit_hash}),
        ExitCode::PartialFailure,
    )
}

fn map_retarget_old_authority_error(error: KioError) -> KioError {
    if error.error_code() == "KIO-E-EVIDENCE-RETARGET-REQUIRED-001"
        || is_store_not_found(&error)
        || error.exit_code() == ExitCode::InvalidUsage
    {
        retarget_error(
            "KIO-E-STORE-CORRUPT-001",
            "old Evidence CAS authority is missing or has an invalid schema",
        )
    } else {
        error
    }
}

fn retarget_old_verification_error(
    verification: EvidenceVerification,
    pointer: &EvidencePointer,
) -> KioError {
    match verification.result.status {
        EvidenceStatus::Tombstoned | EvidenceStatus::NotFound => KioError::new(
            verification
                .result
                .error_code
                .as_deref()
                .unwrap_or("KIO-E-EVIDENCE-RETARGET-NOT-FOUND-001"),
            "old Evidence pointer is not retargetable",
            verification.result.details,
            ExitCode::PermanentFailure,
        ),
        EvidenceStatus::Unverifiable => {
            if verification
                .result
                .details
                .get("reason")
                .and_then(Value::as_str)
                == Some("commit_shallow")
            {
                retarget_shallow_error(
                    "old Evidence commit is shallow; retarget requires its tree",
                    &pointer.commit,
                )
            } else {
                retarget_error(
                    "KIO-E-STORE-CORRUPT-001",
                    "old Evidence attribution cannot be authenticated",
                )
            }
        }
        EvidenceStatus::Alive => retarget_error(
            "KIO-E-STORE-CORRUPT-001",
            "old Evidence verification unexpectedly reported alive as a failure",
        ),
        EvidenceStatus::ScopeUnreachable | EvidenceStatus::RegistryDuplicate => {
            retarget_not_found_error()
        }
    }
}

/// Exact, history-pinned Evidence reissue.  SQLite is used only to enumerate
/// bounded possible chunks; every authority used below comes from the target
/// tree, manifest/unit CAS, and chunk CAS.
fn run_evidence_retarget(args: EvidenceRetargetArgs) -> Result<Value> {
    if !is_hash(&args.at) {
        return Err(KioError::invalid_usage(
            "--at must be a full lowercase `sha256:` object hash",
        ));
    }
    let raw = read_pointer_input(vec![args.pointer])?;
    if raw.starts_with("sha256:") || parse_object_uri(&raw)?.is_some() {
        return Err(KioError::invalid_usage(
            "evidence retarget accepts only a pointer URI, inline JSON, or '-' stdin",
        ));
    }
    let pointer = parse_pointer_text(&raw)?;
    kio_core::cas::with_verified_cas_read_budget(MAX_RETARGET_VERIFIED_BYTES, || {
        run_evidence_retarget_bound(&pointer, &args.at)
    })
    .map(|(value, _)| value)
    .map_err(|error| {
        if matches!(
            error.error_code(),
            "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001" | "KIO-E-COMMIT-HISTORY-LIMIT-001"
        ) {
            retarget_limit_error()
        } else {
            error
        }
    })
}

fn run_evidence_retarget_bound(pointer: &EvidencePointer, at: &str) -> Result<Value> {
    let binding = bind_evidence_scope(pointer, true)?;
    let EvidenceScopeState::Ready {
        target,
        repo,
        checkpoint,
        source_index,
        ..
    } = &binding.state
    else {
        // Scope discovery has its own externally visible classifications; do
        // not turn an authority failure into an ordinary exact-match miss.
        return match &binding.state {
            EvidenceScopeState::Unavailable(result) => Err(KioError::new(
                result
                    .result
                    .error_code
                    .as_deref()
                    .unwrap_or("KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001"),
                "Evidence retarget scope is unavailable",
                result.result.details.clone(),
                result.exit,
            )),
            EvidenceScopeState::Ready { .. } => unreachable!(),
        };
    };
    // Authenticate and bound the old immutable inputs before the canonical
    // verifier can traverse their manifest units. Missing chunk CAS is a
    // retarget corruption condition, whereas a missing tree is left to the
    // verifier's existing shallow-state judgment.
    let old_commit = match repo.read_commit(&pointer.commit) {
        Ok(commit) => commit,
        Err(error) if is_store_not_found(&error) => {
            return Err(unresolvable_commit_pointer_error(pointer));
        }
        Err(error) if error.exit_code() == ExitCode::InvalidUsage => {
            return Err(retarget_error(
                "KIO-E-STORE-CORRUPT-001",
                "old Evidence commit CAS object has an invalid schema",
            ));
        }
        Err(error) => return Err(error),
    };
    let old_tree = match repo.read_tree(&old_commit.tree) {
        Ok(tree) => Some(tree),
        Err(error) if is_store_not_found(&error) => None,
        Err(error) if error.exit_code() == ExitCode::InvalidUsage => {
            return Err(retarget_error(
                "KIO-E-STORE-CORRUPT-001",
                "old Evidence tree CAS object has an invalid schema",
            ));
        }
        Err(error) => return Err(error),
    };
    let store = ObjectStore::new(&target.kio_dir);
    if let Some(marker_verification) =
        canonical_pointer_marker_verification(pointer, true, target, &store, checkpoint)?
    {
        binding.recheck()?;
        return Err(retarget_old_verification_error(
            marker_verification,
            pointer,
        ));
    }
    if let Some(tree) = &old_tree
        && let Some(entry) = canonical_pointer_tree_entry(tree, pointer)
        && let Some(normalize) = &entry.normalize
    {
        retarget_manifest_unit_preflight(&target.kio_dir, &pointer.raw_hash, normalize, false)?;
    }
    // Run the same canonical verifier used by `evidence verify` before doing
    // any retarget-specific reconstruction.  Optional presentation fields on
    // the input have no effect on this judgment.
    let canonical = verify_pointer_in_scope(pointer, true, target, repo, checkpoint, source_index)
        .map_err(map_retarget_old_authority_error)?;
    if canonical.result.status != EvidenceStatus::Alive {
        // A final retained binding check takes precedence over a stale
        // classification produced before a concurrent purge/index change.
        binding.recheck()?;
        return Err(retarget_old_verification_error(canonical, pointer));
    }
    let old_tree = old_tree.ok_or_else(|| {
        retarget_error(
            "KIO-E-STORE-CORRUPT-001",
            "canonical old Evidence verification accepted a missing tree",
        )
    })?;
    let old_entry = canonical_pointer_tree_entry(&old_tree, pointer).ok_or_else(|| {
        retarget_error(
            "KIO-E-STORE-CORRUPT-001",
            "old Evidence pointer has no canonical normalized tree entry",
        )
    })?;
    let old_normalize = old_entry
        .normalize
        .as_ref()
        .expect("filtered normalized entry");
    let old_chunk = match store.read_chunk(&pointer.chunk_hash) {
        Ok(chunk) => chunk,
        Err(error) if is_store_not_found(&error) => {
            return Err(retarget_error(
                "KIO-E-STORE-CORRUPT-001",
                "old Evidence pointer is missing its chunk CAS object",
            ));
        }
        Err(error) => return Err(error),
    };
    if old_chunk.raw_hash != pointer.raw_hash
        || old_chunk.tool_profile_hash != pointer.tool_profile_hash
        || old_chunk.r#gen != old_normalize.r#gen
    {
        return Err(retarget_error(
            "KIO-E-STORE-CORRUPT-001",
            "old Evidence pointer chunk does not match its canonical tree entry",
        ));
    }

    // `--at` was grammar-checked above; this is deliberately a direct object
    // read, never a ref/tag/HEAD resolver.
    let target_commit = read_exact_retarget_commit(&target.kio_dir, at)?;
    let target_tree = match repo.read_tree(&target_commit.tree) {
        Ok(tree) => tree,
        Err(error) if is_store_not_found(&error) => {
            kio_core::gc::validate_final_shallow_tree(&target.kio_dir, at, &target_commit.tree)?;
            return Err(retarget_shallow_error(
                "target Evidence commit is shallow; retarget requires its tree",
                at,
            ));
        }
        Err(error) if error.exit_code() == ExitCode::InvalidUsage => {
            return Err(retarget_error(
                "KIO-E-STORE-CORRUPT-001",
                "target Evidence tree contradicts its commit CAS object",
            ));
        }
        Err(error) => return Err(error),
    };
    let target_entry = target_tree
        .entries
        .iter()
        .find(|entry| entry.path == old_entry.path && entry.raw_hash == pointer.raw_hash)
        .ok_or_else(retarget_not_found_error)?;
    let target_normalize = target_entry
        .normalize
        .as_ref()
        .ok_or_else(retarget_not_found_error)?;
    retarget_manifest_unit_preflight(&target.kio_dir, &pointer.raw_hash, target_normalize, true)?;
    let target_units =
        pinned_done_units(repo, &pointer.raw_hash, target_normalize).map_err(|error| {
            if is_store_not_found(&error) {
                retarget_error(
                    "KIO-E-STORE-CORRUPT-001",
                    "target retarget manifest or normalized-unit CAS object is missing",
                )
            } else {
                error
            }
        })?;
    let candidates = match source_index
        .exact_retarget_candidates(
            &pointer.raw_hash,
            &target_normalize.tool_profile_hash,
            target_normalize.r#gen,
        )
        .map_err(index_to_kio)?
    {
        RetargetCandidates::Candidates(candidates) => candidates,
        RetargetCandidates::Overflow => return Err(retarget_limit_error()),
    };
    // Retain at most one large chunk body. Continue authenticating later
    // candidates so corruption still outranks ambiguity, but never let many
    // individually valid 128 MiB chunks accumulate in memory.
    let mut first_match = None;
    let mut ambiguous = false;
    for candidate in candidates {
        if !is_hash(&candidate.chunk_id) {
            return Err(retarget_error(
                "KIO-E-STORE-CORRUPT-001",
                "retarget candidate has a non-canonical chunk hash",
            ));
        }
        let chunk = match store.read_chunk(&candidate.chunk_id) {
            Ok(chunk) => chunk,
            Err(error) if is_store_not_found(&error) => {
                return Err(retarget_error(
                    "KIO-E-STORE-CORRUPT-001",
                    "retarget candidate is missing its chunk CAS object",
                ));
            }
            Err(error) => return Err(error),
        };
        if chunk.raw_hash != pointer.raw_hash
            || chunk.tool_profile_hash != target_normalize.tool_profile_hash
            || chunk.r#gen != target_normalize.r#gen
        {
            return Err(retarget_error(
                "KIO-E-STORE-CORRUPT-001",
                "retarget candidate chunk contradicts its SQLite target identity",
            ));
        }
        match check_publication_and_association_on_connection(
            source_index,
            repo,
            &candidate.chunk_id,
            at,
        )? {
            AssociationCheck::Ok => {}
            AssociationCheck::NotFound => continue,
            AssociationCheck::IndexRebuilding => return Err(index_rebuilding_error()),
        }
        if !target_units.iter().any(|unit| {
            unit.unit_key == chunk.unit_key
                && hash_bytes(unit.markdown.as_bytes()) == chunk.unit_content_hash
        }) || chunk.heading_path != old_chunk.heading_path
        {
            continue;
        }
        if first_match.is_some() {
            ambiguous = true;
        } else {
            first_match = Some((candidate.chunk_id, chunk));
        }
    }
    if ambiguous {
        return Err(retarget_ambiguous_error());
    }
    let (chunk_hash, chunk) = first_match.ok_or_else(retarget_not_found_error)?;
    binding.recheck()?;
    // Re-read the exact target object/tree after all candidate CAS reads.
    recheck_retarget_target(repo, at, &target_commit, &target_tree)?;
    let issued = issue_evidence_pointer(EvidencePointerIssueRequest {
        scope_id: pointer.scope_id.clone(),
        scope_path: Some(target.kio_dir.display().to_string()),
        commit: at.to_owned(),
        tree: Some(target_commit.tree.clone()),
        raw_hash: pointer.raw_hash.clone(),
        tool_profile_hash: target_normalize.tool_profile_hash.clone(),
        chunk_hash,
        path_at_commit: Some(target_entry.path.clone()),
        heading_path: Some(chunk.heading_path.clone()),
        section_id: chunk.section_id.clone(),
        byte_start: Some(chunk.byte_start),
        byte_end: Some(chunk.byte_end),
    })
    .map_err(search_to_kio)?;
    let verification = verify_pointer_for_cli(&issued, true)?;
    // Drift checks take precedence over a stale final-verifier result.  Only
    // classify a stable non-alive result after every retained authority still
    // denotes the command-start snapshot.
    binding.recheck()?;
    recheck_retarget_target(repo, at, &target_commit, &target_tree)?;
    if verification.result.status != EvidenceStatus::Alive {
        return Err(retarget_not_found_error());
    }
    Ok(json!({
        "schema": "kio.evidence.retarget", "schema_version": 1, "status": "retargeted",
        "target_commit": at, "retargeted_from": pointer, "new_pointer": issued,
        "match_method": "heading_path_exact",
    }))
}

/// Read `--at` as exactly one commit object. A missing hash or an object from
/// another namespace is an invalid operand; malformed bytes inside the commit
/// namespace are authenticated store corruption, not a spelling error.
fn read_exact_retarget_commit(kio_dir: &Path, at: &str) -> Result<CommitObject> {
    let object = ObjectStore::new(kio_dir)
        .read_by_hash(at)
        .map_err(|error| {
            if is_store_not_found(&error) {
                KioError::invalid_usage("--at does not name a commit object in this scope")
            } else {
                error
            }
        })?;
    if object.kind != ObjectKind::Commit {
        return Err(KioError::invalid_usage(
            "--at does not name a commit object in this scope",
        ));
    }
    let commit: CommitObject = serde_json::from_slice(&object.bytes).map_err(|_| {
        retarget_error(
            "KIO-E-STORE-CORRUPT-001",
            "target commit CAS object has an invalid schema",
        )
    })?;
    if commit.validate().is_err() {
        return Err(retarget_error(
            "KIO-E-STORE-CORRUPT-001",
            "target commit CAS object has an invalid schema",
        ));
    }
    Ok(commit)
}

/// Authenticate the manifest object and bound its declared unit count before
/// any unit CAS loader is reached. This is intentionally retarget-local: the
/// existing verifier retains its own historical degraded-result contract.
fn retarget_manifest_unit_preflight(
    kio_dir: &Path,
    raw_hash: &str,
    normalize: &NormalizeRef,
    missing_is_corrupt: bool,
) -> Result<()> {
    let store = ObjectStore::new(kio_dir);
    let bytes = match store.read_content_object_bytes(
        ContentObjectKind::Manifest,
        &normalize.manifest_hash,
        MAX_MANIFEST_OBJECT_READ_BYTES,
    ) {
        Ok(bytes) => bytes,
        Err(error) if is_store_not_found(&error) && missing_is_corrupt => {
            return Err(retarget_error(
                "KIO-E-STORE-CORRUPT-001",
                "target retarget manifest is missing",
            ));
        }
        Err(error) if is_store_not_found(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    if hash_bytes(&bytes) != normalize.manifest_hash {
        return Err(retarget_error(
            "KIO-E-STORE-CORRUPT-001",
            "retarget manifest bytes do not match their CAS hash",
        ));
    }
    let manifest: NormalizedInstanceManifest = serde_json::from_slice(&bytes).map_err(|_| {
        retarget_error(
            "KIO-E-STORE-CORRUPT-001",
            "retarget manifest is not a valid normalized-instance manifest",
        )
    })?;
    if manifest.raw_hash != raw_hash
        || manifest.tool_profile_hash != normalize.tool_profile_hash
        || manifest.r#gen != normalize.r#gen
    {
        return Err(retarget_error(
            "KIO-E-STORE-CORRUPT-001",
            "retarget manifest identity contradicts its tree normalization reference",
        ));
    }
    if manifest.units.len() > 4096 {
        return Err(retarget_limit_error());
    }
    Ok(())
}

fn recheck_retarget_target(
    repo: &Repository,
    at: &str,
    expected_commit: &CommitObject,
    expected_tree: &TreeObject,
) -> Result<()> {
    let commit = repo.read_commit(at).map_err(|_| batch_changed_error())?;
    let tree = repo
        .read_tree(&commit.tree)
        .map_err(|_| batch_changed_error())?;
    if &commit != expected_commit || &tree != expected_tree {
        return Err(batch_changed_error());
    }
    Ok(())
}

fn run_evidence_batch(path: &Path, strict: bool) -> Result<Value> {
    let bytes = read_bounded_regular_file(path, MAX_BATCH_INPUT_BYTES).map_err(|error| {
        if matches!(
            error.error_code(),
            "KIO-E-STORE-OBJECT-TOO-LARGE-001" | "KIO-E-STORE-OBJECT-OVERSIZED-001"
        ) {
            batch_limit_error(0, "file_bytes", MAX_BATCH_INPUT_BYTES as usize)
        } else {
            error
        }
    })?;
    let pointers = parse_batch_pointers(&bytes)?;
    let input_sha256 = hash_bytes(&bytes);

    let (verifications, _) =
        kio_core::cas::with_verified_cas_read_budget(MAX_BATCH_VERIFIED_BYTES, || {
            let bindings = bind_batch_scopes(&pointers, strict)?;
            let mut cache = BTreeMap::<Vec<u8>, EvidenceVerification>::new();
            let verifications = pointers
                .iter()
                .map(|pointer| verify_batch_row(pointer, strict, &bindings, &mut cache))
                .collect::<Result<Vec<_>>>()?;
            for binding in &bindings {
                binding.recheck()?;
            }
            Ok(verifications)
        })?;
    let mut counts = BTreeMap::<&str, usize>::new();
    for key in [
        "alive",
        "tombstoned",
        "not_found",
        "scope_unreachable",
        "unverifiable",
        "registry_duplicate",
    ] {
        counts.insert(key, 0);
    }
    let mut permanent = false;
    let mut retryable = false;
    let results = verifications
        .iter()
        .enumerate()
        .map(|(index, verification)| {
            let status = verification.result.status.as_str();
            *counts.get_mut(status).expect("closed status set") += 1;
            if strict {
                match verification.exit {
                    ExitCode::PermanentFailure => permanent = true,
                    ExitCode::PartialFailure => retryable = true,
                    _ => {}
                }
            } else if verification.exit == ExitCode::PartialFailure {
                retryable = true;
            }
            json!({"line": index + 1, "result": verification.result})
        })
        .collect::<Vec<_>>();
    let mut output = json!({
        "schema": "kio.evidence.batch-verify",
        "schema_version": 1,
        "input_sha256": input_sha256,
        "strict": strict,
        "results": results,
        "summary": {"total": pointers.len(), "status_counts": counts},
        "verified_count": pointers.len(),
    });
    let exit = if strict && permanent {
        ExitCode::PermanentFailure
    } else if retryable {
        ExitCode::PartialFailure
    } else {
        ExitCode::Success
    };
    if exit != ExitCode::Success {
        set_exit_override(&mut output, exit);
    }
    Ok(output)
}

fn verify_batch_row(
    pointer: &EvidencePointer,
    strict: bool,
    bindings: &[EvidenceScopeBinding],
    cache: &mut BTreeMap<Vec<u8>, EvidenceVerification>,
) -> Result<EvidenceVerification> {
    let key = evidence_pointer_cache_key(pointer)?;
    let binding = bindings
        .iter()
        .find(|binding| {
            binding.scope_id == pointer.scope_id && binding.scope_path == pointer.scope_path
        })
        .ok_or_else(batch_changed_error)?;
    let cached = cache.get(&key).cloned();
    let verification = match cached {
        Some(verification) => verification,
        None => match &binding.state {
            EvidenceScopeState::Unavailable(result) => result.clone(),
            EvidenceScopeState::Ready {
                target,
                repo,
                checkpoint,
                source_index,
                ..
            } => verify_pointer_in_scope(pointer, strict, target, repo, checkpoint, source_index)?,
        },
    };
    // Cache entries only avoid repeated object traversal. Every row, including
    // a duplicate cache hit, crosses the live scope/registry/index/read barrier.
    binding.recheck()?;
    cache.entry(key).or_insert_with(|| verification.clone());
    Ok(verification)
}

fn evidence_pointer_cache_key(pointer: &EvidencePointer) -> Result<Vec<u8>> {
    let value =
        serde_json::to_value(pointer).map_err(|error| KioError::schema(error.to_string()))?;
    canonical_json_bytes(&value)
}

fn bind_batch_scopes(
    pointers: &[EvidencePointer],
    strict: bool,
) -> Result<Vec<EvidenceScopeBinding>> {
    let mut bindings = Vec::new();
    let mut seen = BTreeSet::new();
    for pointer in pointers {
        let key = (pointer.scope_id.clone(), pointer.scope_path.clone());
        if !seen.insert(key) {
            continue;
        }
        bindings.push(bind_evidence_scope(pointer, strict)?);
    }
    Ok(bindings)
}

fn batch_changed_error() -> KioError {
    KioError::new(
        "KIO-E-EVIDENCE-BATCH-CHANGED-001",
        "evidence scope authority changed while verifying batch; retry",
        json!({}),
        ExitCode::PartialFailure,
    )
}

fn index_generation_for_evidence(kio_dir: &Path) -> Result<Option<String>> {
    let path = sqlite_path(kio_dir);
    if !path.exists() {
        return Err(index_rebuilding_error());
    }
    let conn = open_existing_source_index_connection(
        &path,
        ExistingSourceIndexOpenMode::ReadOnly,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    Ok(kio_index::fts::read_index_metadata(&conn)
        .map_err(index_to_kio)?
        .map(|metadata| metadata.index_generation))
}

fn parse_batch_pointers(bytes: &[u8]) -> Result<Vec<EvidencePointer>> {
    let records = if bytes.last() == Some(&b'\n') {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    };
    if records.is_empty() {
        return Err(batch_input_error(None, "empty"));
    }
    let mut pointers = Vec::new();
    let mut scopes = BTreeSet::new();
    for (index, line) in records.split(|byte| *byte == b'\n').enumerate() {
        let line_no = index + 1;
        if line.len() > MAX_BATCH_LINE_BYTES {
            return Err(batch_limit_error(
                line_no,
                "line_bytes",
                MAX_BATCH_LINE_BYTES,
            ));
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            return Err(batch_input_error(Some(line_no), "blank"));
        }
        let _: &str =
            std::str::from_utf8(line).map_err(|_| batch_input_error(Some(line_no), "utf8"))?;
        let pointer = serde_json::from_slice::<EvidencePointer>(line)
            .map_err(|_| batch_input_error(Some(line_no), "json"))?;
        scopes.insert(pointer.scope_id.clone());
        if scopes.len() > MAX_BATCH_SCOPES {
            return Err(batch_limit_error(line_no, "scope_count", MAX_BATCH_SCOPES));
        }
        pointers.push(pointer);
        if pointers.len() > MAX_BATCH_RECORDS {
            return Err(batch_limit_error(
                line_no,
                "record_count",
                MAX_BATCH_RECORDS,
            ));
        }
    }
    Ok(pointers)
}

fn batch_input_error(line: Option<usize>, reason: &str) -> KioError {
    KioError::new(
        "KIO-E-EVIDENCE-BATCH-INPUT-001",
        "invalid evidence batch input",
        json!({"line": line, "reason": reason}),
        ExitCode::InvalidUsage,
    )
}

fn batch_limit_error(line: usize, limit: &str, maximum: usize) -> KioError {
    KioError::new(
        "KIO-E-EVIDENCE-BATCH-LIMIT-001",
        "evidence batch exceeds a limit",
        json!({"line": line, "limit": limit, "maximum": maximum}),
        ExitCode::InvalidUsage,
    )
}

/// Read-only, content-free Evidence liveness check (08 §4.3). This deliberately
/// does not call `resolve_pointer_for_cli`: open/view may materialize an open-cache
/// file and return chunk text, both forbidden for verify.
///
/// PB64-68 (step4b-contract-tests-p2b.md §X, LC21 principle extended past
/// malformed-marker corruption to canonical-event *aggregation*): the
/// tombstone/erase-receipt dispatch below calls `enforce_canonical_marker_barrier`
/// (main.rs) — the SAME function `open`/`view`/`restore` use — instead of a
/// parallel, narrower single-marker (`read_tombstone(...).is_some()`) judgment.
/// A canonical `retired` tail (tombstone says `purged`, but the erase receipt
/// or a later tombstone event says `retired` with the greater lifecycle_epoch)
/// therefore resolves as alive here exactly as it does for `open`, per PB64's
/// LC10 worked example.
///
/// PB53-57 (§S): returns the full 6-value `status` union
/// (`alive|tombstoned|not_found|scope_unreachable|unverifiable|registry_duplicate`)
/// as a typed structured result wherever 08§4.3 specifies one. Only genuine
/// command-level conditions — sqlite.db unavailable (PB57), an active purge
/// journal (PB58, unchanged), and genuine store corruption (LC13/LC14 via the
/// shared canonical dispatch) — still propagate as a raw `KioError`.
///
/// Procedures 6a/6b (08§3.1, PB45-50/55) are implemented via the shared
/// `verify_point_in_time_attribution` (main.rs) — `unverifiable`'s
/// `manifest_missing` reason, the publication/config-association
/// ancestor-or-equal checks, and 6b's in_commit-scoped manifest-missing
/// downgrade (with resurrection-link fallback) all flow through the same
/// judgment `resolve_pointer_for_cli` (open/view/restore) uses — PB66's
/// structural requirement.
fn verify_pointer_for_cli(pointer: &EvidencePointer, strict: bool) -> Result<EvidenceVerification> {
    let binding = bind_evidence_scope(pointer, strict)?;
    match &binding.state {
        EvidenceScopeState::Unavailable(result) => Ok(result.clone()),
        EvidenceScopeState::Ready {
            target,
            repo,
            checkpoint,
            source_index,
            ..
        } => verify_pointer_in_scope(pointer, strict, target, repo, checkpoint, source_index),
    }
}

enum EvidenceScopeState {
    Ready {
        target: ScopeTarget,
        repo: Box<Repository>,
        checkpoint: ReadBarrierCheckpoint,
        index_generation: Option<String>,
        source_index: Box<SourceIndexConnection>,
    },
    Unavailable(EvidenceVerification),
}

struct EvidenceScopeBinding {
    scope_id: String,
    scope_path: Option<String>,
    state: EvidenceScopeState,
}

impl EvidenceScopeBinding {
    fn recheck(&self) -> Result<()> {
        match &self.state {
            EvidenceScopeState::Ready {
                target,
                checkpoint,
                index_generation,
                source_index,
                ..
            } => {
                let current =
                    resolve_evidence_scope_target(&self.scope_id, self.scope_path.as_deref())
                        .map_err(|error| {
                            // A second resolver invocation is the existing
                            // post-bind authority re-observation.  Only a known
                            // target/index drift becomes BATCH-CHANGED; an unsafe
                            // or unstable registry snapshot remains the original
                            // command-level error and never turns into a nested
                            // verification result.
                            if is_registry_snapshot_error(&error) {
                                error
                            } else {
                                batch_changed_error()
                            }
                        })?;
                if current.repo_root != target.repo_root || current.kio_dir != target.kio_dir {
                    return Err(batch_changed_error());
                }
                checkpoint.recheck()?;
                source_index
                    .recheck_source_identity()
                    .map_err(|_| batch_changed_error())?;
                let retained_generation = kio_index::fts::read_index_metadata(source_index)
                    .map_err(|_| batch_changed_error())?
                    .map(|metadata| metadata.index_generation);
                if retained_generation != *index_generation {
                    return Err(batch_changed_error());
                }
                check_index_generation_current(&target.kio_dir)?;
                if index_generation_for_evidence(&target.kio_dir)? != *index_generation {
                    return Err(batch_changed_error());
                }
            }
            EvidenceScopeState::Unavailable(expected) => {
                let current = unavailable_evidence_result(
                    &self.scope_id,
                    self.scope_path.as_deref(),
                    expected.exit,
                )?;
                if current.result != expected.result || current.exit != expected.exit {
                    return Err(batch_changed_error());
                }
            }
        }
        Ok(())
    }
}

fn unavailable_evidence_result(
    scope_id: &str,
    scope_path: Option<&str>,
    strict_exit: ExitCode,
) -> Result<EvidenceVerification> {
    match resolve_evidence_scope_target(scope_id, scope_path) {
        Err(error) if error.error_code() == "KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001" => {
            Ok(EvidenceVerification::new(
                EvidenceStatus::ScopeUnreachable,
                Some(error.error_code()),
                error.context().clone(),
                if strict_exit == ExitCode::Success {
                    ExitCode::Success
                } else {
                    ExitCode::PartialFailure
                },
            ))
        }
        Err(error) if error.error_code() == "KIO-E-REGISTRY-DUP-001" => {
            Ok(EvidenceVerification::new(
                EvidenceStatus::RegistryDuplicate,
                Some(error.error_code()),
                error.context().clone(),
                ExitCode::PartialFailure,
            ))
        }
        Err(error) if is_registry_snapshot_error(&error) => Err(error),
        _ => Err(batch_changed_error()),
    }
}

fn is_registry_snapshot_error(error: &KioError) -> bool {
    matches!(
        error.error_code(),
        "KIO-E-REGISTRY-SNAPSHOT-001" | "KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001"
    )
}

fn bind_evidence_scope(pointer: &EvidencePointer, strict: bool) -> Result<EvidenceScopeBinding> {
    let target =
        match resolve_evidence_scope_target(&pointer.scope_id, pointer.scope_path.as_deref()) {
            Ok(target) => target,
            // PB53: a structured `status`, not a raw command failure — exit 0
            // unless `--strict` (PB56 promotes it to 3).
            Err(error) if error.error_code() == "KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001" => {
                return Ok(EvidenceScopeBinding {
                    scope_id: pointer.scope_id.clone(),
                    scope_path: pointer.scope_path.clone(),
                    state: EvidenceScopeState::Unavailable(EvidenceVerification::new(
                        EvidenceStatus::ScopeUnreachable,
                        Some(error.error_code()),
                        error.context().clone(),
                        if strict {
                            ExitCode::PartialFailure
                        } else {
                            ExitCode::Success
                        },
                    )),
                });
            }
            // PB54: live registry duplicate — exit 3 regardless of --strict.
            Err(error) if error.error_code() == "KIO-E-REGISTRY-DUP-001" => {
                return Ok(EvidenceScopeBinding {
                    scope_id: pointer.scope_id.clone(),
                    scope_path: pointer.scope_path.clone(),
                    state: EvidenceScopeState::Unavailable(EvidenceVerification::new(
                        EvidenceStatus::RegistryDuplicate,
                        Some(error.error_code()),
                        error.context().clone(),
                        ExitCode::PartialFailure,
                    )),
                });
            }
            Err(error) => return Err(error),
        };
    // PB57: sqlite.db missing/unavailable is a command-level retryable error
    // (never a `status` field), independent of `--strict` — the verification
    // itself has not run, so there is nothing to report as a result.
    if !sqlite_path(&target.kio_dir).exists() {
        return Err(index_rebuilding_error());
    }
    // QB6 (step4b-contract-tests-p3b.md §A, 10 §3 L300-305): (0)
    // kio_format_version compatibility, checked before (1)/(3) below — this
    // used to open second, so a format-incompatible scope with an active
    // purge journal surfaced the lower-priority
    // `KIO-E-PURGE-JOURNAL-ACTIVE-001` instead of `KIO-E-STORE-VERSION-001`.
    let repo = Repository::open_for_search_read_only(&target.repo_root)?;
    // QB5/QB6/裁定1: shared (1)+(3) preflight pair. Evidence verify's whole
    // response IS existence information (08 §4.3 — it never returns body),
    // so checkpoint 2 below (LC54/LC55) gates every return point, not only a
    // single "success" path.
    let checkpoint = preflight_barrier_and_index(&target.kio_dir)?;
    let source_index = open_existing_source_index_connection(
        sqlite_path(&target.kio_dir),
        ExistingSourceIndexOpenMode::ReadOnly,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    let index_generation = kio_index::fts::read_index_metadata(&source_index)
        .map_err(index_to_kio)?
        .map(|metadata| metadata.index_generation);
    Ok(EvidenceScopeBinding {
        scope_id: pointer.scope_id.clone(),
        scope_path: pointer.scope_path.clone(),
        state: EvidenceScopeState::Ready {
            target,
            repo: Box::new(repo),
            checkpoint,
            index_generation,
            source_index: Box::new(source_index),
        },
    })
}

fn verify_pointer_in_scope(
    pointer: &EvidencePointer,
    strict: bool,
    target: &ScopeTarget,
    repo: &Repository,
    checkpoint: &ReadBarrierCheckpoint,
    source_index: &SourceIndexConnection,
) -> Result<EvidenceVerification> {
    let commit = match repo.read_commit(&pointer.commit) {
        Ok(commit) => commit,
        Err(error) if is_store_not_found(&error) => {
            return Err(unresolvable_commit_pointer_error(pointer));
        }
        Err(error) => return Err(error),
    };
    let store = ObjectStore::new(&target.kio_dir);
    let (commit_shallow, entry_gen, entry_normalize) = match repo.read_tree(&commit.tree) {
        Ok(tree) => {
            // PB42/43/44 (§O, verify side): select the entry whose
            // `normalize.tool_profile_hash` binds to the pointer's (not just
            // the first raw_hash match), tie-broken by UTF-8 byte-order-
            // minimal path (05 §1.7's `path_at_commit` rule) when more than
            // one entry shares that binding. Zero matching entries at all
            // short-circuits straight to KIO-E-STORE-CORRUPT-001 WITHOUT
            // consulting the tombstone/erase-receipt markers (PB44) — the DAG
            // is never rewritten by purge, so a genuinely absent entry is
            // corruption, not an explainable purge outcome.
            let candidates = tree
                .entries
                .iter()
                .filter(|entry| entry.raw_hash == pointer.raw_hash)
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                return Err(store_corrupt_pointer_entry_missing_error(pointer));
            }
            let Some(entry) = canonical_pointer_tree_entry(&tree, pointer) else {
                return Err(invalid_pointer_identity_error(pointer));
            };
            let entry_gen = entry.normalize.as_ref().map(|normalize| normalize.r#gen);
            (false, entry_gen, entry.normalize.clone())
        }
        Err(error) if is_store_not_found(&error) => {
            kio_core::gc::validate_final_shallow_tree(
                &target.kio_dir,
                &pointer.commit,
                &commit.tree,
            )?;
            (true, None, None)
        }
        Err(error) => return Err(error),
    };

    if let Some(marker_verification) =
        canonical_pointer_marker_verification(pointer, strict, target, &store, checkpoint)?
    {
        return Ok(marker_verification);
    }

    let chunk = match store.read_chunk(&pointer.chunk_hash) {
        Ok(chunk) => chunk,
        Err(error) if is_store_not_found(&error) => {
            return Err(KioError::new(
                "KIO-E-EVIDENCE-RETARGET-REQUIRED-001",
                "chunk object is unavailable for this tool profile; retarget required",
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
    if chunk.raw_hash != pointer.raw_hash
        || chunk.tool_profile_hash != pointer.tool_profile_hash
        || entry_gen.is_some_and(|r#gen| chunk.r#gen != r#gen)
    {
        return Err(invalid_pointer_identity_error(pointer));
    }

    // 08 §3.1 procedures 6a/6b (PB45-50/55, §P/§Q; item 2 of this session's
    // task): point-in-time attribution, shared with `resolve_pointer_for_cli`
    // (main.rs, PB66's structural requirement) rather than a parallel
    // judgment. `PB45`'s unit-status check is folded into this shared path
    // rather than a bespoke best-effort read here.
    let mut manifest_missing = false;
    if !commit_shallow && let Some(normalize) = &entry_normalize {
        match verify_point_in_time_attribution(
            target,
            repo,
            pointer,
            normalize,
            &chunk,
            Some(source_index),
        )? {
            PointInTimeAttribution::Alive => {}
            PointInTimeAttribution::ManifestMissing => manifest_missing = true,
            PointInTimeAttribution::NotFound => {
                return checkpoint.finish(EvidenceVerification::new(
                    EvidenceStatus::NotFound,
                    Some("KIO-E-PURGE-NOT-FOUND-001"),
                    not_found_verify_details(target, &pointer.raw_hash),
                    if strict {
                        ExitCode::PermanentFailure
                    } else {
                        ExitCode::Success
                    },
                ));
            }
            PointInTimeAttribution::StoreCorrupt => {
                return Err(store_corrupt_pointer_entry_missing_error(pointer));
            }
            PointInTimeAttribution::IndexRebuilding => return Err(index_rebuilding_error()),
        }
    }

    // Defense-in-depth: re-run the identical canonical dispatch after the
    // chunk read, in case a purge raced the resolution above (mirrors the
    // pre-existing double-check this function has always performed).
    if let Err(marker_error) = enforce_canonical_marker_barrier(target, &pointer.raw_hash, true) {
        return dispatch_marker_barrier_error(
            checkpoint,
            target,
            &pointer.raw_hash,
            marker_error,
            strict,
        );
    }

    // PB55/56 (§S/§N): the closed `unverifiable` reason union.
    if strict && (commit_shallow || manifest_missing) {
        let reason = if manifest_missing {
            "manifest_missing"
        } else {
            "commit_shallow"
        };
        // PB41/56: `commit_shallow` alone is retryable (unshallow may
        // resolve it, exit 3); `manifest_missing` is permanent (exit 4).
        let exit = if manifest_missing {
            ExitCode::PermanentFailure
        } else {
            ExitCode::PartialFailure
        };
        return checkpoint.finish(EvidenceVerification::new(
            EvidenceStatus::Unverifiable,
            None,
            json!({
                "scope_id": pointer.scope_id,
                "scope_path": target.kio_dir.display().to_string(),
                "commit": pointer.commit,
                "raw_hash": pointer.raw_hash,
                "tool_profile_hash": pointer.tool_profile_hash,
                "chunk_hash": pointer.chunk_hash,
                "reason": reason,
            }),
            exit,
        ));
    }

    checkpoint.finish(EvidenceVerification::new(
        EvidenceStatus::Alive,
        None,
        json!({
            "scope_id": pointer.scope_id,
            "scope_path": target.kio_dir.display().to_string(),
            "commit": pointer.commit,
            "raw_hash": pointer.raw_hash,
            "tool_profile_hash": pointer.tool_profile_hash,
            "chunk_hash": pointer.chunk_hash,
            "commit_shallow": commit_shallow,
            "manifest_missing": manifest_missing,
        }),
        ExitCode::Success,
    ))
}

/// Shared canonical raw/marker judgment used by ordinary Evidence verification
/// and retarget's bounded old-manifest preflight. `None` means the raw may
/// proceed to chunk/manifest attribution; `Some` is the canonical terminal
/// tombstoned/not-found result with the checkpoint already finished.
fn canonical_pointer_marker_verification(
    pointer: &EvidencePointer,
    strict: bool,
    target: &ScopeTarget,
    store: &ObjectStore,
    checkpoint: &ReadBarrierCheckpoint,
) -> Result<Option<EvidenceVerification>> {
    let raw_present = match store.inspect_object(ObjectKind::Raw, &pointer.raw_hash) {
        Ok(_) => true,
        Err(error) if is_store_not_found(&error) => false,
        Err(error) => return Err(error),
    };
    match enforce_canonical_marker_barrier(target, &pointer.raw_hash, raw_present) {
        Ok(()) => Ok(None),
        Err(marker_error) => dispatch_marker_barrier_error(
            checkpoint,
            target,
            &pointer.raw_hash,
            marker_error,
            strict,
        )
        .map(Some),
    }
}

/// Canonical historical tree selection for Evidence.  Presentation fields in
/// a pointer are intentionally absent: authority is raw hash, profile hash,
/// and UTF-8 byte-minimum logical path only.
fn canonical_pointer_tree_entry<'a>(
    tree: &'a TreeObject,
    pointer: &EvidencePointer,
) -> Option<&'a kio_core::dag::TreeEntry> {
    tree.entries
        .iter()
        .filter(|entry| {
            entry.raw_hash == pointer.raw_hash
                && entry.normalize.as_ref().is_some_and(|normalize| {
                    normalize.tool_profile_hash == pointer.tool_profile_hash
                })
        })
        .min_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()))
}

/// PB64-68: translate an `enforce_canonical_marker_barrier` `Err` into the
/// SAME `status` shapes verify has always returned for tombstoned/not_found —
/// this is the one place that bridges main.rs's shared canonical dispatch
/// (which returns `KioError`, appropriate for `open`/`view`/`restore`'s "fail
/// the whole command" contract) into verify's "return a structured status,
/// don't fail the command" contract (08 §4.3). `KIO-E-STORE-CORRUPT-001`
/// (LC13/LC14 — a `retired` marker or no marker at all with the raw absent)
/// is not a `status` value in the 6-value union (§S) and propagates as-is.
fn dispatch_marker_barrier_error(
    checkpoint: &ReadBarrierCheckpoint,
    target: &ScopeTarget,
    raw_hash: &str,
    error: KioError,
    strict: bool,
) -> Result<EvidenceVerification> {
    // PB56 (§S/§N exit table): under `--strict`, `tombstoned`/`not_found`
    // are permanent (exit 4) — both are `KIO-E-PURGE-*-001` error-code
    // marker states, never transient. Non-strict always stays exit 0 (a
    // structured status, not a command failure).
    let exit = if strict {
        ExitCode::PermanentFailure
    } else {
        ExitCode::Success
    };
    match error.error_code() {
        "KIO-E-PURGE-TOMBSTONED-001" => checkpoint.finish(EvidenceVerification::new(
            EvidenceStatus::Tombstoned,
            Some("KIO-E-PURGE-TOMBSTONED-001"),
            tombstoned_verify_details(error.context().clone()),
            exit,
        )),
        "KIO-E-PURGE-NOT-FOUND-001" => checkpoint.finish(EvidenceVerification::new(
            EvidenceStatus::NotFound,
            Some("KIO-E-PURGE-NOT-FOUND-001"),
            not_found_verify_details(target, raw_hash),
            exit,
        )),
        _ => Err(error),
    }
}

/// PB57: sqlite.db is missing/unavailable — the same retryable code
/// `check_index_generation_current`'s cursor-mismatch branch and search's
/// exclusion path already use (main.rs), reused here rather than minted
/// fresh so verify and search agree on what "index rebuilding" means.
fn index_rebuilding_error() -> KioError {
    KioError::new(
        "KIO-E-INDEX-REBUILDING-001",
        "the search index is unavailable (not yet built or mid-rebuild); retry",
        json!({}),
        ExitCode::PartialFailure,
    )
}

/// PB44: zero tree entries name this pointer's raw_hash at this commit — the
/// DAG is never rewritten by purge (02-philosophy §2.4), so this is genuine
/// corruption, not an explainable purge outcome. Distinct from
/// `invalid_pointer_identity_error` (entries exist but none bind to this
/// pointer's tool_profile_hash) — same downstream `not_found`-class handling
/// (08 §3.1 step 8), different corruption code per PB44.
fn store_corrupt_pointer_entry_missing_error(pointer: &EvidencePointer) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        "no tree entry at this commit names the pointer's raw_hash; the DAG is never \
         rewritten by purge, so this is corruption rather than a purge outcome",
        json!({
            "commit": pointer.commit,
            "raw_hash": pointer.raw_hash,
        }),
        ExitCode::PermanentFailure,
    )
}

fn tombstoned_verify_details(mut tombstone: Value) -> Value {
    if let Some(object) = tombstone.as_object_mut() {
        object.remove("status");
    }
    tombstone
}

fn not_found_verify_details(target: &ScopeTarget, raw_hash: &str) -> Value {
    json!({
        "raw_hash": raw_hash,
        "scope_path": target.kio_dir.display().to_string(),
    })
}

#[derive(Debug, Default, Serialize)]
pub struct CheckedObjects {
    pub raw: u64,
    pub chunks: u64,
    pub trees: u64,
    pub commits: u64,
    pub normalized_instances: u64,
    /// Immutable, content-addressed normalized unit objects verified from
    /// `objects/normalized_unit_objects/`.
    pub normalized_units: u64,
    /// PB01 (§A, U39): `objects/embeddings/` CAS objects verified (digest +
    /// declared-length/finite-vector).
    pub embeddings: u64,
    /// PB02 (§B, U39): `objects/manifests/` CAS objects verified (content
    /// hash).
    pub manifests: u64,
    /// PB02 (§B, U39): `objects/toollocks/` CAS objects verified (canonical
    /// JCS content hash — 03 §5.2).
    pub toollocks: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ObjectFinding {
    pub kind: String,
    pub object_hash: String,
    pub reason: String,
    pub affected_commits: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct VerifyObjectsReport {
    pub status: String,
    pub checked: CheckedObjects,
    pub repaired_raw_count: u64,
    pub repaired_commit_hash: Option<String>,
    pub dead_by_tombstone_count: u64,
    pub dead_by_erase_receipt_count: u64,
    /// Reachable pre-resurrection normalize references with purge-explained absence.
    pub purge_explained_missing_history_count: u64,
    pub remaining_findings: Vec<ObjectFinding>,
    pub findings_truncated: bool,
    pub external_pointers_may_be_affected: bool,
    #[serde(skip)]
    #[cfg_attr(not(test), allow(dead_code))]
    verified_bytes: u64,
    #[serde(skip)]
    #[cfg_attr(not(test), allow(dead_code))]
    inventoried_objects: usize,
}

impl VerifyObjectsReport {
    #[must_use]
    pub fn has_remaining_findings(&self) -> bool {
        !self.remaining_findings.is_empty() || self.findings_truncated
    }

    /// Repository binding failed before inventory. Report the blocked boundary
    /// without relaxing the CAS capability or inspecting its linked target.
    pub(super) fn namespace_unavailable(error: &KioError) -> Self {
        let namespace = error.context()["namespace"].as_str().unwrap_or("unknown");
        Self {
            status: "verification_blocked".to_owned(),
            checked: CheckedObjects::default(),
            repaired_raw_count: 0,
            repaired_commit_hash: None,
            dead_by_tombstone_count: 0,
            dead_by_erase_receipt_count: 0,
            purge_explained_missing_history_count: 0,
            remaining_findings: vec![ObjectFinding {
                kind: "object_namespace_unavailable".to_owned(),
                object_hash: String::new(),
                reason: format!("objects/{namespace}: {}", error.message()),
                affected_commits: Vec::new(),
            }],
            findings_truncated: false,
            external_pointers_may_be_affected: true,
            verified_bytes: 0,
            inventoried_objects: 0,
        }
    }
}

pub fn verify_objects(repo: &Repository) -> Result<VerifyObjectsReport> {
    verify_objects_with_limits(repo, VerifyLimits::default())
}

#[derive(Debug, Clone, Copy)]
struct VerifyLimits {
    max_objects: usize,
    max_verified_bytes: u64,
}

impl Default for VerifyLimits {
    fn default() -> Self {
        Self {
            max_objects: MAX_OBJECTS,
            max_verified_bytes: MAX_VERIFIED_BYTES,
        }
    }
}

fn verify_objects_with_limits(
    repo: &Repository,
    limits: VerifyLimits,
) -> Result<VerifyObjectsReport> {
    let store = repo.object_store();
    let purge = PurgeState::open(repo.kio_dir())?;
    let invocation_time = now_utc_seconds();
    let mut state = State {
        max_objects: limits.max_objects,
        max_verified_bytes: limits.max_verified_bytes,
        ..State::default()
    };
    match purge.read_journal() {
        Ok(Some(_)) => {
            state.finding("purge_incomplete", "", "active purge journal", &[]);
            return Ok(finish_report(state, 0, None));
        }
        Ok(None) => {}
        Err(error) => {
            state.finding("purge_journal_corrupt", "", &error.to_string(), &[]);
            return Ok(finish_report(state, 0, None));
        }
    }
    // GC's receipt-first sweep is a crash-recoverable state machine, not an
    // ordinary missing-object condition.  Read the two shared canonical
    // records before any repair-capable fsck work.  A malformed receipt or
    // marker is never treated as absence.  A valid active marker returns one
    // bounded retryable finding and leaves every tree/receipt untouched; the
    // sweeper's recovery path owns completing or rolling back the transition.
    let shallow_receipts = match read_shallow_receipts(repo.kio_dir()) {
        Ok(receipts) => receipts,
        Err(error) => {
            state.finding("gc_shallow_receipt_corrupt", "", &error.to_string(), &[]);
            return Ok(finish_report(state, 0, None));
        }
    };
    let active_gc_marker = match read_active_marker(repo.kio_dir()) {
        Ok(marker) => marker,
        Err(error) => {
            state.finding("gc_sweep_marker_corrupt", "", &error.to_string(), &[]);
            return Ok(finish_report(state, 0, None));
        }
    };
    // Validate final receipt bindings even for commits that are currently not
    // graph-reachable.  Apart from detecting a forged receipt early, this
    // records when historical tree entries are intentionally unavailable: a
    // chunk can no longer be re-attested against its normalized unit after the
    // sole tree that named it was shallow-discarded.
    let mut final_shallow_commits = BTreeSet::new();
    if active_gc_marker.is_none() {
        match kio_core::gc::validated_final_shallow_receipts(repo.kio_dir()) {
            Ok(receipts) => {
                final_shallow_commits.extend(receipts.into_keys());
            }
            Err(error) => {
                state.finding("gc_shallow_receipt_corrupt", "", &error.to_string(), &[]);
                return Ok(finish_report(state, 0, None));
            }
        }
    }
    if let Some(marker) = active_gc_marker {
        // Canonical marker syntax is not sufficient to call a transition
        // recoverable: validate its phase against descriptor-bound receipt and
        // tree state.  This is the same state machine used by `kio gc --yes`,
        // so impossible combinations are corruption rather than a misleading
        // retryable finding.
        let session = match GcSweepSession::bind(repo.root().to_path_buf()) {
            Ok(session) => session,
            Err(error) => {
                state.finding("gc_sweep_marker_corrupt", "", &error.to_string(), &[]);
                return Ok(finish_report(state, 0, None));
            }
        };
        if let Err(error) = session.validate_recovery_state(&marker) {
            state.finding("gc_sweep_marker_corrupt", "", &error.to_string(), &[]);
            return Ok(finish_report(state, 0, None));
        }
        if let Err(error) = crate::gc::validate_marker_index_binding_for_observer(&session, &marker)
        {
            state.finding("gc_sweep_marker_corrupt", "", &error.to_string(), &[]);
            return Ok(finish_report(state, 0, None));
        }
        // The marker is strictly canonical, but its frozen commit/tree pairs
        // must still bind to the current immutable commit objects.  Receipts
        // for a frozen pair may be absent (before receipt), coexist with the
        // tree (after receipt, before unlink), or explain its absence (after
        // unlink). Non-frozen receipts remain final shallow state and may not
        // coexist with their trees.
        for candidate in &marker.candidates {
            match repo.read_commit(&candidate.commit_hash) {
                Ok(commit) if commit.tree == candidate.tree_hash => {}
                Ok(_) => {
                    state.finding(
                        "gc_sweep_marker_corrupt",
                        &candidate.commit_hash,
                        "GC marker tree does not match its commit",
                        &[],
                    );
                    return Ok(finish_report(state, 0, None));
                }
                Err(error) => {
                    state.finding(
                        "gc_sweep_marker_corrupt",
                        &candidate.commit_hash,
                        &error.to_string(),
                        &[],
                    );
                    return Ok(finish_report(state, 0, None));
                }
            }
        }
        for (commit_hash, receipt) in &shallow_receipts {
            let commit = match repo.read_commit(commit_hash) {
                Ok(commit) => commit,
                Err(error) => {
                    state.finding(
                        "gc_shallow_receipt_corrupt",
                        commit_hash,
                        &error.to_string(),
                        &[],
                    );
                    return Ok(finish_report(state, 0, None));
                }
            };
            if commit.tree != receipt.tree_hash {
                state.finding(
                    "gc_shallow_receipt_corrupt",
                    commit_hash,
                    "shallow receipt tree does not match the commit tree",
                    &[],
                );
                return Ok(finish_report(state, 0, None));
            }
            if !marker.is_frozen_pair(commit_hash, &receipt.tree_hash)
                && repo.read_tree(&receipt.tree_hash).is_ok()
            {
                state.finding(
                    "gc_shallow_receipt_corrupt",
                    commit_hash,
                    "markerless shallow receipt coexists with its tree",
                    &[],
                );
                return Ok(finish_report(state, 0, None));
            }
        }
        // The frozen target set is the authoritative bound for transitional
        // receipt/tree combinations; do not speculate or repair.
        state.finding(
            "gc_sweep_incomplete",
            "",
            &format!(
                "active GC shallow sweep {}; retry after crash recovery",
                marker.sweep_id
            ),
            &[],
        );
        return Ok(finish_report(state, 0, None));
    }
    let mut repairs_allowed = true;

    let raw_hashes = inventory(repo.kio_dir(), "raw", &mut state)?;
    if state.exceeded_bounds {
        return Ok(finish_limit_report(state));
    }
    if state.unsafe_namespace {
        return Ok(finish_report(state, 0, None));
    }
    let chunk_hashes = inventory(repo.kio_dir(), "chunks", &mut state)?;
    if state.exceeded_bounds {
        return Ok(finish_limit_report(state));
    }
    if state.unsafe_namespace {
        return Ok(finish_report(state, 0, None));
    }
    let tree_hashes = inventory(repo.kio_dir(), "trees", &mut state)?;
    if state.exceeded_bounds {
        return Ok(finish_limit_report(state));
    }
    if state.unsafe_namespace {
        return Ok(finish_report(state, 0, None));
    }
    let commit_hashes = inventory(repo.kio_dir(), "commits", &mut state)?;
    if state.exceeded_bounds {
        return Ok(finish_limit_report(state));
    }
    if state.unsafe_namespace {
        return Ok(finish_report(state, 0, None));
    }
    let tombstone_hashes = marker_inventory(repo.kio_dir(), "tombstones", &mut state)?;
    if state.exceeded_bounds {
        return Ok(finish_limit_report(state));
    }
    if state.unsafe_namespace {
        return Ok(finish_report(state, 0, None));
    }
    let receipt_hashes = marker_inventory(repo.kio_dir(), "purge/erase-receipts", &mut state)?;
    if state.exceeded_bounds {
        return Ok(finish_limit_report(state));
    }
    if state.unsafe_namespace {
        return Ok(finish_report(state, 0, None));
    }

    // PB01/PB02 (§A/§B, 10 §7.5.1 L489): embedding/manifest/toollock CAS
    // objects join the verification closure. These are inventoried
    // standalone (not tree-cross-referenced — that requires
    // `normalize.manifest_hash`, PB04's prerequisite schema field, not
    // implemented this session; see this module's PB64-68 doc comment for
    // the analogous, explicitly-documented gap on the resolver side).
    verify_embeddings(&store, repo.kio_dir(), &mut state)?;
    if state.exceeded_bounds {
        return Ok(finish_limit_report(state));
    }
    verify_content_hash_closure(
        &store,
        repo.kio_dir(),
        ContentObjectKind::Manifest,
        "manifest_corrupt",
        &mut state,
    )?;
    if state.exceeded_bounds {
        return Ok(finish_limit_report(state));
    }
    verify_content_hash_closure(
        &store,
        repo.kio_dir(),
        ContentObjectKind::Toollock,
        "toollock_corrupt",
        &mut state,
    )?;
    if state.exceeded_bounds {
        return Ok(finish_limit_report(state));
    }
    verify_content_hash_closure(
        &store,
        repo.kio_dir(),
        ContentObjectKind::NormalizedUnit,
        "normalized_unit_corrupt",
        &mut state,
    )?;
    if state.exceeded_bounds {
        return Ok(finish_limit_report(state));
    }

    // §C (U41, PB07-09): names.jsonl full-line verification + canonical tag
    // ref correspondence.
    verify_names_jsonl(repo.kio_dir(), &mut state)?;
    if state.exceeded_bounds {
        return Ok(finish_limit_report(state));
    }

    let mut corrupt_raws = BTreeMap::<String, String>::new();
    let mut verified_raws = BTreeSet::<String>::new();
    for hash in &raw_hashes {
        match store.inspect_object_accounted(ObjectKind::Raw, hash) {
            Ok(metadata) => {
                state.checked.raw += 1;
                verified_raws.insert(hash.clone());
                state.add_bytes(metadata.size_bytes);
                if state.exceeded_bounds {
                    return Ok(finish_limit_report(state));
                }
            }
            Err(failure) => {
                state.add_bytes(failure.consumed_bytes);
                if state.exceeded_bounds {
                    return Ok(finish_limit_report(state));
                }
                corrupt_raws.insert(hash.clone(), failure.error.to_string());
            }
        }
    }

    let mut chunks = BTreeMap::<String, ChunkObject>::new();
    for hash in &chunk_hashes {
        match store.read_chunk_accounted(hash) {
            Ok((chunk, bytes)) => {
                state.checked.chunks += 1;
                state.add_bytes(bytes);
                if state.exceeded_bounds {
                    return Ok(finish_limit_report(state));
                }
                chunks.insert(hash.clone(), chunk);
            }
            Err(failure) => {
                state.add_bytes(failure.consumed_bytes);
                if state.exceeded_bounds {
                    return Ok(finish_limit_report(state));
                }
                state.finding("chunk_corrupt", hash, &failure.error.to_string(), &[]);
            }
        }
    }

    let mut trees = BTreeMap::<String, TreeObject>::new();
    for hash in &tree_hashes {
        match store.read_object_accounted(ObjectKind::Tree, hash) {
            Ok((object, verified_bytes)) => {
                state.add_bytes(verified_bytes);
                if state.exceeded_bounds {
                    return Ok(finish_limit_report(state));
                }
                match serde_json::from_slice::<TreeObject>(&object.bytes)
                    .map_err(|error| KioError::schema(error.to_string()))
                    .and_then(|tree| {
                        tree.validate()?;
                        Ok(tree)
                    }) {
                    Ok(tree) => {
                        state.checked.trees += 1;
                        trees.insert(hash.clone(), tree);
                    }
                    Err(error) => state.finding("tree_corrupt", hash, &error.to_string(), &[]),
                }
            }
            Err(failure) => {
                state.add_bytes(failure.consumed_bytes);
                if state.exceeded_bounds {
                    return Ok(finish_limit_report(state));
                }
                state.finding("tree_corrupt", hash, &failure.error.to_string(), &[]);
            }
        }
    }

    let mut commits = BTreeMap::<String, CommitObject>::new();
    for hash in &commit_hashes {
        match store.read_object_accounted(ObjectKind::Commit, hash) {
            Ok((object, verified_bytes)) => {
                state.add_bytes(verified_bytes);
                if state.exceeded_bounds {
                    return Ok(finish_limit_report(state));
                }
                match serde_json::from_slice::<CommitObject>(&object.bytes)
                    .map_err(|error| KioError::schema(error.to_string()))
                    .and_then(|commit| {
                        commit.validate()?;
                        Ok(commit)
                    }) {
                    Ok(commit) => {
                        state.checked.commits += 1;
                        commits.insert(hash.clone(), commit);
                    }
                    Err(error) => state.finding("commit_corrupt", hash, &error.to_string(), &[]),
                }
            }
            Err(failure) => {
                state.add_bytes(failure.consumed_bytes);
                if state.exceeded_bounds {
                    return Ok(finish_limit_report(state));
                }
                state.finding("commit_corrupt", hash, &failure.error.to_string(), &[]);
            }
        }
    }

    let reachable = reachable_commits(repo, &commits, &mut state)?;
    let current_head = repo.head_commit_hash()?;
    if state.exceeded_bounds {
        return Ok(finish_limit_report(state));
    }
    let mut raw_affected = BTreeMap::<String, BTreeSet<String>>::new();
    // The path-named `normalized_units/` directory is only a mutable current
    // cache. Every historical tree instead names a manifest CAS object, which
    // in turn pins immutable unit CAS bodies. After that pin has been
    // validated, chunks select the exact Markdown body by its content hash,
    // never by the non-unique
    // (raw, profile, generation, unit_key) tuple.
    let mut pinned_normalized = BTreeMap::<String, ValidatedNormalizedInstance>::new();
    let mut reachable_units = BTreeMap::<String, Vec<ValidatedNormalizedInstanceUnit>>::new();
    let mut prepared_references = BTreeSet::<String>::new();
    let mut raw_substitutable_prepared = BTreeSet::<String>::new();
    let mut image_references = BTreeSet::<String>::new();
    let mut prepared_affected = BTreeMap::<String, BTreeSet<String>>::new();
    let mut image_affected = BTreeMap::<String, BTreeSet<String>>::new();
    let mut recovery_paths = BTreeMap::<String, Vec<PathBuf>>::new();
    for commit_hash in &reachable {
        let Some(commit) = commits.get(commit_hash) else {
            continue;
        };
        let Some(tree) = trees.get(&commit.tree) else {
            continue;
        };
        for entry in &tree.entries {
            let affected = raw_affected.entry(entry.raw_hash.clone()).or_default();
            if affected.len() < MAX_AFFECTED_COMMITS {
                affected.insert(commit_hash.clone());
            }
            let path = repo.root().join(&entry.path);
            let paths = recovery_paths.entry(entry.raw_hash.clone()).or_default();
            if paths.len() < MAX_AFFECTED_COMMITS && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }

    let explained_manifests = explained_historical_manifests(
        &purge,
        &commits,
        &trees,
        &reachable,
        current_head.as_deref(),
        &invocation_time,
    );
    // A final shallow receipt proves that a historical tree was deliberately
    // discarded, while manifests and normalized-unit CAS objects remain.  The
    // chunk ledger is mutable operational metadata, so its introduction fields
    // must not decide which chunk closures may be omitted.  Instead, when at
    // least one strict final shallow receipt exists, add every independently
    // hash-verified retained manifest closure to the exact-unit index.  A
    // chunk still has to match raw/profile/generation/unit-key/content-hash;
    // an unrelated ledger row alone cannot make it reachable.
    if !final_shallow_commits.is_empty() {
        index_retained_manifest_units(
            &store,
            repo.kio_dir(),
            &explained_manifests,
            &mut reachable_units,
            &mut state,
        )?;
        if state.exceeded_bounds {
            return Ok(finish_limit_report(state));
        }
    }
    let dead_raws = raw_affected
        .keys()
        .filter(|raw_hash| {
            valid_dead_terminal(
                &verified_raws,
                &purge,
                raw_hash,
                &commits,
                &trees,
                &reachable,
                &invocation_time,
            )
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    for commit_hash in &reachable {
        let Some(commit) = commits.get(commit_hash) else {
            continue;
        };
        let Some(tree) = trees.get(&commit.tree) else {
            if final_shallow_commits.contains(commit_hash) {
                continue;
            }
            state.finding(
                "missing_tree",
                &commit.tree,
                "commit references a missing tree without a valid final shallow receipt",
                std::slice::from_ref(commit_hash),
            );
            continue;
        };
        if shallow_receipts.contains_key(commit_hash) {
            state.finding(
                "gc_shallow_receipt_corrupt",
                &commit.tree,
                "markerless shallow receipt coexists with its tree",
                std::slice::from_ref(commit_hash),
            );
            continue;
        }
        for entry in &tree.entries {
            if dead_raws.contains(&entry.raw_hash) {
                continue;
            }
            if let Some(reference) = &entry.normalize {
                let manifest_hash = &reference.manifest_hash;
                if !pinned_normalized.contains_key(manifest_hash) {
                    match load_pinned_normalized_instance(
                        &store,
                        repo.kio_dir(),
                        &entry.raw_hash,
                        reference,
                        &mut state,
                    ) {
                        Ok(instance) => {
                            state.checked.normalized_instances += 1;
                            pinned_normalized.insert(manifest_hash.clone(), instance);
                        }
                        Err(PinnedNormalizedError::Missing)
                            if explained_manifests.get(manifest_hash).is_some_and(
                                |(raw, exact)| raw == &entry.raw_hash && exact == reference,
                            ) =>
                        {
                            match validate_purge_explained_missing_closure(
                                &store,
                                repo.kio_dir(),
                                &entry.raw_hash,
                                reference,
                                &mut state,
                            ) {
                                Ok(()) => state.purge_explained_missing_history_count += 1,
                                Err(error) => state.finding(
                                    "normalized_closure_corrupt",
                                    manifest_hash,
                                    &error.to_string(),
                                    std::slice::from_ref(commit_hash),
                                ),
                            }
                            if state.exceeded_bounds {
                                return Ok(finish_limit_report(state));
                            }
                            continue;
                        }
                        Err(error) => {
                            state.finding(
                                "normalized_closure_corrupt",
                                manifest_hash,
                                &error.to_string(),
                                std::slice::from_ref(commit_hash),
                            );
                            continue;
                        }
                    }
                    if state.exceeded_bounds {
                        return Ok(finish_limit_report(state));
                    }
                }
                if let Some(instance) = pinned_normalized.get(manifest_hash) {
                    for manifest_entry in &instance.manifest.units {
                        prepared_references.insert(manifest_entry.prepared_hash.clone());
                        let affected = prepared_affected
                            .entry(manifest_entry.prepared_hash.clone())
                            .or_default();
                        if affected.len() < MAX_AFFECTED_COMMITS {
                            affected.insert(commit_hash.clone());
                        }
                        if manifest_entry.prepared_hash == instance.manifest.raw_hash {
                            raw_substitutable_prepared.insert(manifest_entry.prepared_hash.clone());
                        }
                    }
                    for (_manifest_entry, unit) in instance
                        .manifest
                        .units
                        .iter()
                        .filter(|entry| entry.status == kio_pipeline::markdownize::UnitStatus::Done)
                        .zip(&instance.units)
                    {
                        // `load_pinned_normalized_instance` has validated the
                        // manifest/body correspondence. Index the exact body
                        // only after its full CAS binding passed validation;
                        // a content hash stays stable when volatile metadata
                        // differs across a resurrection.
                        let content_hash = hash_bytes(unit.markdown.as_bytes());
                        reachable_units.entry(content_hash).or_default().push(
                            ValidatedNormalizedInstanceUnit {
                                raw_hash: instance.manifest.raw_hash.clone(),
                                tool_profile_hash: instance.manifest.tool_profile_hash.clone(),
                                r#gen: instance.manifest.r#gen,
                                unit: unit.clone(),
                            },
                        );
                        for hash in &unit.owned_image_hashes {
                            image_references.insert(hash.clone());
                            let affected = image_affected.entry(hash.clone()).or_default();
                            if affected.len() < MAX_AFFECTED_COMMITS {
                                affected.insert(commit_hash.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    for prepared_hash in prepared_references {
        state.count_object();
        if state.exceeded_bounds {
            return Ok(finish_limit_report(state));
        }
        match verify_prepared_reference(
            &store,
            &prepared_hash,
            raw_substitutable_prepared.contains(&prepared_hash)
                && verified_raws.contains(&prepared_hash),
        ) {
            Ok(bytes) => state.add_bytes(bytes),
            Err(failure) => {
                state.add_bytes(failure.consumed_bytes);
                if state.exceeded_bounds {
                    return Ok(finish_limit_report(state));
                }
                let affected = prepared_affected
                    .get(&prepared_hash)
                    .map_or_else(Vec::new, |commits| commits.iter().cloned().collect());
                state.finding(
                    "prepared_corrupt",
                    &prepared_hash,
                    &failure.error.to_string(),
                    &affected,
                )
            }
        }
        if state.exceeded_bounds {
            return Ok(finish_limit_report(state));
        }
    }
    for image_hash in image_references {
        state.count_object();
        if state.exceeded_bounds {
            return Ok(finish_limit_report(state));
        }
        match store.inspect_content_accounted(ContentObjectKind::Image, &image_hash) {
            Ok(metadata) => state.add_bytes(metadata.size_bytes),
            Err(failure) => {
                state.add_bytes(failure.consumed_bytes);
                if state.exceeded_bounds {
                    return Ok(finish_limit_report(state));
                }
                let affected = image_affected
                    .get(&image_hash)
                    .map_or_else(Vec::new, |commits| commits.iter().cloned().collect());
                state.finding(
                    "image_corrupt",
                    &image_hash,
                    &failure.error.to_string(),
                    &affected,
                )
            }
        }
        if state.exceeded_bounds {
            return Ok(finish_limit_report(state));
        }
    }

    let marker_hashes = tombstone_hashes
        .union(&receipt_hashes)
        .cloned()
        .collect::<BTreeSet<_>>();
    // (raw_hash, marker_kind, republication commit hash, republication
    // commit `created_at`) queued for a `retired` append once the
    // corresponding physical-object pass completes (LC27/LC35: verified raw
    // + a ref-reachable, ancestor-respecting, raw-carrying republication
    // commit of the canonical purged/erased event -- R23-08).
    let mut retirements_to_backfill = Vec::<(String, TombstoneMode, String, String)>::new();
    for raw_hash in marker_hashes {
        if raw_affected.contains_key(&raw_hash) {
            continue;
        }
        let lookup = canonical_lookup(
            &purge,
            &raw_hash,
            &commits,
            &trees,
            &reachable,
            &invocation_time,
        );
        if let Some(reason) = lookup.tombstone_error {
            state.finding("tombstone_corrupt", &raw_hash, &reason, &[]);
        }
        if let Some(reason) = lookup.receipt_error {
            state.finding("erase_receipt_corrupt", &raw_hash, &reason, &[]);
        }
        let Some(canonical) = lookup.canonical else {
            continue;
        };
        let raw_alive = verified_raws.contains(&raw_hash);
        match (canonical.event.kind, raw_alive) {
            // LC34/F34: canonical = retired is normal coexistence (resurrection).
            (EventKind::Retired, _) => {}
            (EventKind::Purged, false) => state.dead_by_tombstone_count += 1,
            (EventKind::Erased, false) => state.dead_by_erase_receipt_count += 1,
            (EventKind::Purged | EventKind::Erased, true) => {
                // LC35/LC36 (§F): verified raw + canonical purged/erased.
                // Backfill `retired` only when a ref-reachable, ancestor-
                // respecting republication commit exists; otherwise this is
                // an incomplete purge (exit 3), not a corruption, and is
                // never silently "fixed" by resurrecting the tombstone.
                if repairs_allowed && !state.exceeded_bounds {
                    match find_republication_commit(
                        &raw_hash,
                        &canonical.event.in_commit,
                        &commits,
                        &trees,
                        &reachable,
                    ) {
                        Some((republication_commit, republication_created_at)) => {
                            retirements_to_backfill.push((
                                raw_hash.clone(),
                                canonical.marker_kind,
                                republication_commit,
                                republication_created_at,
                            ));
                        }
                        None => state.finding(
                            "purge_incomplete",
                            &raw_hash,
                            "verified raw exists but no causal republication commit was found (09 §5.2: re-run kio purge --raw-hash to complete idempotently)",
                            &[],
                        ),
                    }
                } else {
                    state.finding(
                        "purge_incomplete",
                        &raw_hash,
                        "resurrection backfill suppressed while purge state is active or corrupt",
                        &[],
                    );
                }
            }
        }
    }

    repairs_allowed &= !state.exceeded_bounds;
    let mut staged_raws = Vec::<(String, Vec<u8>)>::new();
    for (raw_hash, affected) in &raw_affected {
        let affected = affected
            .iter()
            .take(MAX_AFFECTED_COMMITS)
            .cloned()
            .collect::<Vec<_>>();
        if verified_raws.contains(raw_hash) {
            // §F (LC34-LC38): canonical final event basis, not marker-pair
            // presence — a tombstone and erase receipt coexisting is not
            // itself a finding (§C computes canonical over both).
            let lookup = canonical_lookup(
                &purge,
                raw_hash,
                &commits,
                &trees,
                &reachable,
                &invocation_time,
            );
            if let Some(reason) = lookup.tombstone_error {
                state.finding("tombstone_corrupt", raw_hash, &reason, &affected);
            }
            if let Some(reason) = lookup.receipt_error {
                state.finding("erase_receipt_corrupt", raw_hash, &reason, &affected);
            }
            if let Some(canonical) = lookup.canonical {
                match canonical.event.kind {
                    EventKind::Retired => {} // LC34: normal coexistence.
                    EventKind::Purged | EventKind::Erased => {
                        if repairs_allowed && !state.exceeded_bounds {
                            match find_republication_commit(
                                raw_hash,
                                &canonical.event.in_commit,
                                &commits,
                                &trees,
                                &reachable,
                            ) {
                                Some((republication_commit, republication_created_at)) => {
                                    retirements_to_backfill.push((
                                        raw_hash.clone(),
                                        canonical.marker_kind,
                                        republication_commit,
                                        republication_created_at,
                                    ));
                                }
                                None => state.finding(
                                    "purge_incomplete",
                                    raw_hash,
                                    "verified raw exists but no causal republication commit was found (09 §5.2: re-run kio purge --raw-hash to complete idempotently)",
                                    &affected,
                                ),
                            }
                        } else {
                            state.finding(
                                "purge_incomplete",
                                raw_hash,
                                "resurrection backfill suppressed while purge state is active or corrupt",
                                &affected,
                            );
                        }
                    }
                }
            }
            continue;
        }
        // Raw is *not* verified-present: a canonical `purged`/`erased` marker
        // is the normal, explained dead terminal (10 §7.5.1). A canonical
        // `retired` here means the marker asserts the raw should be alive
        // again but it is missing — that is not a valid explanation (LC13's
        // resolver-side rule is the same corruption class), so it falls
        // through to the recovery/`missing_raw` path below like an unmarked
        // absence would.
        let lookup = canonical_lookup(
            &purge,
            raw_hash,
            &commits,
            &trees,
            &reachable,
            &invocation_time,
        );
        if let Some(reason) = lookup.tombstone_error {
            state.finding("tombstone_corrupt", raw_hash, &reason, &affected);
            continue;
        }
        if let Some(reason) = lookup.receipt_error {
            state.finding("erase_receipt_corrupt", raw_hash, &reason, &affected);
            continue;
        }
        match lookup.canonical.map(|canonical| canonical.event.kind) {
            Some(EventKind::Purged) => {
                state.dead_by_tombstone_count += 1;
                continue;
            }
            Some(EventKind::Erased) => {
                state.dead_by_erase_receipt_count += 1;
                continue;
            }
            Some(EventKind::Retired) | None => {}
        }
        if !repairs_allowed {
            state.finding(
                "missing_raw",
                raw_hash,
                "raw recovery suppressed while purge state is active or corrupt",
                &affected,
            );
            continue;
        }
        let mut recovered = false;
        for path in recovery_paths.get(raw_hash).into_iter().flatten() {
            let remaining = state
                .max_verified_bytes
                .saturating_sub(state.verified_bytes);
            match recover_raw(path, raw_hash, remaining)? {
                RawRecovery::Missing(bytes) => {
                    state.add_bytes(bytes);
                    if state.exceeded_bounds {
                        break;
                    }
                }
                RawRecovery::Candidate(bytes) => {
                    state.add_bytes(bytes.len() as u64);
                    if state.exceeded_bounds {
                        break;
                    }
                    corrupt_raws.remove(raw_hash);
                    staged_raws.push((raw_hash.clone(), bytes));
                    recovered = true;
                    break;
                }
                RawRecovery::LimitExceeded => {
                    state.exceeded_bounds = true;
                    break;
                }
            }
        }
        if !recovered && !state.exceeded_bounds {
            if let Some(reason) = corrupt_raws.remove(raw_hash) {
                state.finding("raw_corrupt", raw_hash, &reason, &affected);
            } else {
                state.finding(
                    "missing_raw",
                    raw_hash,
                    "reachable tree references an unmarked missing raw object",
                    &affected,
                );
            }
        }
    }
    if state.exceeded_bounds {
        return Ok(finish_limit_report(state));
    }
    for (raw_hash, reason) in corrupt_raws {
        state.finding("raw_corrupt", &raw_hash, &reason, &[]);
    }

    for (chunk_hash, chunk) in &chunks {
        if dead_raws.contains(&chunk.raw_hash) {
            continue;
        }
        let pinned_unit = match exact_pinned_content_unit_for_chunk(chunk, &reachable_units) {
            Ok(unit) => unit,
            Err(ChunkPinnedUnitError::Unreachable) => {
                state.finding(
                    "chunk_unit_content_unreachable",
                    chunk_hash,
                    "chunk unit_content_hash is absent from the reachable pinned normalized closure",
                    &[],
                );
                continue;
            }
            Err(ChunkPinnedUnitError::IdentityMismatch) => {
                state.finding(
                    "chunk_unit_identity_mismatch",
                    chunk_hash,
                    "chunk identity does not match its pinned normalized unit object",
                    &[],
                );
                continue;
            }
        };
        let unit = &pinned_unit.unit;
        // byte_start/byte_end are unit-local UTF-8 byte offsets (03 §8.1), always
        // present and ordered by construction — `ChunkObject::validate()` already
        // rejected any object with byte_start > byte_end before it reached this
        // map (cas.rs `read_chunk_path_accounted`). `str::get` on a byte range
        // additionally guards against an out-of-bounds span or one that doesn't
        // land on a UTF-8 char boundary — either is exact-span corruption
        // surfaced as a finding here, not a panic.
        let start = chunk.byte_start as usize;
        let end = chunk.byte_end as usize;
        match unit.markdown.get(start..end) {
            Some(exact)
                if exact == chunk.text && hash_bytes(exact.as_bytes()) == chunk.text_hash => {}
            _ => {
                state.finding(
                    "chunk_span_mismatch",
                    chunk_hash,
                    "chunk text does not match normalized span",
                    &[],
                );
            }
        }
    }

    for (raw_hash, bytes) in &staged_raws {
        store.repair_raw(raw_hash, bytes)?;
    }
    // LC27/LC35 backfill: append `retired` to whichever marker was canonical,
    // linking the republication commit found above as `resurrection_commit`.
    // R23-08: `at` is that commit's OWN `created_at` (05-runtime.md §3.5's
    // "その event の commit created_at と一致" requirement), never this fsck
    // invocation's own clock -- `find_republication_commit` now returns it
    // paired with the winning hash so this loop never has to re-derive it.
    // Recover a missing or rolled-back lifecycle counter before the first
    // append. Otherwise `increment_lifecycle_epoch` can reissue the existing
    // tail epoch and the stricter marker validator correctly rejects the
    // resulting non-monotonic sequence. The post-append call below remains
    // necessary to rotate the index generation for the newly written event.
    let index_exists = sqlite_path(repo.kio_dir()).exists();
    if !retirements_to_backfill.is_empty() {
        let max_event_lifecycle_epoch = purge.max_recorded_lifecycle_epoch()?;
        purge.recover_lifecycle_epoch(0, max_event_lifecycle_epoch)?;
        if index_exists {
            recover_index_generation(repo.kio_dir())?;
        }
    }
    let repair_actor = std::env::var("USER").unwrap_or_else(|_| "local-user".to_owned());
    let mut any_retired = false;
    for (raw_hash, marker_kind, republication_commit, republication_created_at) in
        retirements_to_backfill
    {
        let outcome = match marker_kind {
            TombstoneMode::Default => purge
                .retire_tombstone(
                    &raw_hash,
                    &republication_commit,
                    &republication_created_at,
                    &repair_actor,
                )
                .map(|_| ()),
            TombstoneMode::Erase => purge
                .retire_erase_receipt(
                    &raw_hash,
                    &republication_commit,
                    &republication_created_at,
                    &repair_actor,
                )
                .map(|_| ()),
        };
        match outcome {
            Ok(()) => any_retired = true,
            Err(error) => {
                let kind = match marker_kind {
                    TombstoneMode::Default => "tombstone_corrupt",
                    TombstoneMode::Erase => "erase_receipt_corrupt",
                };
                state.finding(kind, &raw_hash, &error.to_string(), &[]);
            }
        }
    }
    // R23-09/R23-22 (05-runtime.md §1.5 L215-219 "tombstone lifecycle の
    // 更新 (retire・再 purge)" is one of the six named `index_generation`
    // rotation triggers, and §3.5 L792-799's counter-rollback recovery must
    // run at every locked-mutation entry point that can advance the
    // lifecycle-epoch counter): a backfilled `retired` above bumps that
    // counter exactly like the index/reindex/purge paths' own retire events
    // do. Synchronize the post-append value so `index_metadata` does not stay
    // stale until an unrelated index-touching write. Only when `sqlite.db`
    // already exists: fsck must not conjure a fresh, empty index for a scope that
    // was never indexed (`recover_index_generation` has no existence guard
    // of its own -- its other callers run only after `rebuild_step3_index`
    // already guarantees the file is there).
    if any_retired && index_exists {
        recover_index_generation(repo.kio_dir())?;
    }
    let repaired = staged_raws.len() as u64;
    let repaired_commit_hash = if repaired > 0 && repairs_allowed && !state.exceeded_bounds {
        Some(repo.record_repaired_commit(Some("repair verify-objects recovered raw CAS"))?)
    } else {
        None
    };
    if state.exceeded_bounds {
        state.finding("inventory_limit", "", "fsck inventory bound exceeded", &[]);
    }
    Ok(finish_report(state, repaired, repaired_commit_hash))
}

/// The only error class that a canonical `retired` lifecycle can explain is a
/// physically absent object in the historical closure. A malformed manifest,
/// a hash mismatch, or an invalid unit tuple is never a purge explanation.
#[derive(Debug)]
enum PinnedNormalizedError {
    Missing,
    Corrupt(String),
}

impl std::fmt::Display for PinnedNormalizedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => formatter.write_str("pinned normalized CAS object is missing"),
            Self::Corrupt(reason) => formatter.write_str(reason),
        }
    }
}

const MAX_MANIFEST_OBJECT_BYTES: u64 = 8 * 1024 * 1024;

/// Extend the exact-unit index with immutable closures retained after a final
/// shallow sweep.  This intentionally does not consult `chunks.jsonl`: its
/// introduction fields are not authenticated bindings between a chunk and a
/// discarded tree.  Each accepted unit instead comes from a content-addressed
/// manifest and content-addressed body whose full schema/identity closure has
/// been validated by the same loader used for live tree references.
fn index_retained_manifest_units(
    store: &ObjectStore,
    kio_dir: &Path,
    explained: &BTreeMap<String, (String, NormalizeRef)>,
    units_by_content_hash: &mut BTreeMap<String, Vec<ValidatedNormalizedInstanceUnit>>,
    state: &mut State,
) -> Result<()> {
    let manifest_hashes = inventory(kio_dir, ContentObjectKind::Manifest.directory(), state)?;
    if state.exceeded_bounds || state.unsafe_namespace {
        return Ok(());
    }
    for manifest_hash in manifest_hashes {
        state.count_object();
        if state.exceeded_bounds {
            return Ok(());
        }
        let bytes = match store.read_content_object_bytes(
            ContentObjectKind::Manifest,
            &manifest_hash,
            MAX_MANIFEST_OBJECT_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) => {
                state.finding(
                    "normalized_closure_corrupt",
                    &manifest_hash,
                    &error.to_string(),
                    &[],
                );
                continue;
            }
        };
        state.add_bytes(bytes.len() as u64);
        if state.exceeded_bounds {
            return Ok(());
        }
        let manifest: NormalizedInstanceManifest = match serde_json::from_slice(&bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                state.finding(
                    "normalized_closure_corrupt",
                    &manifest_hash,
                    &format!("manifest schema: {error}"),
                    &[],
                );
                continue;
            }
        };
        let canonical = match serde_json::to_value(&manifest)
            .map_err(|error| KioError::schema(error.to_string()))
            .and_then(|value| canonical_json_bytes(&value))
        {
            Ok(canonical) => canonical,
            Err(error) => {
                state.finding(
                    "normalized_closure_corrupt",
                    &manifest_hash,
                    &error.to_string(),
                    &[],
                );
                continue;
            }
        };
        if canonical != bytes {
            state.finding(
                "normalized_closure_corrupt",
                &manifest_hash,
                "manifest is not canonical JSON or has unknown fields",
                &[],
            );
            continue;
        }
        let reference = NormalizeRef {
            tool_profile_hash: manifest.tool_profile_hash.clone(),
            r#gen: manifest.r#gen,
            manifest_hash,
        };
        let instance = match load_pinned_normalized_instance(
            store,
            kio_dir,
            &manifest.raw_hash,
            &reference,
            state,
        ) {
            Ok(instance) => instance,
            Err(PinnedNormalizedError::Missing)
                if explained
                    .get(&reference.manifest_hash)
                    .is_some_and(|(raw, exact)| {
                        raw == &manifest.raw_hash && exact == &reference
                    }) =>
            {
                match validate_purge_explained_missing_closure(
                    store,
                    kio_dir,
                    &manifest.raw_hash,
                    &reference,
                    state,
                ) {
                    Ok(()) => continue,
                    Err(error) => {
                        state.finding(
                            "normalized_closure_corrupt",
                            &reference.manifest_hash,
                            &error.to_string(),
                            &[],
                        );
                        continue;
                    }
                }
            }
            Err(error) => {
                state.finding(
                    "normalized_closure_corrupt",
                    &reference.manifest_hash,
                    &error.to_string(),
                    &[],
                );
                continue;
            }
        };
        for unit in instance.units {
            let content_hash = hash_bytes(unit.markdown.as_bytes());
            let candidates = units_by_content_hash.entry(content_hash).or_default();
            if !candidates.iter().any(|candidate| {
                candidate.raw_hash == instance.manifest.raw_hash
                    && candidate.tool_profile_hash == instance.manifest.tool_profile_hash
                    && candidate.r#gen == instance.manifest.r#gen
                    && candidate.unit.unit_key == unit.unit_key
            }) {
                candidates.push(ValidatedNormalizedInstanceUnit {
                    raw_hash: instance.manifest.raw_hash.clone(),
                    tool_profile_hash: instance.manifest.tool_profile_hash.clone(),
                    r#gen: instance.manifest.r#gen,
                    unit,
                });
            }
        }
    }
    Ok(())
}

/// Verify the immutable closure named by one tree `NormalizeRef`. This never
/// opens `objects/normalized_units/`: that path-named representation is a
/// mutable current cache and cannot authenticate a historical snapshot.
fn read_pinned_normalized_manifest(
    store: &ObjectStore,
    kio_dir: &Path,
    raw_hash: &str,
    normalize: &NormalizeRef,
    state: &mut State,
) -> std::result::Result<
    (NormalizedInstanceManifest, NormalizedInstanceIdentity, u64),
    PinnedNormalizedError,
> {
    state.count_object();
    if state.exceeded_bounds {
        return Err(PinnedNormalizedError::Corrupt(
            "verification object bound exceeded".to_owned(),
        ));
    }
    // This path is diagnostic only. The retained store is the read authority.
    let manifest_path = kio_dir
        .join("objects/manifests")
        .join(&normalize.manifest_hash);
    let manifest_bytes = store
        .read_content_object_bytes(
            ContentObjectKind::Manifest,
            &normalize.manifest_hash,
            MAX_MANIFEST_OBJECT_BYTES,
        )
        .map_err(|error| {
            if is_store_not_found(&error) {
                PinnedNormalizedError::Missing
            } else {
                PinnedNormalizedError::Corrupt(error.to_string())
            }
        })?;
    state.add_bytes(manifest_bytes.len() as u64);
    if state.exceeded_bounds {
        return Err(PinnedNormalizedError::Corrupt(
            "verification byte bound exceeded".to_owned(),
        ));
    }
    if hash_bytes(&manifest_bytes) != normalize.manifest_hash {
        return Err(PinnedNormalizedError::Corrupt(
            "pinned manifest bytes do not match their CAS hash".to_owned(),
        ));
    }
    let manifest: NormalizedInstanceManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| PinnedNormalizedError::Corrupt(format!("manifest schema: {error}")))?;
    // CAS manifests are canonical JCS. Comparing the exact bytes both proves
    // canonical serialization and makes serde's otherwise permissive unknown
    // field behavior fail closed for this durable schema.
    let canonical_manifest = canonical_json_bytes(
        &serde_json::to_value(&manifest)
            .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?,
    )
    .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
    if canonical_manifest != manifest_bytes {
        return Err(PinnedNormalizedError::Corrupt(
            "manifest is not canonical JSON or has unknown fields".to_owned(),
        ));
    }
    let identity = NormalizedInstanceIdentity {
        raw_hash: raw_hash.to_owned(),
        tool_profile_hash: normalize.tool_profile_hash.clone(),
        r#gen: normalize.r#gen,
    };
    if manifest.raw_hash != identity.raw_hash
        || manifest.tool_profile_hash != identity.tool_profile_hash
        || manifest.r#gen != identity.r#gen
    {
        return Err(PinnedNormalizedError::Corrupt(
            "manifest identity does not match its tree normalization reference".to_owned(),
        ));
    }
    kio_pipeline::markdownize::validate_normalized_manifest(&manifest_path, &identity, &manifest)
        .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
    Ok((manifest, identity, manifest_bytes.len() as u64))
}

fn load_pinned_normalized_instance(
    store: &ObjectStore,
    kio_dir: &Path,
    raw_hash: &str,
    normalize: &NormalizeRef,
    state: &mut State,
) -> std::result::Result<ValidatedNormalizedInstance, PinnedNormalizedError> {
    let (manifest, identity, manifest_bytes) =
        read_pinned_normalized_manifest(store, kio_dir, raw_hash, normalize, state)?;
    let manifest_path = kio_dir
        .join("objects/manifests")
        .join(&normalize.manifest_hash);
    // Identify an actual absence before entering the shared loader so only a
    // missing historical closure, not any other loader error, can receive the
    // narrow lifecycle explanation below.
    for entry in manifest
        .units
        .iter()
        .filter(|entry| entry.status == UnitStatus::Done)
    {
        let hash = entry.unit_object_hash.as_deref().ok_or_else(|| {
            PinnedNormalizedError::Corrupt(
                "done manifest entry has no normalized unit object hash".to_owned(),
            )
        })?;
        state.count_object();
        if state.exceeded_bounds {
            return Err(PinnedNormalizedError::Corrupt(
                "verification object bound exceeded".to_owned(),
            ));
        }
        match store.inspect_content_accounted(ContentObjectKind::NormalizedUnit, hash) {
            Ok(metadata) => state.add_bytes(metadata.size_bytes),
            Err(error) => {
                state.add_bytes(error.consumed_bytes);
                return Err(if is_store_not_found(&error.error) {
                    PinnedNormalizedError::Missing
                } else {
                    PinnedNormalizedError::Corrupt(error.error.to_string())
                });
            }
        }
        if state.exceeded_bounds {
            return Err(PinnedNormalizedError::Corrupt(
                "verification byte bound exceeded".to_owned(),
            ));
        }
    }
    let units = load_validated_normalized_units_from_manifest(store, &manifest)
        .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
    // The loader has already compared each stored body with canonical JSON.
    // Count those exact canonical bytes for this tree-reachable closure.
    let mut verified_unit_bytes = 0_u64;
    for unit in &units {
        let canonical = canonical_json_bytes(
            &serde_json::to_value(unit)
                .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?,
        )
        .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
        let bytes = canonical.len() as u64;
        verified_unit_bytes = verified_unit_bytes.saturating_add(bytes);
        // The shared loader performs digest inspection and a bounded body read.
        state.add_bytes(bytes.saturating_mul(2));
        if state.exceeded_bounds {
            return Err(PinnedNormalizedError::Corrupt(
                "verification byte bound exceeded".to_owned(),
            ));
        }
    }
    validate_normalized_instance(&manifest_path, &identity, &manifest, &units)
        .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
    Ok(ValidatedNormalizedInstance {
        manifest,
        units,
        verified_bytes: manifest_bytes + verified_unit_bytes,
    })
}

/// Validate all surviving bodies even when an earlier body was purged.
/// Whole-manifest validation precedes the absence exception.
fn load_surviving_normalized_units(
    store: &ObjectStore,
    manifest: &NormalizedInstanceManifest,
    state: &mut State,
) -> std::result::Result<
    (Vec<kio_pipeline::markdownize::NormalizedUnitObject>, bool),
    PinnedNormalizedError,
> {
    let identity = NormalizedInstanceIdentity {
        raw_hash: manifest.raw_hash.clone(),
        tool_profile_hash: manifest.tool_profile_hash.clone(),
        r#gen: manifest.r#gen,
    };
    kio_pipeline::markdownize::validate_normalized_manifest(
        "historical manifest",
        &identity,
        manifest,
    )
    .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
    let mut single = manifest.clone();
    single.units.clear();
    let mut units = Vec::new();
    let mut missing = false;
    for entry in manifest
        .units
        .iter()
        .filter(|entry| entry.status == UnitStatus::Done)
    {
        state.count_object();
        if state.exceeded_bounds {
            return Err(PinnedNormalizedError::Corrupt(
                "historical closure verification bound exceeded".into(),
            ));
        }
        let hash = entry
            .unit_object_hash
            .as_deref()
            .expect("validated manifest pin");
        match store.inspect_content_accounted(ContentObjectKind::NormalizedUnit, hash) {
            Ok(metadata) => state.add_bytes(metadata.size_bytes),
            Err(error) => {
                state.add_bytes(error.consumed_bytes);
                if is_store_not_found(&error.error) {
                    missing = true;
                    continue;
                }
                return Err(PinnedNormalizedError::Corrupt(error.error.to_string()));
            }
        }
        if state.exceeded_bounds {
            return Err(PinnedNormalizedError::Corrupt(
                "historical closure verification bound exceeded".into(),
            ));
        }
        single.units.clear();
        single.units.push(entry.clone());
        let loaded = load_validated_normalized_units_from_manifest(store, &single)
            .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
        for unit in &loaded {
            let bytes = canonical_json_bytes(
                &serde_json::to_value(unit)
                    .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?,
            )
            .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
            state.add_bytes((bytes.len() as u64).saturating_mul(2));
        }
        if state.exceeded_bounds {
            return Err(PinnedNormalizedError::Corrupt(
                "historical closure verification bound exceeded".into(),
            ));
        }
        units.extend(loaded);
    }
    Ok((units, missing))
}

fn validate_purge_explained_missing_closure(
    store: &ObjectStore,
    kio_dir: &Path,
    raw_hash: &str,
    normalize: &NormalizeRef,
    state: &mut State,
) -> std::result::Result<(), PinnedNormalizedError> {
    let (manifest, _, _) =
        match read_pinned_normalized_manifest(store, kio_dir, raw_hash, normalize, state) {
            Ok(value) => value,
            Err(PinnedNormalizedError::Missing) => return Ok(()),
            Err(error) => return Err(error),
        };
    let (_, missing) = load_surviving_normalized_units(store, &manifest, state)?;
    if !missing {
        return Err(PinnedNormalizedError::Corrupt(
            "historical closure changed during verification".into(),
        ));
    }
    Ok(())
}

/// Only exact closures whose every visible appearance predates the explaining
/// purge can borrow its absence authority. Generation numbers may be reused.
fn explained_historical_manifests(
    purge: &PurgeState,
    commits: &BTreeMap<String, CommitObject>,
    trees: &BTreeMap<String, TreeObject>,
    reachable: &BTreeSet<String>,
    head: Option<&str>,
    now: &str,
) -> BTreeMap<String, (String, NormalizeRef)> {
    let mut eligible = BTreeMap::new();
    let mut rejected = BTreeSet::new();
    for hash in reachable {
        let Some(tree) = commits.get(hash).and_then(|commit| trees.get(&commit.tree)) else {
            continue;
        };
        for entry in &tree.entries {
            let Some(reference) = &entry.normalize else {
                continue;
            };
            let key = &reference.manifest_hash;
            let same_identity =
                eligible
                    .get(key)
                    .is_none_or(|(raw, previous): &(String, NormalizeRef)| {
                        raw == &entry.raw_hash && previous == reference
                    });
            if head == Some(hash.as_str())
                || !same_identity
                || !purge_explains_old_closure_gap(
                    purge,
                    &entry.raw_hash,
                    hash,
                    commits,
                    trees,
                    reachable,
                    now,
                )
            {
                rejected.insert(key.clone());
                eligible.remove(key);
            } else if !rejected.contains(key) {
                eligible.insert(key.clone(), (entry.raw_hash.clone(), reference.clone()));
            }
        }
    }
    eligible
}

/// Read-only missing-closure classification shared by replica materialization.
/// A successful exception grants no image/unit ownership from the omitted closure.
pub(super) fn purge_explains_missing_retained_instance(
    repo: &Repository,
    raw_hash: &str,
    normalize: &NormalizeRef,
) -> Result<bool> {
    let session = GcSweepSession::bind_repository(repo)?;
    let directory = kio_core::store_dir::StoreDirectory::from_retained(
        session.retained_kio_handle()?,
        repo.kio_dir().to_path_buf(),
    )?;
    let purge = PurgeState::from_directory(directory);
    if purge.barrier_blocks(raw_hash)? {
        return Ok(false);
    }
    let shallow = session.validated_final_shallow_receipts()?;
    let store = repo.object_store();
    let mut state = State::default();
    let mut commits = BTreeMap::new();
    let mut trees = BTreeMap::new();
    let mut reachable = BTreeSet::new();
    let mut queue: VecDeque<_> = repo.current_ref_targets()?.into_iter().collect();
    while let Some(hash) = queue.pop_front() {
        if !reachable.insert(hash.clone()) {
            continue;
        }
        state.count_object();
        let commit = repo.read_commit(&hash)?;
        state.add_bytes(
            canonical_json_bytes(
                &serde_json::to_value(&commit)
                    .map_err(|error| KioError::schema(error.to_string()))?,
            )?
            .len() as u64,
        );
        queue.extend(commit.parent.iter().cloned());
        match repo.read_tree(&commit.tree) {
            Ok(tree) => {
                if shallow.contains_key(&hash) {
                    return Err(KioError::schema("shallow receipt coexists with tree"));
                }
                state.add_bytes(
                    canonical_json_bytes(
                        &serde_json::to_value(&tree)
                            .map_err(|error| KioError::schema(error.to_string()))?,
                    )?
                    .len() as u64,
                );
                trees.insert(commit.tree.clone(), tree);
            }
            Err(error)
                if is_store_not_found(&error) && shallow.get(&hash) == Some(&commit.tree) => {}
            Err(error) => return Err(error),
        }
        commits.insert(hash, commit);
        if state.exceeded_bounds {
            return Err(KioError::schema(
                "historical authority verification bound exceeded",
            ));
        }
    }
    let authority = explained_historical_manifests(
        &purge,
        &commits,
        &trees,
        &reachable,
        repo.head_commit_hash()?.as_deref(),
        &now_utc_seconds(),
    );
    if !authority
        .get(&normalize.manifest_hash)
        .is_some_and(|(raw, reference)| raw == raw_hash && reference == normalize)
    {
        return Ok(false);
    }
    let missing = match read_pinned_normalized_manifest(
        &store,
        repo.kio_dir(),
        raw_hash,
        normalize,
        &mut state,
    ) {
        Err(PinnedNormalizedError::Missing) => true,
        Err(error) => return Err(KioError::schema(error.to_string())),
        Ok((manifest, _, _)) => {
            load_surviving_normalized_units(&store, &manifest, &mut state)
                .map_err(|error| KioError::schema(error.to_string()))?
                .1
        }
    };
    session.assert_public_identity()?;
    Ok(missing)
}

/// A retired marker proves that the closure which existed before its verified
/// resurrection may have been physically removed by purge. It does *not*
/// excuse the resurrection snapshot itself, nor any unrelated later branch.
fn purge_explains_old_closure_gap(
    purge: &PurgeState,
    raw_hash: &str,
    commit_hash: &str,
    commits: &BTreeMap<String, CommitObject>,
    trees: &BTreeMap<String, TreeObject>,
    reachable: &BTreeSet<String>,
    invocation_time: &str,
) -> bool {
    let lookup = canonical_lookup(purge, raw_hash, commits, trees, reachable, invocation_time);
    if lookup.tombstone_error.is_some() || lookup.receipt_error.is_some() {
        return false;
    }
    let Some(canonical) = lookup.canonical else {
        return false;
    };
    if canonical.event.kind != EventKind::Retired {
        return false;
    }
    let Some(resurrection) = canonical.event.resurrection_commit.as_deref() else {
        return false;
    };
    if commit_hash == resurrection || !is_ancestor(commits, commit_hash, resurrection) {
        return false;
    }
    lookup
        .explaining_purge_commit
        .is_some_and(|cutoff| commit_hash == cutoff || is_ancestor(commits, commit_hash, &cutoff))
}

/// PB01 (§A, 10 §7.5.1 L489 → 03 §8.1): `objects/embeddings/` CAS objects.
///
/// The per-type algorithm is 03 §8.1's, not the generic byte-hash every other
/// content-addressed kind uses, because an embedding's storage key is its
/// IDENTITY hash — what the vector is OF (target, profile, context) — rather
/// than a hash of the bytes. Recomputing the identity from the parsed header
/// and comparing it to the leaf name is the equivalent check, and it catches
/// something the byte hash cannot: an object filed under the wrong identity.
///
/// [`EmbeddingObject::from_bytes`] performs the rest of what 03 §8.1 asks for —
/// vector length against declared `dimensions`, NaN/infinity rejection, and the
/// trailing vector digest (which is what detects a bit flip INSIDE the vector,
/// since the storage key says nothing about the body).
fn verify_embeddings(store: &ObjectStore, kio_dir: &Path, state: &mut State) -> Result<()> {
    let hashes = inventory(kio_dir, ContentObjectKind::Embedding.directory(), state)?;
    if state.exceeded_bounds || state.unsafe_namespace {
        return Ok(());
    }
    for hash in hashes {
        state.count_object();
        if state.exceeded_bounds {
            return Ok(());
        }
        let bytes =
            match store.read_content_object_bytes(ContentObjectKind::Embedding, &hash, 1 << 20) {
                Ok(bytes) => bytes,
                Err(error) => {
                    state.finding("embedding_corrupt", &hash, &error.to_string(), &[]);
                    continue;
                }
            };
        state.add_bytes(bytes.len() as u64);
        if state.exceeded_bounds {
            return Ok(());
        }
        match EmbeddingObject::from_bytes(&bytes) {
            Ok(embedding) => match embedding.identity_hash() {
                Ok(identity) if identity == hash => state.checked.embeddings += 1,
                Ok(identity) => state.finding(
                    "embedding_corrupt",
                    &hash,
                    &format!("embedding identity hashes to {identity}, not its storage key"),
                    &[],
                ),
                Err(error) => {
                    state.finding("embedding_corrupt", &hash, &error.to_string(), &[]);
                }
            },
            Err(error) => {
                state.finding("embedding_corrupt", &hash, &error.to_string(), &[]);
            }
        }
    }
    Ok(())
}

/// PB02 (§B, 10 §7.5.1 L489): the standalone (non-tree-cross-referenced —
/// see the module-level PB64-68 doc comment's gap note on
/// `normalize.manifest_hash`) part of manifest/toollock verification: every
/// object under `objects/<kind>/` is content-addressed and its stored bytes
/// must hash to its own leaf name, exactly like prepared/image already are.
fn verify_content_hash_closure(
    store: &ObjectStore,
    kio_dir: &Path,
    kind: ContentObjectKind,
    finding_kind: &str,
    state: &mut State,
) -> Result<()> {
    let hashes = inventory(kio_dir, kind.directory(), state)?;
    if state.exceeded_bounds || state.unsafe_namespace {
        return Ok(());
    }
    for hash in hashes {
        state.count_object();
        if state.exceeded_bounds {
            return Ok(());
        }
        match store.inspect_content_accounted(kind, &hash) {
            Ok(metadata) => {
                state.add_bytes(metadata.size_bytes);
                if state.exceeded_bounds {
                    return Ok(());
                }
                match kind {
                    ContentObjectKind::Manifest => state.checked.manifests += 1,
                    ContentObjectKind::Toollock => state.checked.toollocks += 1,
                    ContentObjectKind::NormalizedUnit => state.checked.normalized_units += 1,
                    _ => {}
                }
            }
            Err(failure) => {
                state.add_bytes(failure.consumed_bytes);
                if state.exceeded_bounds {
                    return Ok(());
                }
                state.finding(finding_kind, &hash, &failure.error.to_string(), &[]);
            }
        }
    }
    Ok(())
}

/// One parsed `names.jsonl` line (PB07-09, 03-data-model.md §2 L140-152).
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NamesJsonlRecord {
    digest64: String,
    logical_name: String,
    recorded_at: String,
}

/// §C (U41, PB07-09): `refs/tags-v1/names.jsonl` full-line verification and
/// its correspondence with the canonical tag refs `commit_roots` already
/// inventoried into `roots` — but this function re-derives the canonical ref
/// leaf set independently (a bounded directory listing, not a mutation of
/// `commit_roots`'s return shape) so it stays a self-contained, orthogonal
/// check.
///
/// - PB07: every line's schema is valid and `digest64` equals
///   `portable_tag_digest64(logical_name)` (a mismatch — schema-valid but
///   wrong digest — is corruption distinct from a schema-invalid line, but
///   both are reported under the same `names_jsonl_corrupt` finding kind,
///   matching PB07's framing of both as corruption without requiring a
///   third finding kind).
/// - PB08: only the FINAL line may be a torn (truncated) tail; a malformed
///   line anywhere else is corruption.
/// - PB09: a canonical ref with no corresponding names.jsonl line is
///   corruption; a names.jsonl line with no corresponding canonical ref is
///   normal (tag-delete residue, append-only by design).
fn verify_names_jsonl(kio_dir: &Path, state: &mut State) -> Result<()> {
    // Bounded like every other fsck read in this module (never an unbounded
    // read of a file an attacker/bug could grow without limit).
    const MAX_NAMES_JSONL_BYTES: u64 = 64 * 1024 * 1024;
    let path = names_jsonl_path(kio_dir);
    // A scope with no tags at all has no names.jsonl yet — normal, not a
    // finding (checked before the open so ENOENT is never ambiguous with a
    // genuine I/O failure on an existing path).
    if fs::symlink_metadata(&path).is_err() {
        return Ok(());
    }
    let text = match read_bounded_regular_file(&path, MAX_NAMES_JSONL_BYTES) {
        Ok(bytes) => bytes,
        Err(error) => {
            state.finding("names_jsonl_corrupt", "", &error.to_string(), &[]);
            return Ok(());
        }
    };
    state.add_bytes(text.len() as u64);
    if state.exceeded_bounds {
        return Ok(());
    }
    let raw_lines = text
        .split(|byte| *byte == b'\n')
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect::<Vec<_>>();
    // PB08: a JSONL file ending in `\n` has one trailing empty split segment
    // (not a torn tail); a file NOT ending in `\n` has a genuinely torn final
    // line, tolerated (silently truncated) here exactly like `chunks.jsonl`
    // (Q1) and `PurgeState`'s own record parsing.
    let mut lines = raw_lines;
    let torn_tail = !text.is_empty() && text.last() != Some(&b'\n');
    // Either the well-terminated case's trailing empty split segment, or the
    // torn-tail case's genuinely incomplete final line — both are dropped by
    // the same `pop()`, just for different reasons (see the comment above).
    if torn_tail || lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }

    let mut last_valid_by_digest = BTreeMap::<String, String>::new();
    for line in &lines {
        if line.trim().is_empty() {
            continue;
        }
        state.count_object();
        if state.exceeded_bounds {
            return Ok(());
        }
        match serde_json::from_str::<NamesJsonlRecord>(line) {
            Ok(record) => {
                if portable_tag_name_has_unassigned(&record.logical_name) {
                    state.finding(
                        "names_jsonl_corrupt",
                        &record.digest64,
                        "names.jsonl logical_name contains a character unassigned in Unicode 16",
                        &[],
                    );
                    continue;
                }
                let expected_digest = portable_tag_digest64(&record.logical_name);
                if record.digest64 != expected_digest {
                    state.finding(
                        "names_jsonl_corrupt",
                        &record.digest64,
                        "names.jsonl digest64 does not match the recomputed logical_name digest",
                        &[],
                    );
                } else if record.recorded_at.is_empty() {
                    state.finding(
                        "names_jsonl_corrupt",
                        &record.digest64,
                        "names.jsonl row is missing recorded_at",
                        &[],
                    );
                } else {
                    last_valid_by_digest.insert(record.digest64.clone(), record.logical_name);
                }
            }
            Err(error) => {
                state.finding("names_jsonl_corrupt", "", &error.to_string(), &[]);
            }
        }
    }

    // PB09: canonical ref <-> names.jsonl correspondence. A ref with no
    // matching (schema-valid) names row is corruption; the reverse (a names
    // row with no matching ref) is normal tag-delete residue.
    let canonical_dir = kio_dir.join("refs/tags-v1");
    let Ok(entries) = fs::read_dir(&canonical_dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let leaf = entry.file_name().to_string_lossy().into_owned();
        if leaf == "names.jsonl" {
            continue;
        }
        let Some(digest) = leaf.strip_prefix("tag-") else {
            continue;
        };
        if !last_valid_by_digest.contains_key(digest) {
            state.finding(
                "names_jsonl_corrupt",
                digest,
                "canonical tag ref has no corresponding names.jsonl row",
                &[],
            );
        }
    }
    Ok(())
}

fn finish_report(
    state: State,
    repaired_raw_count: u64,
    repaired_commit_hash: Option<String>,
) -> VerifyObjectsReport {
    let has_findings = !state.findings.is_empty() || state.findings_truncated;
    VerifyObjectsReport {
        status: if has_findings { "corrupt" } else { "ok" }.to_owned(),
        checked: state.checked,
        repaired_raw_count,
        repaired_commit_hash,
        dead_by_tombstone_count: state.dead_by_tombstone_count,
        dead_by_erase_receipt_count: state.dead_by_erase_receipt_count,
        purge_explained_missing_history_count: state.purge_explained_missing_history_count,
        remaining_findings: state.findings,
        findings_truncated: state.findings_truncated,
        external_pointers_may_be_affected: has_findings
            || state.purge_explained_missing_history_count > 0,
        verified_bytes: state.verified_bytes,
        inventoried_objects: state.inventoried_objects,
    }
}

fn finish_limit_report(mut state: State) -> VerifyObjectsReport {
    state.finding("inventory_limit", "", "fsck inventory bound exceeded", &[]);
    finish_report(state, 0, None)
}

struct State {
    checked: CheckedObjects,
    findings: Vec<ObjectFinding>,
    findings_truncated: bool,
    exceeded_bounds: bool,
    unsafe_namespace: bool,
    verified_bytes: u64,
    inventoried_objects: usize,
    visited_entries: usize,
    dead_by_tombstone_count: u64,
    dead_by_erase_receipt_count: u64,
    purge_explained_missing_history_count: u64,
    max_objects: usize,
    max_verified_bytes: u64,
    remaining_affected_commits: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            checked: CheckedObjects::default(),
            findings: Vec::new(),
            findings_truncated: false,
            exceeded_bounds: false,
            unsafe_namespace: false,
            verified_bytes: 0,
            inventoried_objects: 0,
            visited_entries: 0,
            dead_by_tombstone_count: 0,
            dead_by_erase_receipt_count: 0,
            purge_explained_missing_history_count: 0,
            max_objects: MAX_OBJECTS,
            max_verified_bytes: MAX_VERIFIED_BYTES,
            remaining_affected_commits: MAX_AFFECTED_COMMITS,
        }
    }
}

impl State {
    fn add_bytes(&mut self, bytes: u64) {
        self.verified_bytes = self.verified_bytes.saturating_add(bytes);
        self.exceeded_bounds |= self.verified_bytes > self.max_verified_bytes;
    }

    fn visit_entry(&mut self) {
        self.visited_entries = self.visited_entries.saturating_add(1);
        self.exceeded_bounds |= self.visited_entries > MAX_OBJECTS;
    }

    fn count_object(&mut self) {
        self.inventoried_objects = self.inventoried_objects.saturating_add(1);
        self.exceeded_bounds |= self.inventoried_objects > self.max_objects;
    }

    fn finding(&mut self, kind: &str, hash: &str, reason: &str, affected: &[String]) {
        if self.findings.len() >= MAX_FINDINGS {
            self.findings_truncated = true;
            return;
        }
        let affected_count = affected.len().min(self.remaining_affected_commits);
        self.remaining_affected_commits -= affected_count;
        self.findings.push(ObjectFinding {
            kind: kind.to_owned(),
            object_hash: hash.to_owned(),
            reason: reason.to_owned(),
            affected_commits: affected.iter().take(affected_count).cloned().collect(),
        });
    }
}

fn inventory(kio_dir: &Path, kind: &str, state: &mut State) -> Result<BTreeSet<String>> {
    let base = kio_dir.join("objects").join(kind);
    if !real_directory(&base)? {
        if fs::symlink_metadata(&base).is_ok() {
            state.unsafe_namespace = true;
            state.finding(
                "non_regular_object",
                "",
                "object namespace root is not a real directory",
                &[],
            );
        }
        return Ok(BTreeSet::new());
    }
    let mut hashes = BTreeSet::new();
    let mut stack = vec![base.clone()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| KioError::io(error.to_string(), directory.display().to_string()))?
        {
            let entry = entry.map_err(|error| {
                KioError::io(error.to_string(), directory.display().to_string())
            })?;
            state.visit_entry();
            if state.exceeded_bounds {
                return Ok(hashes);
            }
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
            if file_type.is_dir() && !file_type.is_symlink() {
                if real_directory(&path)? {
                    stack.push(path);
                } else {
                    state.unsafe_namespace = true;
                    state.finding(
                        "non_regular_object",
                        "",
                        "object inventory contains a linked directory",
                        &[],
                    );
                }
                continue;
            }
            if !file_type.is_file() || file_type.is_symlink() {
                state.count_object();
                state.finding(
                    "non_regular_object",
                    "",
                    "object inventory contains a non-regular entry",
                    &[],
                );
                continue;
            }
            let leaf = entry.file_name().to_string_lossy().into_owned();
            let digest = leaf.strip_prefix("sha256:").unwrap_or(&leaf);
            let hash = format!("sha256:{digest}");
            let relative = path.strip_prefix(&base).unwrap_or(&path);
            let parts = relative
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>();
            if !is_hash(&hash)
                || parts.len() != 3
                || parts[0] != digest[0..2]
                || parts[1] != digest[2..4]
            {
                state.count_object();
                state.finding(
                    "invalid_fanout",
                    &hash,
                    "object leaf does not match canonical fan-out",
                    &[],
                );
                continue;
            }
            if hashes.insert(hash) {
                state.count_object();
            }
            if state.exceeded_bounds {
                return Ok(hashes);
            }
        }
    }
    Ok(hashes)
}

fn verify_prepared_reference(
    store: &ObjectStore,
    prepared_hash: &str,
    verified_raw_substitution: bool,
) -> std::result::Result<u64, AccountedReadError> {
    match store.inspect_content_accounted(ContentObjectKind::Prepared, prepared_hash) {
        Ok(metadata) => Ok(metadata.size_bytes),
        Err(failure)
            if failure.error.error_code() == "KIO-E-STORE-NOT-FOUND-001"
                && verified_raw_substitution =>
        {
            Ok(0)
        }
        Err(failure) => Err(failure),
    }
}

fn marker_inventory(kio_dir: &Path, relative: &str, state: &mut State) -> Result<BTreeSet<String>> {
    let base = kio_dir.join(relative);
    if !real_directory(&base)? {
        if fs::symlink_metadata(&base).is_ok() {
            state.unsafe_namespace = true;
            state.finding(
                "purge_marker_corrupt",
                "",
                "purge marker namespace root is not a real directory",
                &[],
            );
        }
        return Ok(BTreeSet::new());
    }
    let mut hashes = BTreeSet::new();
    let mut stack = vec![base.clone()];
    while let Some(directory) = stack.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|error| KioError::io(error.to_string(), directory.display().to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                KioError::io(error.to_string(), directory.display().to_string())
            })?;
            state.visit_entry();
            if state.exceeded_bounds {
                return Ok(hashes);
            }
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?;
            if file_type.is_dir() && !file_type.is_symlink() {
                if real_directory(&path)? {
                    stack.push(path);
                } else {
                    state.unsafe_namespace = true;
                    state.finding(
                        "purge_marker_corrupt",
                        "",
                        "purge marker inventory contains a linked directory",
                        &[],
                    );
                }
                continue;
            }
            if !file_type.is_file() || file_type.is_symlink() {
                state.count_object();
                state.finding(
                    "purge_marker_corrupt",
                    "",
                    "purge marker inventory contains a non-regular entry",
                    &[],
                );
                continue;
            }
            let leaf = entry.file_name().to_string_lossy().into_owned();
            if relative == "tombstones" && directory == base && leaf == "lifecycle-epoch" {
                // `.kio/tombstones/lifecycle-epoch` is the monotonic lifecycle
                // counter (03 §4.1 / 05 §3.5), not a marker record — it lives at
                // the inventory root, outside the fan-out namespace.
                continue;
            }
            let digest = if relative == "tombstones" {
                leaf.strip_prefix("sha256:").unwrap_or(&leaf)
            } else {
                &leaf
            };
            let hash = format!("sha256:{digest}");
            let parts = path
                .strip_prefix(&base)
                .unwrap_or(&path)
                .iter()
                .map(|part| part.to_string_lossy())
                .collect::<Vec<_>>();
            if !is_hash(&hash)
                || parts.len() != 3
                || parts[0] != digest[0..2]
                || parts[1] != digest[2..4]
            {
                state.count_object();
                state.finding(
                    "purge_marker_corrupt",
                    &hash,
                    "purge marker leaf does not match canonical fan-out",
                    &[],
                );
                continue;
            }
            match read_bounded_regular_file(&path, MAX_PURGE_RECORD_BYTES) {
                Ok(bytes) => {
                    state.add_bytes(bytes.len() as u64);
                    if state.exceeded_bounds {
                        return Ok(hashes);
                    }
                }
                Err(error) => {
                    state.finding("purge_marker_corrupt", &hash, &error.to_string(), &[]);
                    continue;
                }
            }
            if hashes.insert(hash) {
                state.count_object();
            }
            if state.exceeded_bounds {
                return Ok(hashes);
            }
        }
    }
    Ok(hashes)
}

fn real_directory(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Ok(false);
    }
    #[cfg(windows)]
    if !kio_core::cas::windows_directory_is_real(path)
        .map_err(|error| KioError::io(error.to_string(), path.display().to_string()))?
    {
        return Ok(false);
    }
    Ok(true)
}

fn reachable_commits(
    repo: &Repository,
    commits: &BTreeMap<String, CommitObject>,
    state: &mut State,
) -> Result<BTreeSet<String>> {
    let mut visited = BTreeSet::new();
    let mut queue = authenticated_history_roots(repo, commits, state)?
        .into_iter()
        .collect::<VecDeque<_>>();
    while let Some(hash) = queue.pop_front() {
        if !visited.insert(hash.clone()) {
            continue;
        }
        let Some(commit) = commits.get(&hash) else {
            state.finding(
                "missing_commit",
                &hash,
                "reachable commit object is missing",
                &[],
            );
            continue;
        };
        queue.extend(commit.parent.iter().cloned());
        if visited.len() > state.max_objects {
            state.exceeded_bounds = true;
            break;
        }
    }
    Ok(visited)
}

/// Fsck inspects malformed refs as findings without granting them history
/// authority. The entire published closure starts at HEAD; tags and durable
/// publication records can only refer to ancestors already in that closure.
fn authenticated_history_roots(
    repo: &Repository,
    commits: &BTreeMap<String, CommitObject>,
    state: &mut State,
) -> Result<BTreeSet<String>> {
    let ref_roots = commit_roots(repo, state)?;
    let head = repo.head_commit_hash()?;
    let roots = head.iter().cloned().collect::<BTreeSet<_>>();
    if state.exceeded_bounds {
        return Ok(roots);
    }
    let mut ancestry = BTreeSet::new();
    let mut cursor = head.as_deref();
    while let Some(hash) = cursor {
        if !ancestry.insert(hash) {
            state.unsafe_namespace = true;
            state.finding("commit_cycle", hash, "HEAD ancestry contains a cycle", &[]);
            break;
        }
        if ancestry.len() > state.max_objects {
            state.exceeded_bounds = true;
            return Ok(roots);
        }
        // Missing/corrupt commit objects are independently reported by the
        // inventory and reachable walk. Never invent an edge from a cache.
        cursor = commits
            .get(hash)
            .and_then(|commit| commit.parent.as_deref());
    }
    for root in &ref_roots {
        if !ancestry.contains(root.as_str()) {
            state.unsafe_namespace = true;
            state.finding(
                "ref_outside_head",
                root,
                "ref target is outside the published HEAD ancestry",
                &[],
            );
        }
    }
    // Operational readers reject invalid publication roots. Fsck must instead
    // report that rejection and continue examining the real HEAD closure.
    // Neither an invalid tag nor a corrupt ledger is a fallback publication.
    if let Err(error) = durable_history_roots(repo) {
        state.unsafe_namespace = true;
        state.finding(
            "history_authority_corrupt",
            head.as_deref().unwrap_or(""),
            &error.to_string(),
            &[],
        );
    }
    Ok(roots)
}

fn commit_roots(repo: &Repository, state: &mut State) -> Result<BTreeSet<String>> {
    const MAX_REF_BYTES: u64 = 128;
    let mut roots = BTreeSet::new();
    let head_path = repo.kio_dir().join("HEAD");
    match read_bounded_regular_file(&head_path, MAX_REF_BYTES) {
        Ok(bytes) => {
            state.add_bytes(bytes.len() as u64);
            if state.exceeded_bounds {
                return Ok(roots);
            }
            match std::str::from_utf8(&bytes).map(str::trim) {
                Ok("unborn") => {}
                Ok(value) if is_hash(value) => {
                    roots.insert(value.to_owned());
                }
                _ => state.finding("ref_corrupt", "", "HEAD is not a commit hash", &[]),
            }
        }
        Err(error) => state.finding("ref_corrupt", "", &error.to_string(), &[]),
    }
    for relative in ["refs/heads", "refs/tags-v1"] {
        let base = repo.kio_dir().join(relative);
        let metadata = match fs::symlink_metadata(&base) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                state.finding("ref_io", "", &error.to_string(), &[]);
                continue;
            }
        };
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            state.finding(
                "ref_non_regular",
                "",
                "ref root is not a real directory",
                &[],
            );
            continue;
        }
        let mut stack = vec![base];
        while let Some(directory) = stack.pop() {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) => {
                    state.finding("ref_io", "", &error.to_string(), &[]);
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        state.finding("ref_io", "", &error.to_string(), &[]);
                        continue;
                    }
                };
                state.visit_entry();
                if state.exceeded_bounds {
                    return Ok(roots);
                }
                let path = entry.path();
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(error) => {
                        state.finding("ref_io", "", &error.to_string(), &[]);
                        continue;
                    }
                };
                if file_type.is_dir() && !file_type.is_symlink() {
                    state.finding(
                        "ref_non_regular",
                        "",
                        "nested ref directories are not allowed",
                        &[],
                    );
                    continue;
                }
                if !file_type.is_file() || file_type.is_symlink() {
                    state.finding("ref_non_regular", "", "ref is not a real regular file", &[]);
                    continue;
                }
                if relative == "refs/tags-v1" && entry.file_name() == "names.jsonl" {
                    // §C (PB07-09): names.jsonl co-resides in refs/tags-v1/
                    // (03 §2 L80) but is a ledger, not a tag ref — its own
                    // verification lives in `verify_names_jsonl`, not here.
                    continue;
                }
                if relative == "refs/tags-v1" {
                    let leaf = entry.file_name().to_string_lossy().into_owned();
                    let valid = leaf.strip_prefix("tag-").is_some_and(|digest| {
                        digest.len() == 64
                            && digest
                                .bytes()
                                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                    });
                    if !valid {
                        state.finding("ref_corrupt", "", "canonical tag leaf is invalid", &[]);
                        continue;
                    }
                }
                let bytes = match read_bounded_regular_file(&path, MAX_REF_BYTES) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        state.finding("ref_corrupt", "", &error.to_string(), &[]);
                        continue;
                    }
                };
                state.add_bytes(bytes.len() as u64);
                if state.exceeded_bounds {
                    return Ok(roots);
                }
                let value = match std::str::from_utf8(&bytes) {
                    Ok(value) => value.trim(),
                    Err(_) => {
                        state.finding("ref_corrupt", "", "ref is not UTF-8", &[]);
                        continue;
                    }
                };
                if value.is_empty() {
                    if relative != "refs/heads" || entry.file_name() != "main" {
                        state.finding("ref_corrupt", "", "ref value is empty", &[]);
                    }
                    continue;
                }
                if !is_hash(value) {
                    state.finding("ref_corrupt", value, "ref value is not a commit hash", &[]);
                    continue;
                }
                roots.insert(value.to_owned());
                if roots.len() > state.max_objects {
                    state.exceeded_bounds = true;
                    return Ok(roots);
                }
            }
        }
    }
    Ok(roots)
}

// ===========================================================================
// `kio repair verify-objects --prune-orphans` (step4b-contract-tests-p2b.md
// §E, U43, 10-operations.md §7.5.1 L586-626).
// ===========================================================================

mod prune_orphans;
#[cfg(test)]
use prune_orphans::PruneOrphansPlan;
pub use prune_orphans::{prune_orphans_apply, prune_orphans_plan, prune_orphans_preflight};

// ===========================================================================
// `kio repair registry-prune` (step4b-contract-tests-p2b.md §H, U46,
// 10-operations.md §3 L291-293).
// ===========================================================================

#[derive(Debug, Default, Serialize)]
pub struct RegistryPruneReport {
    pub pruned_count: u64,
}

/// PB25: delete registry rows whose `.kio` is unreachable (no re-init, no
/// re-discovery possible) — NOT rows that are merely live-duplicated (PB21's
/// concern; dedupe there is a user decision, never automatic here). A row is
/// unreachable when `open_scope_from_hint` cannot open it AT ALL (the `.kio`
/// itself does not validate), independent of whether its `scope_id` matches
/// what the registry row claims (a mismatched-but-openable `.kio` is a stale
/// registration for a DIFFERENT purpose — R15-3's `retire_stale_kio_path`
/// already owns that case on the next `init`/`index`, not this command).
/// `dry_run` counts the rows that WOULD be retired without touching the
/// registry — the preview half of 06 §1's required confirmation prompt.
/// H2-4/H2-6: the exact registry rows a prune will retire.
#[derive(Debug, Default)]
pub struct RegistryPrunePlan {
    /// `(scope_id, kio_path, root_path)` per row.
    pub rows: Vec<(String, String, String)>,
}

impl RegistryPrunePlan {
    #[must_use]
    pub fn target_lines(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|(scope_id, _, root_path)| format!("registry  {scope_id}  {root_path}"))
            .collect()
    }
}

/// Compute which registry rows are unreachable. Removes nothing.
pub fn registry_prune_plan() -> Result<RegistryPrunePlan> {
    let registry = RegistryDb::open_default().map_err(index_to_kio)?;
    let mut rows = Vec::new();
    for entry in registry.all_entries().map_err(index_to_kio)? {
        if crate::open_scope_from_hint_with_mode(
            &entry.root_path,
            crate::ScopeResolutionMode::Normal,
        )
        .is_none()
        {
            rows.push((entry.scope_id, entry.kio_path, entry.root_path));
        }
    }
    Ok(RegistryPrunePlan { rows })
}

/// Retire exactly the rows [`registry_prune_plan`] listed.
///
/// H2-6 (R24b terra-002, fatal): re-scanning here instead would let a row that
/// became unreachable AFTER the user saw the preview be deleted without ever
/// having been shown.
pub fn registry_prune_apply(plan: &RegistryPrunePlan) -> Result<RegistryPruneReport> {
    let registry = RegistryDb::open_default().map_err(index_to_kio)?;
    let mut pruned = 0u64;
    for (scope_id, kio_path, _) in &plan.rows {
        registry.remove(scope_id, kio_path).map_err(index_to_kio)?;
        pruned += 1;
    }
    Ok(RegistryPruneReport {
        pruned_count: pruned,
    })
}

/// LC8-LC10 (§C): canonical final event across both markers for `raw_hash`,
/// after LC9's "only validated markers participate" gate. A marker whose
/// `events[]` fails structural (`kio_core::purge`, already applied by
/// `PurgeState::read_tombstone`/`read_erase_receipt`) or semantic (this
/// module's DAG-bound `validate_marker_events`) validation does not
/// contribute its tail — its failure reason is returned separately so the
/// caller can still surface a `*_corrupt` finding for it (LC21: fsck and the
/// resolver never disagree about which markers explain state).
struct CanonicalLookup {
    explaining_purge_commit: Option<String>,
    canonical: Option<CanonicalFinalEvent>,
    tombstone_error: Option<String>,
    receipt_error: Option<String>,
}

fn canonical_lookup(
    purge: &PurgeState,
    raw_hash: &str,
    commits: &BTreeMap<String, CommitObject>,
    trees: &BTreeMap<String, TreeObject>,
    reachable: &BTreeSet<String>,
    invocation_time: &str,
) -> CanonicalLookup {
    let mut tombstone_tail = None;
    let mut tombstone_purge = None;
    let mut tombstone_error = None;
    match purge.read_tombstone(raw_hash) {
        Ok(Some(record)) => {
            match validate_marker_events(
                raw_hash,
                &record.events,
                commits,
                trees,
                reachable,
                invocation_time,
            ) {
                Ok(()) => {
                    tombstone_tail = Some(record.tail().clone());
                    tombstone_purge = record
                        .events
                        .iter()
                        .rev()
                        .find(|event| matches!(event.kind, EventKind::Purged | EventKind::Erased))
                        .map(|event| event.in_commit.clone());
                }
                Err(reason) => tombstone_error = Some(reason),
            }
        }
        Ok(None) => {}
        Err(error) => tombstone_error = Some(error.to_string()),
    }
    let mut receipt_tail = None;
    let mut receipt_purge = None;
    let mut receipt_error = None;
    match purge.read_erase_receipt(raw_hash) {
        Ok(Some(receipt)) => {
            match validate_marker_events(
                raw_hash,
                &receipt.events,
                commits,
                trees,
                reachable,
                invocation_time,
            ) {
                Ok(()) => {
                    receipt_tail = Some(receipt.tail().clone());
                    receipt_purge = receipt
                        .events
                        .iter()
                        .rev()
                        .find(|event| matches!(event.kind, EventKind::Purged | EventKind::Erased))
                        .map(|event| event.in_commit.clone());
                }
                Err(reason) => receipt_error = Some(reason),
            }
        }
        Ok(None) => {}
        Err(error) => receipt_error = Some(error.to_string()),
    }
    let canonical = match canonical_final_event(tombstone_tail.as_ref(), receipt_tail.as_ref()) {
        Ok(canonical) => canonical,
        Err(error) => {
            // Keep malformed tails out of ordering and report them through
            // the existing per-marker corruption findings.
            let reason = error.to_string();
            if tombstone_tail.is_some_and(|event| event.lifecycle_epoch.is_none()) {
                tombstone_error = Some(reason.clone());
            }
            if receipt_tail.is_some_and(|event| event.lifecycle_epoch.is_none()) {
                receipt_error = Some(reason);
            }
            None
        }
    };
    let explaining_purge_commit = canonical
        .as_ref()
        .and_then(|value| match value.marker_kind {
            TombstoneMode::Default => tombstone_purge,
            TombstoneMode::Erase => receipt_purge,
        });
    CanonicalLookup {
        explaining_purge_commit,
        canonical,
        tombstone_error,
        receipt_error,
    }
}

/// LC17/LC18/LC20/LC21 (10 §7.5.1): the DAG-bound semantic checks on top of
/// `kio_core::purge`'s structural validation (kind closure, required-field
/// matrix, transition grammar). Applied identically to tombstone and erase
/// receipt events (10 §7.5.1: "tombstone lifecycle にも同じ event 検証を適用する").
fn validate_marker_events(
    raw_hash: &str,
    events: &[LifecycleEvent],
    commits: &BTreeMap<String, CommitObject>,
    trees: &BTreeMap<String, TreeObject>,
    reachable: &BTreeSet<String>,
    invocation_time: &str,
) -> std::result::Result<(), String> {
    for (index, event) in events.iter().enumerate() {
        match event.kind {
            EventKind::Purged | EventKind::Erased => {
                validate_purge_or_erase_in_commit(
                    raw_hash,
                    event,
                    commits,
                    reachable,
                    invocation_time,
                )?;
            }
            EventKind::Retired => {
                let previous_index = index
                    .checked_sub(1)
                    .ok_or_else(|| "retired event has no preceding event".to_owned())?;
                let previous = events
                    .get(previous_index)
                    .ok_or_else(|| "retired event has no preceding event".to_owned())?;
                validate_retired_event(
                    raw_hash,
                    event,
                    &previous.in_commit,
                    commits,
                    trees,
                    reachable,
                )?;
            }
        }
    }
    Ok(())
}

/// LC17/LC18: a `purged`/`erased` event's `in_commit` must be a bounded
/// verified CAS object, ref-reachable, `commit_type=purged`, whose
/// `purged_raws` includes this marker's raw_hash (03 §8: forged-`in_commit`
/// defense — a purge commit for a *different* raw cannot explain this one's
/// absence), with `at` equal to that commit's `created_at` and not in the
/// future relative to the fixed invocation time.
fn validate_purge_or_erase_in_commit(
    raw_hash: &str,
    event: &LifecycleEvent,
    commits: &BTreeMap<String, CommitObject>,
    reachable: &BTreeSet<String>,
    invocation_time: &str,
) -> std::result::Result<(), String> {
    if !reachable.contains(&event.in_commit) {
        return Err("lifecycle event in_commit is not ref-reachable".to_owned());
    }
    let commit = commits
        .get(&event.in_commit)
        .ok_or_else(|| "lifecycle event in_commit object is missing or corrupt".to_owned())?;
    // R23-11: the type / purged_raws-membership / at-equality / not-future
    // checks are the shared validator (`kio_core::purge::verify_marker_binding`)
    // also used by the resolver's `read_tombstone` wrapper and
    // `PurgeState::begin`'s re-purge short-circuit — only the
    // ref-reachability check above (this module's own bounded all-parent
    // walk) stays local, matching that function's documented
    // "resolver/re-purge-weight vs fsck-weight" split.
    kio_core::purge::verify_marker_binding(raw_hash, event, commit, invocation_time)
        .map_err(|error| error.to_string())
}

/// LC20: terminal `retired`'s `resurrection_commit` must be ref-reachable and
/// a (strict) descendant of the immediately-preceding `purged`/`erased`
/// event's `in_commit` — i.e. the republication happened *after* that purge.
/// Additionally (defense-in-depth, 08-evidence-pointer-spec.md step 8's own
/// tree-membership re-check), when the resurrection commit's tree has NOT
/// been shallow-discarded, that tree must actually contain a leaf entry for
/// this same `raw_hash` — a `retired` event whose resurrection commit exists,
/// is ref-reachable, and descends correctly, but whose tree simply never
/// carried this raw_hash at all, is still corruption (a forged/mismatched
/// resurrection link). `trees` mirrors this module's own inventory-scanned
/// tree map (`fsck`'s `trees`, keyed by tree hash — physically-absent trees,
/// including a shallow-discarded one, are simply absent from it, so the leaf
/// check is skipped rather than misreported as corruption for a legitimately
/// shallow auto-type publication commit).
fn validate_retired_event(
    raw_hash: &str,
    event: &LifecycleEvent,
    previous_in_commit: &str,
    commits: &BTreeMap<String, CommitObject>,
    trees: &BTreeMap<String, TreeObject>,
    reachable: &BTreeSet<String>,
) -> std::result::Result<(), String> {
    let resurrection_commit = event
        .resurrection_commit
        .as_deref()
        .ok_or_else(|| "retired event is missing its resurrection_commit".to_owned())?;
    if !reachable.contains(resurrection_commit) {
        return Err("retired event resurrection_commit is not ref-reachable".to_owned());
    }
    let Some(commit) = commits.get(resurrection_commit) else {
        return Err("retired event resurrection_commit object is missing or corrupt".to_owned());
    };
    if resurrection_commit == previous_in_commit
        || !is_ancestor(commits, previous_in_commit, resurrection_commit)
    {
        return Err(
            "retired event resurrection_commit does not descend from the preceding purge/erase event"
                .to_owned(),
        );
    }
    if let Some(tree) = trees.get(&commit.tree)
        && !tree.entries.iter().any(|entry| entry.raw_hash == raw_hash)
    {
        return Err(
            "retired event resurrection_commit tree does not contain a leaf for this raw_hash"
                .to_owned(),
        );
    }
    // `trees.get(&commit.tree) == None`: the resurrection commit's tree is
    // not physically present (shallow-discarded, or GC'd) — LC20's explicit
    // "tree 不在時は本検証を省略する" carve-out. A tree that IS present but
    // fails to parse already produced its own `tree_corrupt` finding
    // elsewhere in the fsck scan that built `trees`, independent of this
    // function.
    Ok(())
}

/// Strict-descendant test over the fsck-scanned commit map's parent chains
/// (`ancestor != descendant`; every commit is reachable from itself only via
/// its own chain, never treated as its own ancestor here since callers always
/// want "happened strictly after").
fn is_ancestor(commits: &BTreeMap<String, CommitObject>, ancestor: &str, descendant: &str) -> bool {
    let mut pending = vec![descendant.to_owned()];
    let mut visited = BTreeSet::new();
    while let Some(hash) = pending.pop() {
        let Some(commit) = commits.get(&hash) else {
            continue;
        };
        if let Some(parent) = commit.parent.as_deref() {
            if parent == ancestor {
                return true;
            }
            if visited.insert(parent.to_owned()) {
                pending.push(parent.to_owned());
            }
        }
    }
    false
}

/// LC27/LC29/LC35/LC36 (R23-08): search the ref-reachable set for a commit
/// that causally republished `raw_hash` after `purge_in_commit` — a strict
/// descendant of it whose tree, when present, actually carries a live leaf
/// for `raw_hash` (10-operations.md §7.5.1's "resurrection_commit の
/// verified tree が同一 raw_hash の leaf を含むことを tree 存置時に限り
/// 検証する" — an ancestry-only match could pick an unrelated LATER commit
/// that never republished this raw at all, which `validate_retired_event`
/// would then flag as corruption on the very next fsck run: this function
/// used to be the one caller that skipped its own module's leaf check). A
/// tree that is not present at all (shallow-discarded — an `auto`-type
/// publication commit can lose its tree to shallow GC) does not penalize
/// the candidate, matching `validate_retired_event`'s own carve-out.
/// Deterministic (lexicographically smallest hash) when more than one
/// candidate exists. Returns the candidate's own `created_at` alongside its
/// hash — the backfilled `retired` event's `at` must be that commit's
/// `created_at` (05-runtime.md §3.5's "その event の commit created_at と
/// 一致"), never the fsck invocation's own clock.
fn find_republication_commit(
    raw_hash: &str,
    purge_in_commit: &str,
    commits: &BTreeMap<String, CommitObject>,
    trees: &BTreeMap<String, TreeObject>,
    reachable: &BTreeSet<String>,
) -> Option<(String, String)> {
    reachable
        .iter()
        .filter(|candidate| {
            candidate.as_str() != purge_in_commit
                && commits.get(candidate.as_str()).is_some_and(|commit| {
                    is_ancestor(commits, purge_in_commit, candidate)
                        && trees.get(&commit.tree).is_none_or(|tree| {
                            tree.entries.iter().any(|entry| entry.raw_hash == raw_hash)
                        })
                })
        })
        .min()
        .cloned()
        .map(|winner| {
            let created_at = commits
                .get(&winner)
                .expect("filtered candidates are always present in `commits`")
                .created_at
                .clone();
            (winner, created_at)
        })
}

/// LC17: `dead_raws` (skips normalized/manifest/chunk verification for a
/// purged raw_hash) now follows the canonical final event, not per-marker
/// presence — a `retired` canonical means the raw is alive again and must
/// NOT be treated as an explained dead terminal even if a (superseded)
/// tombstone/receipt event still exists earlier in that marker's history.
fn valid_dead_terminal(
    verified_raws: &BTreeSet<String>,
    purge: &PurgeState,
    raw_hash: &str,
    commits: &BTreeMap<String, CommitObject>,
    trees: &BTreeMap<String, TreeObject>,
    reachable: &BTreeSet<String>,
    invocation_time: &str,
) -> bool {
    if verified_raws.contains(raw_hash) {
        return false;
    }
    let lookup = canonical_lookup(purge, raw_hash, commits, trees, reachable, invocation_time);
    matches!(
        lookup.canonical.map(|canonical| canonical.event.kind),
        Some(EventKind::Purged) | Some(EventKind::Erased)
    )
}

enum RawRecovery {
    Missing(u64),
    Candidate(Vec<u8>),
    LimitExceeded,
}

fn recover_raw(path: &Path, expected_hash: &str, remaining_bytes: u64) -> Result<RawRecovery> {
    let bytes = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.len() > remaining_bytes => return Ok(RawRecovery::LimitExceeded),
        Ok(_) => match read_bounded_regular_file(path, MAX_RAW_OBJECT_BYTES.min(remaining_bytes)) {
            Ok(bytes) => bytes,
            Err(error) if error.error_code() == "KIO-E-STORE-OBJECT-OVERSIZED-001" => {
                return Ok(RawRecovery::LimitExceeded);
            }
            Err(error) => return Err(error),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RawRecovery::Missing(0));
        }
        Err(error) => return Err(KioError::io(error.to_string(), path.display().to_string())),
    };
    if hash_bytes(&bytes) != expected_hash {
        return Ok(RawRecovery::Missing(bytes.len() as u64));
    }
    Ok(RawRecovery::Candidate(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kio_core::dag::{
        CommitStats, CommitType, DEFAULT_CHUNKING_MAX_CHARS, DEFAULT_CHUNKING_STRATEGY, TreeEntry,
        chunking_config_hash,
    };
    use kio_pipeline::markdownize::{
        MarkdownizeMode, NormalizedUnitManifestEntry, NormalizedUnitObject, UnitStatus,
    };
    use kio_pipeline::prepare::UnitType;

    fn hash(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    #[test]
    fn batch_cache_hit_still_rechecks_scope_authority() {
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("isolated-data/kio/scope-registry.sqlite");
        let pointer = EvidencePointer {
            schema_version: 1,
            commit: hash('a'),
            tree: None,
            raw_hash: hash('b'),
            tool_profile_hash: hash('c'),
            chunk_hash: hash('d'),
            path_at_commit: None,
            heading_path: None,
            section_id: None,
            byte_start: None,
            byte_end: None,
            scope_id: "batch-cache-final-recheck-unreachable".to_owned(),
            scope_path: Some(dir.path().join("missing-scope").display().to_string()),
        };
        with_evidence_registry_path_for_test(registry_path.clone(), || {
            let mut bindings = bind_batch_scopes(std::slice::from_ref(&pointer), false).unwrap();
            let original = match &bindings[0].state {
                EvidenceScopeState::Unavailable(result) => result.clone(),
                EvidenceScopeState::Ready { .. } => panic!("fixture scope must be unreachable"),
            };
            let key = evidence_pointer_cache_key(&pointer).unwrap();
            let mut cache = BTreeMap::from([(key, original)]);

            let EvidenceScopeState::Unavailable(expected) = &mut bindings[0].state else {
                unreachable!("fixture scope must remain unreachable");
            };
            expected
                .result
                .details
                .as_object_mut()
                .unwrap()
                .insert("changed_after_cache_fill".to_owned(), json!(true));

            let error = verify_batch_row(&pointer, false, &bindings, &mut cache).unwrap_err();
            assert_eq!(error.error_code(), "KIO-E-EVIDENCE-BATCH-CHANGED-001");
        });
        assert!(!registry_path.exists());
        assert!(!registry_path.parent().unwrap().exists());
    }

    fn commit(kind: CommitType, created_at: &str) -> CommitObject {
        CommitObject::new(
            hash('a'),
            None,
            created_at.to_owned(),
            "marker test".to_owned(),
            hash('b'),
            CommitStats {
                files_added: 0,
                files_modified: 0,
                files_deleted: 0,
            },
            kind,
        )
        .unwrap()
    }

    fn purged_commit(created_at: &str, purged_raws: Vec<String>) -> CommitObject {
        CommitObject::new_purged(
            hash('a'),
            None,
            created_at.to_owned(),
            "marker test".to_owned(),
            hash('b'),
            CommitStats {
                files_added: 0,
                files_modified: 0,
                files_deleted: 0,
            },
            purged_raws,
        )
        .unwrap()
    }

    fn purged_event(in_commit: &str, at: &str) -> LifecycleEvent {
        LifecycleEvent::purged(
            at,
            in_commit,
            kio_core::purge::PurgeReason::Legal,
            "user",
            1,
        )
    }

    fn pinned_fixture(
        kio_dir: &Path,
    ) -> (
        String,
        NormalizeRef,
        NormalizedInstanceManifest,
        NormalizedUnitObject,
    ) {
        let raw_hash = hash('1');
        let profile_hash = hash('2');
        let prepared_hash = hash('3');
        let unit = NormalizedUnitObject {
            unit_key: "file:1".to_owned(),
            unit_type: UnitType::File,
            raw_hash: raw_hash.clone(),
            prepared_hash: prepared_hash.clone(),
            preparation_profile_hash: profile_hash.clone(),
            tool_profile_hash: profile_hash.clone(),
            r#gen: 4,
            mode: MarkdownizeMode::Full,
            markdown: "immutable markdown".to_owned(),
            owned_image_hashes: BTreeSet::new(),
            metadata: BTreeMap::new(),
            reused_from: None,
            generated_at: "2026-08-12T00:00:00Z".to_owned(),
        };
        let store = ObjectStore::new(kio_dir);
        let unit_bytes = canonical_json_bytes(&serde_json::to_value(&unit).unwrap()).unwrap();
        let unit_hash = store
            .write_content_object(ContentObjectKind::NormalizedUnit, &unit_bytes)
            .unwrap();
        let manifest = NormalizedInstanceManifest {
            raw_hash: raw_hash.clone(),
            tool_profile_hash: profile_hash.clone(),
            r#gen: 4,
            parent_gen: None,
            run_id: "test-run".to_owned(),
            units: vec![NormalizedUnitManifestEntry {
                order: 0,
                unit_key: unit.unit_key.clone(),
                unit_ref: kio_pipeline::prepare::unit_ref(&unit.unit_key),
                unit_type: UnitType::File,
                status: UnitStatus::Done,
                prepared_hash,
                preparation_profile_hash: profile_hash.clone(),
                unit_object_hash: Some(unit_hash),
                error_kind: None,
            }],
            generated_at: "2026-08-12T00:00:00Z".to_owned(),
        };
        let manifest_bytes =
            canonical_json_bytes(&serde_json::to_value(&manifest).unwrap()).unwrap();
        let manifest_hash = store
            .write_content_object(ContentObjectKind::Manifest, &manifest_bytes)
            .unwrap();
        (
            raw_hash,
            NormalizeRef {
                tool_profile_hash: profile_hash,
                r#gen: 4,
                manifest_hash,
            },
            manifest,
            unit,
        )
    }

    #[test]
    fn pinned_normalized_closure_uses_immutable_cas_and_rejects_tamper_or_wrong_tuple() {
        let dir = tempfile::tempdir().unwrap();
        let (raw_hash, normalize, manifest, _unit) = pinned_fixture(dir.path());
        let store = ObjectStore::new(dir.path());
        let mut state = State::default();
        let valid =
            load_pinned_normalized_instance(&store, dir.path(), &raw_hash, &normalize, &mut state)
                .unwrap();
        assert_eq!(valid.units.len(), 1);
        assert_eq!(state.inventoried_objects, 2);
        assert!(state.verified_bytes > 0);

        let unit_hash = manifest.units[0].unit_object_hash.as_ref().unwrap();
        let unit_path = store
            .content_path(ContentObjectKind::NormalizedUnit, unit_hash)
            .unwrap();
        std::fs::write(&unit_path, b"tampered").unwrap();
        let mut state = State::default();
        assert!(matches!(
            load_pinned_normalized_instance(&store, dir.path(), &raw_hash, &normalize, &mut state,),
            Err(PinnedNormalizedError::Corrupt(_))
        ));

        // A validly hashed unit object carrying a different tuple is equally
        // invalid: CAS integrity alone must not retarget a tree reference.
        let second = tempfile::tempdir().unwrap();
        let (raw_hash, mut normalize, mut manifest, mut unit) = pinned_fixture(second.path());
        unit.raw_hash = hash('9');
        let store = ObjectStore::new(second.path());
        let bytes = canonical_json_bytes(&serde_json::to_value(&unit).unwrap()).unwrap();
        manifest.units[0].unit_object_hash = Some(
            store
                .write_content_object(ContentObjectKind::NormalizedUnit, &bytes)
                .unwrap(),
        );
        let bytes = canonical_json_bytes(&serde_json::to_value(&manifest).unwrap()).unwrap();
        normalize.manifest_hash = store
            .write_content_object(ContentObjectKind::Manifest, &bytes)
            .unwrap();
        let mut state = State::default();
        assert!(matches!(
            load_pinned_normalized_instance(
                &store,
                second.path(),
                &raw_hash,
                &normalize,
                &mut state,
            ),
            Err(PinnedNormalizedError::Corrupt(_))
        ));
    }

    #[test]
    fn chunk_uses_exact_reachable_unit_content_not_a_same_tuple_fallback() {
        let raw_hash = hash('1');
        let profile_hash = hash('2');
        let first = NormalizedUnitObject {
            unit_key: "file:1".to_owned(),
            unit_type: UnitType::File,
            raw_hash: raw_hash.clone(),
            prepared_hash: hash('3'),
            preparation_profile_hash: profile_hash.clone(),
            tool_profile_hash: profile_hash.clone(),
            r#gen: 4,
            mode: MarkdownizeMode::Full,
            markdown: "first body".to_owned(),
            owned_image_hashes: BTreeSet::new(),
            metadata: BTreeMap::new(),
            reused_from: None,
            generated_at: "2026-08-12T00:00:00Z".to_owned(),
        };
        let mut second = first.clone();
        second.markdown = "second body".to_owned();
        second.prepared_hash = hash('4');
        // The fixture deliberately models two reachable manifests with the
        // same (raw, profile, generation, unit_key), but different Markdown
        // bodies. A chunk must select the latter by `unit_content_hash`.
        let first_hash = hash_bytes(first.markdown.as_bytes());
        let second_hash = hash_bytes(second.markdown.as_bytes());
        let mut reachable = BTreeMap::new();
        reachable.insert(
            first_hash,
            vec![ValidatedNormalizedInstanceUnit {
                raw_hash: raw_hash.clone(),
                tool_profile_hash: profile_hash.clone(),
                r#gen: 4,
                unit: first,
            }],
        );
        reachable.insert(
            second_hash.clone(),
            vec![ValidatedNormalizedInstanceUnit {
                raw_hash: raw_hash.clone(),
                tool_profile_hash: profile_hash.clone(),
                r#gen: 4,
                unit: second,
            }],
        );
        let mut chunk = ChunkObject {
            spec_version: 1,
            raw_hash: raw_hash.clone(),
            tool_profile_hash: profile_hash.clone(),
            r#gen: 4,
            unit_key: "file:1".to_owned(),
            unit_content_hash: second_hash,
            heading_path: Vec::new(),
            section_id: None,
            byte_start: 0,
            byte_end: "second".len() as u64,
            text_hash: hash_bytes(b"second"),
            text: "second".to_owned(),
        };
        let selected = exact_pinned_content_unit_for_chunk(&chunk, &reachable).unwrap();
        assert_eq!(
            selected.unit.markdown.get(0..6),
            Some("second"),
            "the chunk must not fall back to the first same-tuple manifest"
        );

        chunk.unit_content_hash = hash('c');
        assert!(matches!(
            exact_pinned_content_unit_for_chunk(&chunk, &reachable),
            Err(ChunkPinnedUnitError::Unreachable)
        ));

        // Equal Markdown can be valid for several document tuples; select the
        // tuple-matching candidate rather than treating the first one as
        // authoritative.
        let mut same_content_other_tuple = reachable[&hash_bytes(b"second body")][0].clone();
        same_content_other_tuple.raw_hash = hash('9');
        reachable
            .get_mut(&hash_bytes(b"second body"))
            .unwrap()
            .push(same_content_other_tuple);
        chunk.unit_content_hash = hash_bytes(b"second body");
        chunk.raw_hash = hash('9');
        assert!(exact_pinned_content_unit_for_chunk(&chunk, &reachable).is_ok());

        chunk.tool_profile_hash = hash('8');
        assert!(matches!(
            exact_pinned_content_unit_for_chunk(&chunk, &reachable),
            Err(ChunkPinnedUnitError::IdentityMismatch)
        ));
    }

    /// R23-08 test helper: [`commit`], parameterized on `tree`/`parent` so a
    /// candidate republication commit's tree membership and ancestry can be
    /// controlled independently.
    fn commit_with_tree(
        kind: CommitType,
        created_at: &str,
        tree: &str,
        parent: Option<String>,
    ) -> CommitObject {
        CommitObject::new(
            tree.to_owned(),
            parent,
            created_at.to_owned(),
            "marker test".to_owned(),
            hash('b'),
            CommitStats {
                files_added: 0,
                files_modified: 0,
                files_deleted: 0,
            },
            kind,
        )
        .unwrap()
    }

    #[test]
    fn r23_08_find_republication_commit_requires_a_raw_leaf_and_returns_created_at() {
        let purge_commit_hash = hash('c');
        let target_raw = hash('9');
        let other_raw = hash('8');
        let purge_created_at = "2026-07-13T00:00:00Z";
        let commits_only_purge = BTreeMap::from([(
            purge_commit_hash.clone(),
            purged_commit(purge_created_at, vec![target_raw.clone()]),
        )]);

        // A candidate that is ref-reachable and a strict descendant of the
        // purge commit, but whose tree does NOT carry a leaf for the target
        // raw_hash, must be rejected -- ancestry alone is not enough (it
        // could be an unrelated later commit that never republished this
        // raw at all).
        let bad_candidate_hash = hash('d');
        let bad_tree_hash = hash('e');
        let mut commits = commits_only_purge.clone();
        commits.insert(
            bad_candidate_hash.clone(),
            commit_with_tree(
                CommitType::Auto,
                "2026-07-14T00:00:00Z",
                &bad_tree_hash,
                Some(purge_commit_hash.clone()),
            ),
        );
        let trees = BTreeMap::from([(
            bad_tree_hash.clone(),
            TreeObject {
                chunking_config_hash: chunking_config_hash(
                    DEFAULT_CHUNKING_STRATEGY,
                    DEFAULT_CHUNKING_MAX_CHARS,
                )
                .unwrap(),
                entries: vec![TreeEntry::raw_file("unrelated.txt", other_raw.clone()).unwrap()],
                object_type: "tree".to_owned(),
            },
        )]);
        let reachable = BTreeSet::from([purge_commit_hash.clone(), bad_candidate_hash.clone()]);
        assert!(
            find_republication_commit(
                &target_raw,
                &purge_commit_hash,
                &commits,
                &trees,
                &reachable,
            )
            .is_none()
        );

        // A candidate whose tree DOES carry the leaf is accepted, and its
        // OWN `created_at` -- not the caller's invocation time -- is
        // returned alongside its hash (R23-08).
        let good_candidate_hash = hash('f');
        let good_tree_hash = hash('0');
        let good_created_at = "2026-07-15T00:00:00Z";
        commits.insert(
            good_candidate_hash.clone(),
            commit_with_tree(
                CommitType::Auto,
                good_created_at,
                &good_tree_hash,
                Some(purge_commit_hash.clone()),
            ),
        );
        let mut trees_with_good = trees.clone();
        trees_with_good.insert(
            good_tree_hash.clone(),
            TreeObject {
                chunking_config_hash: chunking_config_hash(
                    DEFAULT_CHUNKING_STRATEGY,
                    DEFAULT_CHUNKING_MAX_CHARS,
                )
                .unwrap(),
                entries: vec![TreeEntry::raw_file("resurrected.txt", target_raw.clone()).unwrap()],
                object_type: "tree".to_owned(),
            },
        );
        let reachable_both = BTreeSet::from([
            purge_commit_hash.clone(),
            bad_candidate_hash.clone(),
            good_candidate_hash.clone(),
        ]);
        let (winner, winner_created_at) = find_republication_commit(
            &target_raw,
            &purge_commit_hash,
            &commits,
            &trees_with_good,
            &reachable_both,
        )
        .expect("a valid resurrection candidate exists");
        assert_eq!(winner, good_candidate_hash);
        assert_eq!(winner_created_at, good_created_at);

        // A candidate whose tree is absent (shallow-discarded) is not
        // penalized -- "tree 存置時に限り検証する" (05-runtime.md §3.5 /
        // 10-operations.md §7.5.1).
        let shallow_candidate_hash = hash('1');
        let shallow_tree_hash = hash('2');
        let mut commits_shallow = commits_only_purge.clone();
        commits_shallow.insert(
            shallow_candidate_hash.clone(),
            commit_with_tree(
                CommitType::Auto,
                "2026-07-16T00:00:00Z",
                &shallow_tree_hash,
                Some(purge_commit_hash.clone()),
            ),
        );
        let reachable_shallow =
            BTreeSet::from([purge_commit_hash.clone(), shallow_candidate_hash.clone()]);
        let (winner, _) = find_republication_commit(
            &target_raw,
            &purge_commit_hash,
            &commits_shallow,
            &BTreeMap::new(), // no trees inventoried -- shallow
            &reachable_shallow,
        )
        .expect("a shallow-tree candidate is still accepted (tree absent skips the leaf check)");
        assert_eq!(winner, shallow_candidate_hash);
    }

    #[test]
    fn lc17_in_commit_binding_requires_reachable_purged_exact_non_future_commit_and_purged_raws_membership()
     {
        let commit_hash = hash('c');
        let target_raw = hash('9');
        let created_at = "2026-07-13T00:00:00.25Z";
        let mut commits = BTreeMap::from([(
            commit_hash.clone(),
            purged_commit(created_at, vec![target_raw.clone()]),
        )]);
        let reachable = BTreeSet::from([commit_hash.clone()]);
        let event = purged_event(&commit_hash, created_at);

        assert!(
            validate_purge_or_erase_in_commit(
                &target_raw,
                &event,
                &commits,
                &reachable,
                "2026-07-13T00:00:01Z"
            )
            .is_ok()
        );
        // Not ref-reachable.
        assert!(
            validate_purge_or_erase_in_commit(
                &target_raw,
                &event,
                &commits,
                &BTreeSet::new(),
                "2026-07-13T00:00:01Z"
            )
            .is_err()
        );
        // `at` does not equal commit.created_at.
        let mismatched_at = purged_event(&commit_hash, "2026-07-13T00:00:00Z");
        assert!(
            validate_purge_or_erase_in_commit(
                &target_raw,
                &mismatched_at,
                &commits,
                &reachable,
                "2026-07-13T00:00:01Z"
            )
            .is_err()
        );
        // commit_type is not purged.
        commits.insert(commit_hash.clone(), commit(CommitType::Manual, created_at));
        assert!(
            validate_purge_or_erase_in_commit(
                &target_raw,
                &event,
                &commits,
                &reachable,
                "2026-07-13T00:00:01Z"
            )
            .is_err()
        );
        // purged_raws does not include this raw_hash (borrowed marker defense).
        commits.insert(
            commit_hash.clone(),
            purged_commit(created_at, vec![hash('8')]),
        );
        assert!(
            validate_purge_or_erase_in_commit(
                &target_raw,
                &event,
                &commits,
                &reachable,
                "2026-07-13T00:00:01Z"
            )
            .is_err()
        );
        // `at` in the future relative to the fixed invocation time.
        commits.insert(
            commit_hash.clone(),
            purged_commit(created_at, vec![target_raw.clone()]),
        );
        assert!(
            validate_purge_or_erase_in_commit(
                &target_raw,
                &event,
                &commits,
                &reachable,
                "2026-07-13T00:00:00.2Z"
            )
            .is_err()
        );
    }

    #[test]
    fn historical_absence_authority_requires_exact_all_appearance_purge_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let purge = PurgeState::open(dir.path()).unwrap();
        let raw = hash('1');
        let old = hash('2');
        let purged = hash('3');
        let gap = hash('4');
        let resurrected = hash('5');
        let old_tree = hash('6');
        let new_tree = hash('7');
        let empty_tree = hash('8');
        let reference = NormalizeRef {
            tool_profile_hash: hash('a'),
            r#gen: 0,
            manifest_hash: hash('b'),
        };
        let mut newer = reference.clone();
        newer.manifest_hash = hash('c');
        let tree = |reference: NormalizeRef| {
            let mut entry = TreeEntry::raw_file("doc.md", raw.clone()).unwrap();
            entry.normalize = Some(reference);
            TreeObject {
                object_type: "tree".into(),
                chunking_config_hash: hash('d'),
                entries: vec![entry],
            }
        };
        let mut purge_commit = purged_commit("2026-07-13T00:00:01Z", vec![raw.clone()]);
        purge_commit.parent = Some(old.clone());
        purge_commit.tree = empty_tree.clone();
        let commits = BTreeMap::from([
            (
                old.clone(),
                commit_with_tree(CommitType::Manual, "2026-07-13T00:00:00Z", &old_tree, None),
            ),
            (purged.clone(), purge_commit),
            (
                gap.clone(),
                commit_with_tree(
                    CommitType::Manual,
                    "2026-07-13T00:00:02Z",
                    &empty_tree,
                    Some(purged.clone()),
                ),
            ),
            (
                resurrected.clone(),
                commit_with_tree(
                    CommitType::Manual,
                    "2026-07-13T00:00:03Z",
                    &new_tree,
                    Some(gap.clone()),
                ),
            ),
        ]);
        let reachable: BTreeSet<_> = commits.keys().cloned().collect();
        let mut trees = BTreeMap::from([
            (old_tree.clone(), tree(reference.clone())),
            (new_tree.clone(), tree(newer)),
        ]);
        purge
            .append_tombstone_event(&raw, purged_event(&purged, "2026-07-13T00:00:01Z"))
            .unwrap();
        purge
            .retire_tombstone(&raw, &resurrected, "2026-07-13T00:00:03Z", "user")
            .unwrap();
        let classify =
            |commits: &BTreeMap<_, _>, trees: &BTreeMap<_, _>, reachable: &BTreeSet<_>, head| {
                explained_historical_manifests(
                    &purge,
                    commits,
                    trees,
                    reachable,
                    head,
                    "2026-07-14T00:00:00Z",
                )
            };
        assert!(
            classify(&commits, &trees, &reachable, Some(&resurrected))
                .contains_key(&reference.manifest_hash),
            "same generation with different manifest is not the old closure"
        );
        assert!(
            !classify(&commits, &trees, &reachable, Some(&old))
                .contains_key(&reference.manifest_hash),
            "HEAD cannot borrow historical absence"
        );
        trees.insert(new_tree.clone(), tree(reference.clone()));
        assert!(
            !classify(&commits, &trees, &reachable, Some(&resurrected))
                .contains_key(&reference.manifest_hash),
            "current exact reuse disqualifies old appearances"
        );
        trees.remove(&new_tree);
        let mut gap_commits = commits.clone();
        gap_commits.get_mut(&gap).unwrap().tree = old_tree;
        assert!(
            !classify(&gap_commits, &trees, &reachable, Some(&resurrected))
                .contains_key(&reference.manifest_hash),
            "post-purge pre-resurrection appearance is not explained"
        );
        let mut unreachable = reachable.clone();
        unreachable.remove(&purged);
        assert!(
            classify(&commits, &trees, &unreachable, Some(&resurrected)).is_empty(),
            "unreachable lifecycle cannot authorize absence"
        );
        let mut malformed = commits.clone();
        malformed.get_mut(&purged).unwrap().purged_raws.clear();
        assert!(
            classify(&malformed, &trees, &reachable, Some(&resurrected)).is_empty(),
            "mismatched lifecycle binding cannot authorize absence"
        );
    }

    #[test]
    fn byte_and_object_bounds_are_global_and_exact() {
        let mut state = State {
            max_objects: 2,
            max_verified_bytes: 5,
            ..State::default()
        };
        state.add_bytes(2);
        state.add_bytes(3);
        assert!(!state.exceeded_bounds);
        state.add_bytes(1);
        assert!(state.exceeded_bounds);
        state.inventoried_objects = 3;
        assert!(state.inventoried_objects > state.max_objects);
    }

    #[test]
    fn injected_fsck_limits_accept_exact_and_reject_one_beyond_without_refs_mutation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.md"), "bounded fsck").unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.snapshot(Some("fixture"), Some("2026-07-13T00:00:00Z"))
            .unwrap();
        let baseline = verify_objects(&repo).unwrap();
        assert!(!baseline.has_remaining_findings());
        let head_before = std::fs::read(repo.kio_dir().join("HEAD")).unwrap();

        let exact = verify_objects_with_limits(
            &repo,
            VerifyLimits {
                max_objects: baseline.inventoried_objects,
                max_verified_bytes: baseline.verified_bytes,
            },
        )
        .unwrap();
        assert!(!exact.has_remaining_findings());

        let bytes_over = verify_objects_with_limits(
            &repo,
            VerifyLimits {
                max_objects: baseline.inventoried_objects,
                max_verified_bytes: baseline.verified_bytes.saturating_sub(1),
            },
        )
        .unwrap();
        assert!(bytes_over.has_remaining_findings());
        let object_over = verify_objects_with_limits(
            &repo,
            VerifyLimits {
                max_objects: baseline.inventoried_objects.saturating_sub(1),
                max_verified_bytes: baseline.verified_bytes,
            },
        )
        .unwrap();
        assert!(object_over.has_remaining_findings());
        assert_eq!(
            std::fs::read(repo.kio_dir().join("HEAD")).unwrap(),
            head_before
        );
    }

    #[test]
    fn active_or_corrupt_purge_journal_stops_before_any_object_read() {
        let dir = tempfile::tempdir().unwrap();
        let contents = b"private purge target";
        std::fs::write(dir.path().join("private.md"), contents).unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.snapshot(Some("fixture"), Some("2026-07-13T00:00:00Z"))
            .unwrap();
        let purge = PurgeState::open(repo.kio_dir()).unwrap();
        purge
            .begin(
                vec![hash_bytes(contents)],
                kio_core::purge::PurgeReason::Legal,
                kio_core::purge::TombstoneMode::Default,
                "user",
                "2026-07-13T00:00:01Z",
                1,
                hash_bytes(b"planned commit placeholder"),
                hash_bytes(b"planned closure placeholder"),
                kio_core::scope::new_ulid(dir.path()),
            )
            .unwrap();

        let active = verify_objects(&repo).unwrap();
        assert!(active.has_remaining_findings());
        assert_eq!(active.checked.raw, 0);
        assert_eq!(active.checked.chunks, 0);
        assert_eq!(active.checked.trees, 0);
        assert_eq!(active.checked.commits, 0);
        assert_eq!(active.checked.normalized_instances, 0);
        assert_eq!(active.verified_bytes, 0);
        assert_eq!(active.inventoried_objects, 0);
        assert_eq!(active.remaining_findings[0].kind, "purge_incomplete");

        std::fs::write(purge.journal_path(), b"not-json").unwrap();
        let corrupt = verify_objects(&repo).unwrap();
        assert!(corrupt.has_remaining_findings());
        assert_eq!(corrupt.checked.raw, 0);
        assert_eq!(corrupt.verified_bytes, 0);
        assert_eq!(corrupt.inventoried_objects, 0);
        assert_eq!(corrupt.remaining_findings[0].kind, "purge_journal_corrupt");
    }

    #[test]
    fn raw_substitution_only_applies_when_prepared_slot_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let kio_dir = dir.path().join(".kio");
        std::fs::create_dir(&kio_dir).unwrap();
        let store = ObjectStore::new(&kio_dir);
        let hash = store.write_raw(b"direct prepared bytes").unwrap();
        assert_eq!(verify_prepared_reference(&store, &hash, true).unwrap(), 0);

        let digest = hash.strip_prefix("sha256:").unwrap();
        let corrupt = kio_dir
            .join("objects/prepared")
            .join(&digest[..2])
            .join(&digest[2..4])
            .join(digest);
        std::fs::create_dir_all(corrupt.parent().unwrap()).unwrap();
        std::fs::write(&corrupt, b"corrupt prepared bytes").unwrap();
        assert!(verify_prepared_reference(&store, &hash, true).is_err());
    }

    #[test]
    fn names_ledger_accepts_simple_folded_tag_identity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.md"), "tag target").unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let head = repo
            .snapshot(Some("fixture"), Some("2026-10-03T00:00:00Z"))
            .unwrap()
            .commit_hash
            .unwrap();
        repo.tag("Σ", None).unwrap();
        assert_eq!(repo.resolve_commit("ς").unwrap(), head);
        assert!(!verify_objects(&repo).unwrap().has_remaining_findings());
    }

    #[test]
    fn names_ledger_rejects_unicode_16_unassigned_logical_name() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let name = "future\u{0378}";
        let names = names_jsonl_path(repo.kio_dir());
        std::fs::create_dir_all(names.parent().unwrap()).unwrap();
        let row = json!({
            "digest64": portable_tag_digest64(name),
            "logical_name": name,
            "recorded_at": "2026-10-03T00:00:00Z",
        });
        std::fs::write(&names, format!("{row}\n")).unwrap();

        let report = verify_objects(&repo).unwrap();
        assert!(report.remaining_findings.iter().any(|finding| {
            finding.kind == "names_jsonl_corrupt"
                && finding.reason.contains("unassigned in Unicode 16")
        }));
    }

    /// A tag ref pointing at a hash with no commit object behind it is a
    /// dangling root, not a silently-skipped ref.
    #[test]
    fn a_tag_ref_to_a_missing_commit_is_a_finding() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.md"), "tag target").unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.snapshot(Some("fixture"), Some("2026-07-13T00:00:00Z"))
            .unwrap();
        let tag = repo
            .kio_dir()
            .join("refs/tags-v1")
            .join(kio_core::portable::portable_tag_leaf("Release"));
        std::fs::create_dir_all(tag.parent().unwrap()).unwrap();
        std::fs::write(&tag, hash('f')).unwrap();

        let report = verify_objects(&repo).unwrap();
        assert!(
            report
                .remaining_findings
                .iter()
                .any(|finding| finding.object_hash == hash('f'))
        );
    }

    #[test]
    fn failed_corrupt_reads_obey_the_exact_global_byte_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let original = b"accounted raw fixture";
        std::fs::write(dir.path().join("doc.md"), original).unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.snapshot(Some("fixture"), Some("2026-07-13T00:00:00Z"))
            .unwrap();
        let raw_hash = hash_bytes(original);
        let raw_path = ObjectStore::new(repo.kio_dir())
            .object_path(ObjectKind::Raw, &raw_hash)
            .unwrap();
        std::fs::write(&raw_path, vec![b'x'; original.len()]).unwrap();
        std::fs::write(dir.path().join("doc.md"), vec![b'y'; original.len()]).unwrap();

        let baseline = verify_objects(&repo).unwrap();
        assert!(
            baseline
                .remaining_findings
                .iter()
                .any(|finding| finding.kind == "raw_corrupt")
        );
        let exact = verify_objects_with_limits(
            &repo,
            VerifyLimits {
                max_objects: baseline.inventoried_objects,
                max_verified_bytes: baseline.verified_bytes,
            },
        )
        .unwrap();
        assert!(
            !exact
                .remaining_findings
                .iter()
                .any(|finding| finding.kind == "inventory_limit")
        );
        let one_under = verify_objects_with_limits(
            &repo,
            VerifyLimits {
                max_objects: baseline.inventoried_objects,
                max_verified_bytes: baseline.verified_bytes.saturating_sub(1),
            },
        )
        .unwrap();
        assert!(
            one_under
                .remaining_findings
                .iter()
                .any(|finding| finding.kind == "inventory_limit")
        );
    }

    #[test]
    fn logical_object_count_excludes_directories_and_counts_invalid_leaves_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.md"), "logical count").unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.snapshot(Some("fixture"), Some("2026-07-13T00:00:00Z"))
            .unwrap();
        let baseline = verify_objects(&repo).unwrap();
        let extra = repo.kio_dir().join("objects/raw/fe/ed/empty-directory");
        std::fs::create_dir_all(&extra).unwrap();
        let with_directory = verify_objects(&repo).unwrap();
        assert_eq!(
            with_directory.inventoried_objects,
            baseline.inventoried_objects
        );

        std::fs::write(extra.parent().unwrap().join("invalid-leaf"), b"x").unwrap();
        let with_invalid_leaf = verify_objects(&repo).unwrap();
        assert_eq!(
            with_invalid_leaf.inventoried_objects,
            baseline.inventoried_objects + 1
        );
    }

    /// H2-4 (R24b, 3/3 系統一致・うち 2 件 fatal): the plan BINDS the deletion.
    ///
    /// `repair --prune-orphans` used to count with one scan, ask, then delete
    /// with a SECOND scan that re-derived the targets. Anything that became an
    /// orphan between the two — another process finishing, a concurrent
    /// command — was deleted without ever having been shown to the user. The
    /// prompt 06 §1 requires is only meaningful if what is approved is what is
    /// removed, so `apply` now takes the plan and touches nothing else.
    #[test]
    fn apply_removes_only_what_the_plan_listed() {
        let _environment = kio_core::test_control::test_env_lock().lock().unwrap();
        let device = tempfile::tempdir().unwrap();
        let _data =
            kio_core::test_control::TestEnvGuard::set("XDG_DATA_HOME", device.path().join("data"));
        let _cache = kio_core::test_control::TestEnvGuard::set(
            "XDG_CACHE_HOME",
            device.path().join("cache"),
        );
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.snapshot(Some("fixture"), Some("2026-07-13T00:00:00Z"))
            .unwrap();
        let store = ObjectStore::new(repo.kio_dir());

        // An orphan that exists when the user is asked.
        let approved = store
            .write_content_object(ContentObjectKind::Prepared, b"approved orphan")
            .unwrap();
        let plan = prune_orphans_plan(&repo).unwrap();
        assert!(
            plan.prepared.contains(&approved),
            "the fixture orphan must be in the plan: {plan:?}"
        );

        // A NEW orphan appears after the preview — exactly the race the split
        // exists to close.
        let latecomer = store
            .write_content_object(ContentObjectKind::Prepared, b"appeared after the prompt")
            .unwrap();

        let report = prune_orphans_apply(&repo, plan).unwrap();
        assert_eq!(report.status, "pruned");
        assert!(
            store
                .content_path(ContentObjectKind::Prepared, &latecomer)
                .unwrap()
                .exists(),
            "an orphan created after the preview must NOT be deleted"
        );
        assert!(
            !store
                .content_path(ContentObjectKind::Prepared, &approved)
                .unwrap()
                .exists(),
            "the approved orphan must be deleted"
        );
    }

    /// A blocked plan deletes nothing, and `apply` cannot be talked into it.
    #[test]
    fn a_blocked_plan_applies_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.snapshot(Some("fixture"), Some("2026-07-13T00:00:00Z"))
            .unwrap();
        let store = ObjectStore::new(repo.kio_dir());
        let orphan = store
            .write_content_object(ContentObjectKind::Prepared, b"orphan")
            .unwrap();

        let blocked = PruneOrphansPlan::blocked_for_test("active_purge_journal");
        let report = prune_orphans_apply(&repo, blocked).unwrap();
        assert_eq!(report.status, "blocked");
        assert_eq!(report.blocked_by.as_deref(), Some("active_purge_journal"));
        assert_eq!(report.pruned_prepared_count, 0);
        assert!(
            store
                .content_path(ContentObjectKind::Prepared, &orphan)
                .unwrap()
                .exists(),
            "a blocked plan must remove nothing"
        );
    }
}
