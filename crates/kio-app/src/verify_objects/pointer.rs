use super::*;

pub(super) fn run_evidence_verify(
    ctx: &crate::invocation::Invocation<'_>,
    registry_input: &EvidenceRegistry,
    args: EvidenceVerifyArgs,
) -> Result<crate::response::CommandResponse> {
    if let Some(batch) = args.batch {
        return run_evidence_batch(ctx, registry_input, &batch, args.strict);
    }
    let pointer_operand = args.pointer.expect("clap requires pointer or --batch");
    let strict = args.strict;
    let raw = read_pointer_input(ctx, vec![pointer_operand])?;
    if raw.starts_with("sha256:") || parse_object_uri(&raw)?.is_some() {
        return Err(KioError::invalid_usage(
            "evidence verify accepts only a pointer URI, inline JSON, or '-' stdin",
        ));
    }
    let pointer = parse_pointer_text(&raw)?;
    let (verification, _) = kio_core::cas::VerifiedCasReadBudget::new().run(u64::MAX, |reads| {
        verify_pointer_for_cli(ctx, registry_input, &pointer, strict, reads)
    })?;
    response::verification_output(verification)
}

pub(super) fn verify_batch_row(
    ctx: &crate::invocation::Invocation<'_>,
    registry_input: &EvidenceRegistry,
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
    binding.reads.ensure_active()?;
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
    binding.recheck(ctx, registry_input)?;
    cache.entry(key).or_insert_with(|| verification.clone());
    Ok(verification)
}

pub(super) fn evidence_pointer_cache_key(pointer: &EvidencePointer) -> Result<Vec<u8>> {
    let value =
        serde_json::to_value(pointer).map_err(|error| KioError::schema(error.to_string()))?;
    canonical_json_bytes(&value)
}

pub(super) fn bind_batch_scopes(
    ctx: &crate::invocation::Invocation<'_>,
    registry_input: &EvidenceRegistry,
    pointers: &[EvidencePointer],
    strict: bool,
    reads: &kio_core::cas::VerifiedCasReadCapability,
) -> Result<Vec<EvidenceScopeBinding>> {
    let mut bindings = Vec::new();
    let mut seen = BTreeSet::new();
    for pointer in pointers {
        let key = (pointer.scope_id.clone(), pointer.scope_path.clone());
        if !seen.insert(key) {
            continue;
        }
        bindings.push(bind_evidence_scope(
            ctx,
            registry_input,
            pointer,
            strict,
            reads,
        )?);
    }
    Ok(bindings)
}

pub(super) fn batch_changed_error() -> KioError {
    KioError::new(
        "KIO-E-EVIDENCE-BATCH-CHANGED-001",
        "evidence scope authority changed while verifying batch; retry",
        json!({}),
        ExitCode::PartialFailure,
    )
}

pub(super) fn index_generation_for_evidence(kio_dir: &Path) -> Result<Option<String>> {
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

pub(super) fn parse_batch_pointers(bytes: &[u8]) -> Result<Vec<EvidencePointer>> {
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

pub(super) fn batch_limit_error(line: usize, limit: &str, maximum: usize) -> KioError {
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
pub(super) fn verify_pointer_for_cli(
    ctx: &crate::invocation::Invocation<'_>,
    registry_input: &EvidenceRegistry,
    pointer: &EvidencePointer,
    strict: bool,
    reads: &kio_core::cas::VerifiedCasReadCapability,
) -> Result<EvidenceVerification> {
    let binding = bind_evidence_scope(ctx, registry_input, pointer, strict, reads)?;
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

pub(super) fn unavailable_evidence_result(
    ctx: &crate::invocation::Invocation<'_>,
    registry_input: &EvidenceRegistry,
    scope_id: &str,
    scope_path: Option<&str>,
    strict_exit: ExitCode,
) -> Result<EvidenceVerification> {
    match resolve_evidence_scope_target(
        ctx.cwd(),
        registry_input,
        scope_id,
        scope_path,
        ctx.control,
    ) {
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

pub(super) fn is_registry_snapshot_error(error: &KioError) -> bool {
    matches!(
        error.error_code(),
        "KIO-E-REGISTRY-SNAPSHOT-001" | "KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001"
    )
}

pub(super) fn bind_evidence_scope(
    ctx: &crate::invocation::Invocation<'_>,
    registry_input: &EvidenceRegistry,
    pointer: &EvidencePointer,
    strict: bool,
    reads: &kio_core::cas::VerifiedCasReadCapability,
) -> Result<EvidenceScopeBinding> {
    reads.ensure_active()?;
    let target = match resolve_evidence_scope_target(
        ctx.cwd(),
        registry_input,
        &pointer.scope_id,
        pointer.scope_path.as_deref(),
        ctx.control,
    ) {
        Ok(target) => target,
        // PB53: a structured `status`, not a raw command failure — exit 0
        // unless `--strict` (PB56 promotes it to 3).
        Err(error) if error.error_code() == "KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001" => {
            return Ok(EvidenceScopeBinding {
                reads: reads.clone(),
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
                reads: reads.clone(),
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
    let repo =
        Repository::open_search_in_session(&target.repo_root, ctx.control.clone(), ctx.locks)?
            .with_verified_reads(reads)?;
    // QB5/QB6/裁定1: shared (1)+(3) preflight pair. Evidence verify's whole
    // response IS existence information (08 §4.3 — it never returns body),
    // so checkpoint 2 below (LC54/LC55) gates every return point, not only a
    // single "success" path.
    let checkpoint = preflight_barrier_and_index(&target.kio_dir, ctx.control)?;
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
        reads: reads.clone(),
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

pub(super) fn verify_pointer_in_scope(
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
    let store = repo.object_store();
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
            GcSweepSession::bind_repository(repo)?
                .validate_final_shallow_tree(&pointer.commit, &commit.tree)?;
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
    if let Err(marker_error) =
        enforce_canonical_marker_barrier(target, &pointer.raw_hash, true, repo.control())
    {
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
pub(super) fn canonical_pointer_marker_verification(
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
    match enforce_canonical_marker_barrier(
        target,
        &pointer.raw_hash,
        raw_present,
        &checkpoint.control,
    ) {
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
pub(super) fn canonical_pointer_tree_entry<'a>(
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
pub(super) fn index_rebuilding_error() -> KioError {
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
