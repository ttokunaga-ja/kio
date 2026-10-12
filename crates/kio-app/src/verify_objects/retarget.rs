use super::*;
use crate::commands::EvidenceRetargetArgs;
use crate::historical_reindex::pinned_done_units;
use crate::open_view::AssociationCheck;
use crate::open_view::MAX_MANIFEST_OBJECT_READ_BYTES;
use crate::open_view::check_publication_and_association_on_connection;
use crate::open_view::parse_object_uri;
use crate::open_view::parse_pointer_text;
use crate::open_view::read_pointer_input;
use crate::scope_access::is_store_not_found;
use crate::scope_access::unresolvable_commit_pointer_error;
use kio_index::fts::RetargetCandidates;
use kio_search::evidence::EvidencePointer;
use kio_search::evidence::EvidencePointerIssueRequest;
use kio_search::evidence::issue_evidence_pointer;

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
pub(super) fn run_evidence_retarget(
    ctx: &crate::invocation::Invocation<'_>,
    registry_input: &EvidenceRegistry,
    args: EvidenceRetargetArgs,
) -> Result<Value> {
    if !is_hash(&args.at) {
        return Err(KioError::invalid_usage(
            "--at must be a full lowercase `sha256:` object hash",
        ));
    }
    let raw = read_pointer_input(ctx, vec![args.pointer])?;
    if raw.starts_with("sha256:") || parse_object_uri(&raw)?.is_some() {
        return Err(KioError::invalid_usage(
            "evidence retarget accepts only a pointer URI, inline JSON, or '-' stdin",
        ));
    }
    let pointer = parse_pointer_text(&raw)?;
    kio_core::cas::VerifiedCasReadBudget::new()
        .run(MAX_RETARGET_VERIFIED_BYTES, |reads| {
            run_evidence_retarget_bound(ctx, registry_input, &pointer, &args.at, reads)
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

fn run_evidence_retarget_bound(
    ctx: &crate::invocation::Invocation<'_>,
    registry_input: &EvidenceRegistry,
    pointer: &EvidencePointer,
    at: &str,
    reads: &kio_core::cas::VerifiedCasReadCapability,
) -> Result<Value> {
    let binding = bind_evidence_scope(ctx, registry_input, pointer, true, reads)?;
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
    let store = repo.object_store();
    if let Some(marker_verification) =
        canonical_pointer_marker_verification(pointer, true, target, &store, checkpoint)?
    {
        binding.recheck(ctx, registry_input)?;
        return Err(retarget_old_verification_error(
            marker_verification,
            pointer,
        ));
    }
    if let Some(tree) = &old_tree
        && let Some(entry) = canonical_pointer_tree_entry(tree, pointer)
        && let Some(normalize) = &entry.normalize
    {
        retarget_manifest_unit_preflight(&store, &pointer.raw_hash, normalize, false)?;
    }
    // Run the same canonical verifier used by `evidence verify` before doing
    // any retarget-specific reconstruction.  Optional presentation fields on
    // the input have no effect on this judgment.
    let canonical = verify_pointer_in_scope(pointer, true, target, repo, checkpoint, source_index)
        .map_err(map_retarget_old_authority_error)?;
    if canonical.result.status != EvidenceStatus::Alive {
        // A final retained binding check takes precedence over a stale
        // classification produced before a concurrent purge/index change.
        binding.recheck(ctx, registry_input)?;
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
    let target_commit = read_exact_retarget_commit(&store, at)?;
    let target_tree = read_retarget_target_tree(repo, at, &target_commit)?;
    let target_entry = target_tree
        .entries
        .iter()
        .find(|entry| entry.path == old_entry.path && entry.raw_hash == pointer.raw_hash)
        .ok_or_else(retarget_not_found_error)?;
    let target_normalize = target_entry
        .normalize
        .as_ref()
        .ok_or_else(retarget_not_found_error)?;
    retarget_manifest_unit_preflight(&store, &pointer.raw_hash, target_normalize, true)?;
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
    binding.recheck(ctx, registry_input)?;
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
    let verification = verify_pointer_for_cli(ctx, registry_input, &issued, true, reads)?;
    // Drift checks take precedence over a stale final-verifier result.  Only
    // classify a stable non-alive result after every retained authority still
    // denotes the command-start snapshot.
    binding.recheck(ctx, registry_input)?;
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
fn read_retarget_target_tree(
    repo: &Repository,
    at: &str,
    commit: &CommitObject,
) -> Result<TreeObject> {
    match repo.read_tree(&commit.tree) {
        Ok(tree) => Ok(tree),
        Err(error) if is_store_not_found(&error) => {
            GcSweepSession::bind_repository(repo)?.validate_final_shallow_tree(at, &commit.tree)?;
            Err(retarget_shallow_error(
                "target Evidence commit is shallow; retarget requires its tree",
                at,
            ))
        }
        Err(error) if error.exit_code() == ExitCode::InvalidUsage => Err(retarget_error(
            "KIO-E-STORE-CORRUPT-001",
            "target Evidence tree contradicts its commit CAS object",
        )),
        Err(error) => Err(error),
    }
}

fn read_exact_retarget_commit(store: &ObjectStore, at: &str) -> Result<CommitObject> {
    let object = store.read_by_hash(at).map_err(|error| {
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
    store: &ObjectStore,
    raw_hash: &str,
    normalize: &NormalizeRef,
    missing_is_corrupt: bool,
) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retarget_missing_tree_receipt_reads_share_the_active_owner() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        std::fs::write(root.path().join("file.txt"), b"retarget budget fixture").unwrap();
        repo.snapshot(
            Some("retarget budget fixture"),
            Some("2026-08-14T00:00:00Z"),
        )
        .unwrap();
        let at = repo.head_commit_hash().unwrap().unwrap();
        let commit = repo.read_commit(&at).unwrap();
        let path = ObjectStore::new(repo.kio_dir())
            .object_path(ObjectKind::Tree, &commit.tree)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        let error = kio_core::cas::VerifiedCasReadBudget::new()
            .run(0, |reads| {
                let repo = repo.with_verified_reads(reads)?;
                read_retarget_target_tree(&repo, &at, &commit)
            })
            .unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-STORE-VERIFIED-BYTES-LIMIT-001");
        assert_eq!(error.context()["consumed_bytes"], 0);
    }
}
