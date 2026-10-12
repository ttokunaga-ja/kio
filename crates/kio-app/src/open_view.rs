use crate::commands::PointerArgs;
use crate::historical_reindex::pinned_done_units;
use crate::historical_reindex::retained_history_instances;
use crate::image_authority;
use crate::index::chunk_ledger::check_index_generation_current;
use crate::index::chunk_ledger::chunks_jsonl_path;
use crate::index::chunk_ledger::read_stored_chunks;
use crate::index::chunk_ledger::sqlite_path;
use crate::index::rebuild::store_corrupt_error;
use crate::invocation::Invocation;
use crate::management::open_existing_managed_root;
use crate::object_persistence::index_to_kio;
use crate::object_persistence::pipeline_to_kio;
use crate::object_persistence::search_to_kio;
use crate::platform;
use crate::response::CommandResponse;
use crate::response::Completion;
use crate::response::ResponseBody;
use crate::response::pointer::EvidenceOpenResponse;
use crate::response::pointer::EvidenceViewResponse;
use crate::response::pointer::ObjectChunkResponse;
use crate::response::pointer::ObjectFileResponse;
use crate::response::pointer::OpenResponse;
use crate::response::pointer::RawPointerResponse;
use crate::response::pointer::ViewResponse;
use crate::response::project_legacy;
use crate::scope_access::ReadBarrierCheckpoint;
use crate::scope_access::enforce_canonical_marker_barrier;
use crate::scope_access::enforce_canonical_tombstone_only;
use crate::scope_access::evidence_policy_denied;
use crate::scope_access::invalid_pointer_identity_error;
use crate::scope_access::is_store_not_found;
use crate::scope_access::preflight_barrier_and_index;
use crate::scope_access::purge_not_found_error;
use crate::scope_access::resolve_scope_target;
use crate::scope_access::scope_target;
use crate::scope_access::unresolvable_commit_pointer_error;
use crate::scope_access::unresolvable_shallow_tree_pointer_error;
use crate::search::query::load_searchable_chunks;
use crate::search::query::snapshot_chunking_config_hash;
use crate::search::scope::ScopeTarget;
use crate::search::scope::current_embedding_policy;
use crate::search::scope::current_search_policy;
use crate::settings::cache_home;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::cas::ChunkObject;
use kio_core::cas::ContentObjectKind;
use kio_core::cas::MAX_RAW_OBJECT_BYTES;
use kio_core::cas::fanout_path;
use kio_core::cas::is_hash;
use kio_core::control::ControlHandle;
use kio_core::dag::NormalizeRef;
use kio_core::history::HistoryReader;
use kio_core::portable::portable_cache_leaf;
use kio_core::purge::EventKind;
use kio_core::purge::PurgeState;
use kio_core::purge::TombstoneMode;
use kio_core::purge::canonical_final_event;
use kio_core::scope::DEFAULT_MAX_ARCHIVE_SCOPE_BYTES;
use kio_core::scope::Repository;
use kio_index::fts::ExistingSourceIndexOpenMode;
use kio_index::fts::FtsSchemaConfig;
use kio_index::fts::FtsTokenizer;
use kio_index::fts::open_existing_source_index_connection;
use kio_pipeline::markdownize::build_normalized_view_layout;
use kio_pipeline::markdownize::load_validated_normalized_instance_with_control;
use kio_pipeline::markdownize::normalized_view_path;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::prepare::hash_bytes;
use kio_pipeline::scan::hash_verified_scan_input;
use kio_search::evidence::EVIDENCE_POINTER_SCHEMA_VERSION;
use kio_search::evidence::EvidencePointer;
use kio_search::evidence::EvidencePointerIssueRequest;
use kio_search::evidence::issue_evidence_pointer;
use kio_search::evidence::parse_evidence_pointer_uri;
use rusqlite::Connection;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process;

pub(crate) fn run_open(ctx: &Invocation<'_>, args: PointerArgs) -> Result<Value> {
    let raw = read_pointer_input(ctx, args.pointer.into_iter().collect())?;
    if let Some(object) = parse_object_uri(&raw)? {
        return resolve_object_uri(ctx, &object, false);
    }
    if raw.starts_with("sha256:") {
        return resolve_short_hash_command(ctx, &raw, false);
    }
    let pointer = parse_pointer_text(&raw)?;
    let resolved = resolve_pointer_for_cli(ctx, &pointer)?;
    project_legacy(CommandResponse {
        body: ResponseBody::Open(OpenResponse::Evidence(EvidenceOpenResponse {
            status: "opened".to_owned(),
            path: resolved.path,
            raw_hash: pointer.raw_hash,
            chunk_hash: pointer.chunk_hash,
            temporary: resolved.temporary,
            commit_shallow: resolved.commit_shallow,
            manifest_missing: resolved.manifest_missing,
        })),
        completion: Completion::Complete,
    })
}

pub(crate) fn run_view(ctx: &Invocation<'_>, args: PointerArgs) -> Result<Value> {
    let raw = read_pointer_input(ctx, args.pointer.into_iter().collect())?;
    if let Some(object) = parse_object_uri(&raw)? {
        return resolve_object_uri(ctx, &object, true);
    }
    if raw.starts_with("sha256:") {
        return resolve_short_hash_command(ctx, &raw, true);
    }
    let pointer = parse_pointer_text(&raw)?;
    let resolved = resolve_pointer_for_cli(ctx, &pointer)?;
    project_legacy(CommandResponse {
        body: ResponseBody::View(ViewResponse::Evidence(EvidenceViewResponse {
            status: "viewed".to_owned(),
            raw_hash: pointer.raw_hash,
            chunk_hash: pointer.chunk_hash,
            // 05 §1.7.2 / §4.2 (2026-08-11): `view` returns the full-text
            // normalized view's path plus the chunk's view-local span, not the
            // chunk body — null when the view's backing cache cannot be read.
            view_path: resolved.view_path,
            view_byte_start: resolved.view_byte_start,
            view_byte_end: resolved.view_byte_end,
            path: resolved.path,
            // R11-9: mirror `kio open --json`, which exposes `temporary` from the same
            // resolved pointer — `view` resolves identically and Agents branch on it.
            temporary: Some(resolved.temporary),
            commit_shallow: resolved.commit_shallow,
            manifest_missing: resolved.manifest_missing,
        })),
        completion: Completion::Complete,
    })
}

#[derive(Debug)]
/// Successful resolution of an Evidence Pointer (08 §3). Failure modes
/// (scope_unreachable / tombstoned / not_found) are surfaced as `Err(KioError)`
/// with the exit-4 codes from 08 §3.2 / §4.
struct PointerResolution {
    path: Option<PathBuf>,
    /// 05 §1.7.2 / §4.2 (2026-08-11): the full-text normalized view's path
    /// and the chunk's span translated into that view's byte offsets. `None`
    /// when the backing normalized instance cannot be read (GC'd, purged,
    /// or otherwise unavailable) — a cache-read failure degrades the view
    /// fields to null rather than failing the whole pointer resolution, the
    /// same posture as `commit_shallow`/`manifest_missing` below.
    view_path: Option<PathBuf>,
    view_byte_start: Option<u64>,
    view_byte_end: Option<u64>,
    temporary: bool,
    commit_shallow: bool,
    /// PB48/50 (08 §3.1 procedure 6b): set only on a non-shallow resolution
    /// downgraded because the entry's manifest object was purge-explained.
    /// Mutually exclusive with `commit_shallow` (6b never applies on the
    /// shallow path).
    manifest_missing: bool,
}

/// Reads the raw `<pointer>` operand (08 §2.3), resolving `-` from stdin.
/// Branching into evidence-pointer vs `object` URI happens in the caller.
pub(crate) fn read_pointer_input(ctx: &Invocation<'_>, args: Vec<String>) -> Result<String> {
    const MAX_POINTER_INPUT_BYTES: u64 = 64 * 1024;
    let mut args = args.into_iter();
    let Some(pointer) = args.next() else {
        return Err(KioError::invalid_usage("pointer argument is required"));
    };
    if let Some(extra) = args.next() {
        return Err(KioError::invalid_usage(format!(
            "unexpected pointer argument: {extra}"
        )));
    }
    if pointer == "-" {
        let bytes = ctx
            .interaction
            .read_input(MAX_POINTER_INPUT_BYTES as usize)
            .map_err(|err| KioError::invalid_usage(format!("pointer stdin: {}", err.message())))?;
        let input = String::from_utf8(bytes)
            .map_err(|_| KioError::invalid_usage("pointer stdin must be UTF-8"))?;
        return Ok(input.trim().to_owned());
    }
    if pointer.len() as u64 > MAX_POINTER_INPUT_BYTES {
        return Err(KioError::invalid_usage(
            "pointer argument exceeds the 64 KiB limit",
        ));
    }
    Ok(pointer)
}

/// Parses the non-short `<pointer>` operand forms (08 §2.3): a `kio://` evidence
/// URI or inline JSON. The `sha256:` short form is resolved separately by
/// [`resolve_short_hash_command`] (it may be a raw *or* a chunk hash and needs
/// open/view context), and object URIs are handled before this is reached.
pub(crate) fn parse_pointer_text(pointer: &str) -> Result<EvidencePointer> {
    if pointer.starts_with("kio://") {
        return parse_evidence_pointer_uri(pointer).map_err(search_to_kio);
    }
    if pointer.trim_start().starts_with('{') {
        let pointer: EvidencePointer =
            serde_json::from_str(pointer).map_err(|err| KioError::schema(err.to_string()))?;
        if pointer.schema_version != EVIDENCE_POINTER_SCHEMA_VERSION {
            return Err(KioError::schema("unsupported evidence schema version"));
        }
        return Ok(pointer);
    }
    Err(KioError::invalid_usage("invalid pointer argument"))
}

/// Resolution of a `sha256:` short-form operand (08 §2.3 rule 4).
enum ShortHash {
    /// The short hash is a `chunk_hash` — resolve as a full Evidence Pointer.
    Chunk(Box<EvidencePointer>),
    /// The short hash is a `raw_hash` — resolve the raw object directly. Multiple
    /// chunks sharing this raw_hash (a normal multi-heading file) are the *same*
    /// file, so this is unambiguous per rule 4 ("raw_hash 名前空間の一致は
    /// ファイル単位で一意なら OK").
    Raw {
        target: ScopeTarget,
        raw_hash: String,
        path_hint: Option<String>,
        policy: Box<CurrentPolicyEvaluator>,
        scope_id: String,
        head: String,
    },
}

/// Classify a `sha256:` short hash against the current `.kio` + HEAD (08 §2.3
/// rule 4). Ambiguity is only *across kinds* (the hash is simultaneously a
/// chunk_hash and a raw_hash) — never among the several chunks of one file.
/// O6: a `sha256:` short-hash operand must carry a lowercase-hex digest of at
/// least 4 chars before it can reach `cas_object_path`'s `digest[0..2]`/`[2..4]`
/// slices (which panic out of range on `sha256:a`) or any lookup. A malformed
/// operand is a usage error (KIO-E-CONFIG-USAGE-001, exit 2), never a crash.
fn validate_short_hash_operand(hash: &str) -> Result<()> {
    let digest = hash.strip_prefix("sha256:").unwrap_or(hash);
    if digest.len() < 4
        || !digest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(KioError::invalid_usage(
            "short hash must be `sha256:` followed by at least 4 lowercase hex characters",
        ));
    }
    Ok(())
}

fn classify_short_hash(ctx: &Invocation<'_>, hash: &str) -> Result<ShortHash> {
    validate_short_hash_operand(hash)?;
    let initial_repo = ctx.open_recovery_repository()?;
    let target = scope_target(initial_repo.root(), ctx.control)?;
    let (repo, policy) = current_search_policy(&target, ctx.control)?;
    let observed_head = repo
        .head_commit_hash()?
        .ok_or_else(|| KioError::not_found("HEAD"))?;
    // The mutable chunk ledger is an acceleration structure only. Filter it
    // through current path authority before it can participate in ambiguity or
    // pointer metadata, so an ignored chunk cannot disclose its identity.
    let mut chunks = Vec::new();
    for chunk in load_searchable_chunks(&target, ctx.control, ctx.locks)? {
        if policy
            .allows_path(&chunk.path_at_commit)
            .map_err(pipeline_to_kio)?
        {
            chunks.push(chunk);
        }
    }

    let chunk_match = chunks.iter().find(|chunk| chunk.row.chunk_id == hash);
    let raw_path_hint = chunks
        .iter()
        .find(|chunk| chunk.row.raw_hash == hash)
        .map(|chunk| chunk.path_at_commit.clone())
        .or(current_policy_raw_path_for_short_hash(
            &repo, &policy, hash,
        )?);
    // A raw hash without a current-policy-eligible authenticated tree owner is
    // not a short-hash candidate, even if a stale CAS or tombstone remains.
    let is_raw = raw_path_hint.is_some()
        && (raw_object_present(&target, hash)?
            || read_tombstone(&target, hash, ctx.control)?.is_some());
    let is_chunk = chunk_match.is_some();

    match (is_chunk, is_raw) {
        (true, true) => Err(KioError::new(
            "KIO-E-EVIDENCE-SCOPE-AMBIGUOUS-001",
            "short hash matches both a chunk_hash and a raw_hash; disambiguate with a full pointer",
            json!({ "hash": hash, "kinds": ["chunk", "raw"] }),
            ExitCode::InvalidUsage,
        )),
        (true, false) => {
            let chunk = chunk_match.expect("chunk_match is Some");
            // LC8-LC14/item 3: no barrier check here (this used to run a
            // single-marker `enforce_purge_read_barrier` pre-check, which can
            // disagree with the cross-marker canonical final event — LC10's
            // worked example — and wrongly reject a hash canonical dispatch
            // would allow). `resolve_short_hash_command`'s `ShortHash::Chunk`
            // arm immediately calls `resolve_pointer_for_cli`, whose own
            // `enforce_canonical_marker_barrier` checks are the authoritative,
            // complete gate for this raw_hash.
            let pointer = issue_evidence_pointer(EvidencePointerIssueRequest {
                scope_id: chunk.scope_id.clone(),
                scope_path: Some(chunk.scope_path.display().to_string()),
                commit: chunk.snapshot_at.clone(),
                tree: None,
                raw_hash: chunk.row.raw_hash.clone(),
                tool_profile_hash: chunk.row.tool_profile_hash.clone(),
                chunk_hash: chunk.row.chunk_id.clone(),
                path_at_commit: Some(chunk.path_at_commit.clone()),
                heading_path: chunk.row.heading_path.clone(),
                section_id: chunk.row.section_id.clone(),
                byte_start: Some(chunk.row.byte_start),
                byte_end: Some(chunk.row.byte_end),
            })
            .map_err(search_to_kio)?;
            Ok(ShortHash::Chunk(Box::new(pointer)))
        }
        (false, true) => {
            // Same rationale as the chunk arm above: `resolve_short_hash_command`'s
            // `ShortHash::Raw` arm runs the authoritative canonical check
            // itself, once it has resolved the raw object's actual presence.
            Ok(ShortHash::Raw {
                target,
                raw_hash: hash.to_owned(),
                path_hint: raw_path_hint,
                policy: Box::new(policy),
                scope_id: repo.scope_identity()?.scope_id,
                head: observed_head,
            })
        }
        (false, false) => Err(KioError::invalid_usage(
            "short hash is not found in the current scope",
        )),
    }
}

fn current_policy_raw_path_for_short_hash(
    repo: &Repository,
    policy: &CurrentPolicyEvaluator,
    raw_hash: &str,
) -> Result<Option<String>> {
    let Some(head) = repo.head_commit_hash()? else {
        return Ok(None);
    };
    let commit = repo.read_commit(&head)?;
    let tree = repo.read_tree(&commit.tree)?;
    let mut allowed = Vec::new();
    for entry in tree
        .entries
        .iter()
        .filter(|entry| entry.raw_hash == raw_hash)
    {
        if policy.allows_path(&entry.path).map_err(pipeline_to_kio)? {
            allowed.push(entry.path.clone());
        }
    }
    allowed.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(allowed.into_iter().next())
}

/// True when a raw object with `raw_hash` is present in the working tree or the
/// CAS raw store (08 §2.3 rule 4 raw resolution). Used only for the raw-only
/// short-hash case (no chunk carries the raw_hash).
fn raw_object_present(target: &ScopeTarget, raw_hash: &str) -> Result<bool> {
    if cas_object_present(&target.kio_dir, "raw", raw_hash, MAX_RAW_OBJECT_BYTES)? {
        return Ok(true);
    }
    Ok(find_working_tree_raw(&target.repo_root, raw_hash)?.is_some())
}

/// Resolve a `sha256:` short-form operand for `kio open` / `kio view`
/// (08 §2.3 rule 4). Handles both the chunk_hash and raw_hash kinds.
fn resolve_short_hash_command(ctx: &Invocation<'_>, hash: &str, as_view: bool) -> Result<Value> {
    match classify_short_hash(ctx, hash)? {
        ShortHash::Chunk(pointer) => {
            let resolved = resolve_pointer_for_cli(ctx, &pointer)?;
            if as_view {
                project_legacy(CommandResponse {
                    body: ResponseBody::View(ViewResponse::Evidence(EvidenceViewResponse {
                        status: "viewed".to_owned(),
                        raw_hash: pointer.raw_hash,
                        chunk_hash: pointer.chunk_hash,
                        // 05 §1.7.2 / §4.2 (2026-08-11): see `run_view`'s matching fields.
                        view_path: resolved.view_path,
                        view_byte_start: resolved.view_byte_start,
                        view_byte_end: resolved.view_byte_end,
                        path: resolved.path,
                        temporary: None,
                        commit_shallow: resolved.commit_shallow,
                        manifest_missing: resolved.manifest_missing,
                    })),
                    completion: Completion::Complete,
                })
            } else {
                project_legacy(CommandResponse {
                    body: ResponseBody::Open(OpenResponse::Evidence(EvidenceOpenResponse {
                        status: "opened".to_owned(),
                        path: resolved.path,
                        raw_hash: pointer.raw_hash,
                        chunk_hash: pointer.chunk_hash,
                        temporary: resolved.temporary,
                        commit_shallow: resolved.commit_shallow,
                        manifest_missing: resolved.manifest_missing,
                    })),
                    completion: Completion::Complete,
                })
            }
        }
        ShortHash::Raw {
            target,
            raw_hash,
            path_hint,
            policy,
            scope_id,
            head,
        } => {
            // §I checkpoint 1 (LC53).
            let checkpoint = ReadBarrierCheckpoint::open(&target.kio_dir, ctx.control)?;
            // LC45 (item 2).
            check_index_generation_current(&target.kio_dir, ctx.control)?;
            match open_authorized_raw_object(&target, &raw_hash, path_hint.as_deref(), ctx.control)?
            {
                // A raw object has no chunk text; open/view surface only its path.
                Some((path, temporary)) => {
                    // LC8-LC14 canonical dispatch (item 3) + §I checkpoint 2
                    // (LC54/LC55), combined: either failure discards the temp
                    // open-cache this may have just published (LC57's cache
                    // publish-then-final-check ordering).
                    if let Err(error) = enforce_canonical_marker_barrier(
                        &target,
                        &raw_hash,
                        true,
                        ctx.control,
                    )
                    .and_then(|()| policy.revalidate().map_err(pipeline_to_kio))
                    .and_then(|()| {
                        let repo = open_existing_managed_root(&target.repo_root, ctx.control)?
                            .in_lock_session(ctx.locks);
                        if repo.scope_identity()?.scope_id == scope_id
                            && repo.head_commit_hash()?.as_deref() == Some(head.as_str())
                        {
                            Ok(())
                        } else {
                            Err(KioError::new(
                                "KIO-E-EVIDENCE-POLICY-001",
                                "short-hash authority changed while it was being opened; retry",
                                json!({}),
                                ExitCode::PartialFailure,
                            ))
                        }
                    })
                    .and_then(|()| checkpoint.recheck())
                    {
                        if temporary {
                            let _ = fs::remove_file(&path);
                        }
                        return Err(error);
                    }
                    let status = if as_view { "viewed" } else { "opened" };
                    let payload = RawPointerResponse {
                        status: status.to_owned(),
                        object_type: "raw".to_owned(),
                        raw_hash,
                        path,
                        temporary,
                    };
                    let body = if as_view {
                        ResponseBody::View(ViewResponse::Raw(payload))
                    } else {
                        ResponseBody::Open(OpenResponse::Raw(payload))
                    };
                    project_legacy(CommandResponse {
                        body,
                        completion: Completion::Complete,
                    })
                }
                // raw_present=false always yields Err from the canonical
                // dispatch (LC12/13/14 jointly cover every marker state), so
                // this recovers the precise code (not-found / store-corrupt)
                // in place of the old unconditional purge-not-found guess.
                None => {
                    Err(
                        enforce_canonical_marker_barrier(&target, &raw_hash, false, ctx.control)
                            .err()
                            .unwrap_or_else(|| purge_not_found_error(&target, &raw_hash)),
                    )
                }
            }
        }
    }
}

// ===========================================================================
// 08 §3.1 procedures 6a/6b (step4b-contract-tests-p2b.md PB39-50/55; item 2 of
// this session's task). Shared by `resolve_pointer_for_cli` (open/view/
// restore) and `verify_pointer_for_cli` (`crates/kio-cli/src/verify_objects.rs`,
// PB66's structural requirement — one implementation, not two independently
// drifting judgments). Never invoked on the shallow path (2a): tree/entry are
// unavailable there by construction, and procedure 2a explicitly excludes
// 3-4/6/6a/6b.
// ===========================================================================

pub(crate) const MAX_MANIFEST_OBJECT_READ_BYTES: u64 = 8 * 1024 * 1024;

/// The 08 §3.1 6a/6b verdict for one resolved `(tree entry, chunk)` pair at
/// `pointer_commit`.
pub(crate) enum PointInTimeAttribution {
    /// v2/v3 tree, procedure 6a's unit-status and publication/association
    /// ancestry checks all passed.
    Alive,
    /// 6a's manifest-done check, or the v2/v3 publication/association
    /// ancestry check, failed outright — not admissible evidence for this
    /// commit (`not_found`).
    NotFound,
    /// 6b applied (the manifest object is purge-explained and in scope,
    /// possibly via the resurrection link) — direct-resolution downgrade;
    /// caller still must run procedure 8's entry-level checks.
    ManifestMissing,
    /// 6b's explanation is out of the fsck-equivalent scope (pointer_commit
    /// is not ancestor-or-equal of the explaining event's `in_commit`), or no
    /// marker explains the missing manifest at all — genuine corruption.
    StoreCorrupt,
    /// sqlite.db is unavailable for the v2/v3 association check — a
    /// command-level retryable condition (08 §3.1 step 6a), not a resolution
    /// verdict.
    IndexRebuilding,
}

/// Procedure 6a entry point: `normalize` is the tool-bound tree entry's own
/// `normalize` ref (procedure 4's selection), already established by the
/// caller. `chunk` is the already-resolved, identity-checked chunk object;
/// `chunk_hash` is its CAS key (the object itself carries no self-hash —
/// 03 §8.1).
pub(crate) fn verify_point_in_time_attribution(
    target: &ScopeTarget,
    repo: &Repository,
    pointer: &EvidencePointer,
    normalize: &NormalizeRef,
    chunk: &ChunkObject,
    association_connection: Option<&Connection>,
) -> Result<PointInTimeAttribution> {
    let manifest_hash = &normalize.manifest_hash;
    match repo
        .object_store()
        .inspect_content_accounted(ContentObjectKind::Manifest, manifest_hash)
        .map_err(|error| error.error)
    {
        Ok(_) => {}
        Err(error) if is_store_not_found(&error) => {
            return resolve_manifest_missing(
                target,
                repo,
                &pointer.raw_hash,
                &pointer.chunk_hash,
                &pointer.commit,
                association_connection,
            );
        }
        Err(error) => return Err(error),
    }
    // The tree's manifest hash is the point-in-time authority. Reuse the
    // historical-reindex loader rather than inspecting only a manifest entry:
    // it verifies the manifest CAS digest, canonical/schema/identity tuple,
    // and every Done entry's immutable normalized-unit CAS object. A mutable
    // normalized-unit cache therefore cannot make this pointer appear Alive.
    let done_units = pinned_done_units(repo, &pointer.raw_hash, normalize)?;
    if !done_units.iter().any(|unit| {
        unit.unit_key == chunk.unit_key
            && hash_bytes(unit.markdown.as_bytes()) == chunk.unit_content_hash
    }) {
        return Ok(PointInTimeAttribution::NotFound);
    }
    match check_publication_and_association_for_verifier(
        target,
        repo,
        association_connection,
        &pointer.chunk_hash,
        &pointer.commit,
    )? {
        AssociationCheck::Ok => Ok(PointInTimeAttribution::Alive),
        AssociationCheck::NotFound => Ok(PointInTimeAttribution::NotFound),
        AssociationCheck::IndexRebuilding => Ok(PointInTimeAttribution::IndexRebuilding),
    }
}

/// Procedure 6b: the manifest object itself is gone (purge deleted it — the
/// tree entry still names it, tree/commit objects are never rewritten). Scope
/// the explanation to the fsck-equivalent boundary (10 §7.5.1 / PB05), then
/// downgrade to direct resolution (or resolve via the resurrection link).
fn resolve_manifest_missing(
    target: &ScopeTarget,
    repo: &Repository,
    raw_hash: &str,
    chunk_hash: &str,
    pointer_commit: &str,
    association_connection: Option<&Connection>,
) -> Result<PointInTimeAttribution> {
    let purge = PurgeState::open_with_control(&target.kio_dir, repo.control().clone())?;
    let tombstone = purge.read_tombstone(raw_hash)?;
    let receipt = purge.read_erase_receipt(raw_hash)?;
    let Some(canonical) = canonical_final_event(
        tombstone.as_ref().map(|record| record.tail()),
        receipt.as_ref().map(|record| record.tail()),
    )?
    else {
        // No marker at all explains a missing manifest -- corruption.
        return Ok(PointInTimeAttribution::StoreCorrupt);
    };
    // The canonical final event's OWN `in_commit` is not usable when it is
    // `retired`: `LifecycleEvent::retired` stamps `in_commit` with the
    // resurrection commit, not the original purge's (05 §3.5's retired-event
    // shape) -- walk back to the marker's own last purged/erased event
    // instead, whichever marker canonical says is authoritative.
    let explaining_in_commit = match canonical.marker_kind {
        TombstoneMode::Default => tombstone.as_ref().map(|record| &record.events),
        TombstoneMode::Erase => receipt.as_ref().map(|record| &record.events),
    }
    .and_then(|events| {
        events
            .iter()
            .rev()
            .find(|event| matches!(event.kind, EventKind::Purged | EventKind::Erased))
            .map(|event| event.in_commit.clone())
    });
    let Some(in_commit) = explaining_in_commit else {
        return Ok(PointInTimeAttribution::StoreCorrupt);
    };
    // fsck-equivalent scope (PB05 / 08 §3.1 step 6b): pointer_commit must be
    // at-or-before the explaining purge (ancestor-or-equal of `in_commit`).
    // Outside that range = a newer, unexplained manifest loss.
    if !is_ancestor_or_equal(repo, pointer_commit, &in_commit)? {
        return Ok(PointInTimeAttribution::StoreCorrupt);
    }

    // Direct resolution: pointer_commit is still the ancestry basis for 6a's
    // publication/association checks.
    match check_publication_and_association_for_verifier(
        target,
        repo,
        association_connection,
        chunk_hash,
        pointer_commit,
    )? {
        AssociationCheck::Ok => return Ok(PointInTimeAttribution::ManifestMissing),
        AssociationCheck::IndexRebuilding => return Ok(PointInTimeAttribution::IndexRebuilding),
        AssociationCheck::NotFound => {}
    }

    // Resurrection link: valid only when canonical final event (procedure 5)
    // is itself `retired` (not a stale non-canonical marker's own tail) --
    // re-run the same checks with `resurrection_commit` as the basis.
    if canonical.event.kind == EventKind::Retired
        && let Some(link_commit) = &canonical.event.resurrection_commit
    {
        match check_publication_and_association_for_verifier(
            target,
            repo,
            association_connection,
            chunk_hash,
            link_commit,
        )? {
            AssociationCheck::Ok => return Ok(PointInTimeAttribution::ManifestMissing),
            AssociationCheck::IndexRebuilding => {
                return Ok(PointInTimeAttribution::IndexRebuilding);
            }
            AssociationCheck::NotFound => {}
        }
    }
    Ok(PointInTimeAttribution::NotFound)
}

pub(crate) enum AssociationCheck {
    Ok,
    NotFound,
    IndexRebuilding,
}

/// 08 §3.1 step 6a's publication ancestor-or-equal check against
/// `basis_commit` (the pointer's own commit for a direct resolution, or the
/// resurrection link's commit for 6b's link path). The config association is
/// creation/order metadata; only the publication relation carries history.
fn check_publication_and_association(
    target: &ScopeTarget,
    repo: &Repository,
    chunk_hash: &str,
    basis_commit: &str,
) -> Result<AssociationCheck> {
    let db_path = sqlite_path(&target.kio_dir);
    if !db_path.exists() {
        return Ok(AssociationCheck::IndexRebuilding);
    }
    let conn = open_existing_source_index_connection(
        &db_path,
        ExistingSourceIndexOpenMode::ReadOnly,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;

    check_publication_and_association_on_connection(&conn, repo, chunk_hash, basis_commit)
}

/// Evidence verification supplies the source-index descriptor retained by its
/// scope binding. Other pointer consumers keep their established lazy opener.
/// Both routes execute the same authoritative association core.
fn check_publication_and_association_for_verifier(
    target: &ScopeTarget,
    repo: &Repository,
    connection: Option<&Connection>,
    chunk_hash: &str,
    basis_commit: &str,
) -> Result<AssociationCheck> {
    match connection {
        Some(connection) => check_publication_and_association_on_connection(
            connection,
            repo,
            chunk_hash,
            basis_commit,
        ),
        None => check_publication_and_association(target, repo, chunk_hash, basis_commit),
    }
}

/// The authoritative publication/config-association judgment.  Retarget keeps
/// the source-index descriptor it bound at command start and calls this core
/// directly; ordinary pointer resolution uses the opener wrapper above.
pub(crate) fn check_publication_and_association_on_connection(
    conn: &Connection,
    repo: &Repository,
    chunk_hash: &str,
    basis_commit: &str,
) -> Result<AssociationCheck> {
    let resolved_config = snapshot_chunking_config_hash(repo, basis_commit)?;
    let introductions =
        kio_index::fts::chunk_publication_introductions(conn, chunk_hash, &resolved_config)
            .map_err(index_to_kio)?;
    let mut publication_ok = false;
    for introduction in &introductions {
        if is_ancestor_or_equal(repo, introduction, basis_commit)? {
            publication_ok = true;
            break;
        }
    }
    if !publication_ok {
        return Ok(AssociationCheck::NotFound);
    }
    Ok(AssociationCheck::Ok)
}

/// Bounded "is `ancestor` at or before `descendant`" check via a commit-only
/// walk (parents only, no tree reads — cheaper than
/// `kio_core::history::LinearHistory` for this yes/no query). Bounded the same
/// as the dedicated history walks (`DEFAULT_MAX_HISTORY_COMMITS`) to avoid an
/// unbounded walk on a pathological DAG; a shallow-skipped ancestor (missing
/// commit object) simply prunes that branch rather than erroring, matching
/// the tolerant-walk convention used elsewhere for history traversal.
pub(crate) fn is_ancestor_or_equal(
    repo: &Repository,
    ancestor: &str,
    descendant: &str,
) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    let mut pending = vec![descendant.to_owned()];
    let mut visited = BTreeSet::new();
    let mut steps: u64 = 0;
    while let Some(hash) = pending.pop() {
        if !visited.insert(hash.clone()) {
            continue;
        }
        steps += 1;
        if steps > kio_core::history::DEFAULT_MAX_HISTORY_COMMITS {
            return Err(KioError::new(
                "KIO-E-COMMIT-HISTORY-LIMIT-001",
                "ancestor-or-equal check exceeded the history walk bound",
                json!({ "ancestor": ancestor, "descendant": descendant }),
                ExitCode::PartialFailure,
            ));
        }
        let commit = match repo.read_commit(&hash) {
            Ok(commit) => commit,
            Err(error) if is_store_not_found(&error) => continue,
            Err(error) => return Err(error),
        };
        if let Some(parent) = &commit.parent {
            if parent == ancestor {
                return Ok(true);
            }
            pending.push(parent.clone());
        }
    }
    Ok(false)
}

/// PB48/6b's StoreCorrupt verdict, open/view/restore side: a manifest gap
/// that no marker's explanation scope covers (or that no marker explains at
/// all) is genuine store corruption, the same terminal code procedure 4's
/// zero-matching-entry short circuit uses (08 §3.1 step 4/6b both fold into
/// `KIO-E-STORE-CORRUPT-001`, not_found-equivalent handling).
fn point_in_time_store_corrupt_error(pointer: &EvidencePointer) -> KioError {
    KioError::new(
        "KIO-E-STORE-CORRUPT-001",
        "the entry's manifest object is missing and no purge/erase marker explains \
         its absence within scope; run kio repair verify-objects",
        json!({
            "commit": pointer.commit,
            "raw_hash": pointer.raw_hash,
        }),
        ExitCode::PermanentFailure,
    )
}

/// 08 §3.1 step 6a's sqlite.db-unavailable carve-out: verification could not
/// run at all, so this is not a resolution verdict (not_found or otherwise) —
/// a command-level retryable condition, matching PB57's `evidence verify`
/// contract extended to the open/view/restore side.
fn point_in_time_index_rebuilding_error() -> KioError {
    KioError::new(
        "KIO-E-INDEX-REBUILDING-001",
        "the search index is unavailable (not yet built or mid-rebuild); retry",
        json!({}),
        ExitCode::PartialFailure,
    )
}

/// 05 §1.7.2 / §4.2 (2026-08-11): translate a resolved chunk's unit-local
/// `byte_start`/`byte_end` (03 §8.1) into a byte span within the full-text
/// path-named current normalized view. Returns `None` when the chunk's unit
/// content is not part of that current view, or when the cache bytes do not
/// match the authenticated current manifest. Identical Markdown from a
/// same-generation re-ingest is deliberately compatible.
fn resolve_chunk_view(
    kio_dir: &Path,
    chunk: &ChunkObject,
    control: &ControlHandle,
) -> Option<(PathBuf, u64, u64)> {
    let instance = load_validated_normalized_instance_with_control(
        kio_dir,
        &chunk.raw_hash,
        &chunk.tool_profile_hash,
        chunk.r#gen,
        control,
    )
    .ok()?;
    let unit = instance.units.iter().find(|unit| {
        unit.unit_key == chunk.unit_key
            && hash_bytes(unit.markdown.as_bytes()) == chunk.unit_content_hash
    })?;
    let layout = build_normalized_view_layout(&instance.manifest, &instance.units);
    let unit_start = *layout.unit_starts.get(&unit.unit_key)? as u64;
    let unit_len = *layout.unit_lens.get(&unit.unit_key)? as u64;
    // The chunk's byte_end can run past the trimmed unit length: chunk spans
    // are cut from the untrimmed `unit.markdown` (03 §8.1) and can include the
    // trailing newlines the view assembly trims off (03 §2.1 rule 2) — clamp
    // so the view span never reaches past the unit's actual content in `text`.
    // Clamp BOTH ends, not just `byte_end`: a chunk indexed before the
    // content-free-chunk guard can start inside the trailing newline run the
    // view assembly trims away, and an unclamped `byte_start` would then
    // report `start > end` — an out-of-order span a client would slice with.
    let view_start = unit_start + chunk.byte_start.min(unit_len);
    let view_end = unit_start + chunk.byte_end.min(unit_len);
    let view_path = normalized_view_path(
        kio_dir,
        &chunk.raw_hash,
        &chunk.tool_profile_hash,
        chunk.r#gen,
    );
    if fs::read(&view_path).ok()?.as_slice() != layout.text.as_bytes() {
        return None;
    }
    Some((view_path, view_start, view_end))
}

fn resolve_pointer_for_cli(
    ctx: &Invocation<'_>,
    pointer: &EvidencePointer,
) -> Result<PointerResolution> {
    // 08 §3.1 step 1: two-stage scope resolution (scope_path hint -> registry).
    let target = resolve_scope_target(
        ctx.cwd(),
        ctx.interaction.as_ref(),
        &pointer.scope_id,
        pointer.scope_path.as_deref(),
        ctx.control,
    )?;
    // QB6 (step4b-contract-tests-p3b.md §A, 10 §3 L300-305): (0)
    // kio_format_version compatibility must be checked BEFORE (1) the purge
    // read barrier — this used to open the checkpoint first, so a scope that
    // was both format-incompatible and mid-purge-journal surfaced the lower-
    // priority `KIO-E-PURGE-JOURNAL-ACTIVE-001` instead of
    // `KIO-E-STORE-VERSION-001`.
    let repo = Repository::open_in_session(&target.repo_root, ctx.control.clone(), ctx.locks)?;
    // QB5/QB6/裁定1: shared (1)+(3) preflight pair, opened as soon as the
    // target scope is known — brackets every canonical-marker dispatch below
    // (LC8-14/item 3), so those calls no longer need their own
    // `PurgeState::barrier_blocks` in-flight-journal check: an active journal
    // targeting this raw_hash implies an active journal, which this
    // checkpoint (and its checkpoint-2 recheck at the bottom of this
    // function) already catches more broadly, ABA-safe via the
    // purge-epoch/lifecycle-epoch comparison (LC54).
    let checkpoint = preflight_barrier_and_index(&target.kio_dir, ctx.control)?;

    // 08 §3.1 step 2: fetch the commit object. R17-1: a MISSING commit object
    // (never existed / externally deleted — e.g. a `view`/`open` pointer whose
    // `commit` field is a forged hash) is an Evidence-resolution FAILURE, NOT a
    // shallow commit. docs/08 §3.2:150 makes commit-object *existence* the resolution
    // precondition ("shallow でもよい" tolerates tree GC precisely *because the commit
    // still exists*); docs/05 §3.6's "resolution never fails on a shallow commit"
    // guarantee is therefore scoped to a genuine shallow commit (commit present, tree
    // GC'd) and does NOT extend to a missing commit object. R16-1 best-effort'd this
    // read for status/log/search and that stays — those are pure reads with no
    // authenticity gate to bypass — but resolve_pointer_for_cli is the ONE Evidence-
    // authenticity entry point. Folding a missing commit into the shallow path sets
    // entry_gen=None, which skips BOTH the tree-membership check (raw_hash ∈
    // commit.tree) and the N5 gen binding below — letting a forged `commit` splice a
    // newer-generation chunk (post `reindex --force`) under a commit that never
    // normalized it. So a missing commit is rejected here, not resolved best-effort.
    let commit = match repo.read_commit(&pointer.commit) {
        Ok(commit) => commit,
        Err(error) if is_store_not_found(&error) => {
            return Err(unresolvable_commit_pointer_error(pointer));
        }
        Err(error) => return Err(error),
    };

    // 08 §3.1 step 2a/3-4: a genuine shallow commit proves only that its tree
    // was discarded. It cannot bind a pointer raw hash, normalization generation,
    // or manifest to that historical snapshot, so preserve the canonical raw/purge
    // dispatch below and then reject it before touching a chunk or mutable index.
    // Otherwise the raw_hash must appear in commit.tree for the pointer to resolve
    // against *this* snapshot (not the working tree of some later commit).
    // A missing commit object never reaches here — R17-1 rejected it above.
    // `entry_gen` is the tree entry's normalization generation on a non-shallow
    // commit; `None` on the shallow path (no tree to read). It binds the chunk's
    // gen below (N5).
    let (commit_shallow, entry_gen, mut entry_normalize, mut entry_path, entry_candidates) =
        match repo.read_tree(&commit.tree) {
            Ok(tree) => {
                let raw_matches = tree
                    .entries
                    .iter()
                    .filter(|entry| entry.raw_hash == pointer.raw_hash)
                    .collect::<Vec<_>>();
                if raw_matches.is_empty() {
                    // step 5 (tombstone) is checked before declaring not_found.
                    // This is a tree-membership failure (this pointer's commit
                    // never referenced this raw_hash), independent of LC12/13/14's
                    // raw-CAS-presence branches — the raw itself may be perfectly
                    // present elsewhere. Only escalate to the tombstone response
                    // when canonical dispatch actually says `purged` (LC11);
                    // otherwise this stays the plain not-found it always was.
                    enforce_canonical_tombstone_only(&target, &pointer.raw_hash, ctx.control)?;
                    return Err(purge_not_found_error(&target, &pointer.raw_hash));
                }
                let entry_candidates = raw_matches
                    .iter()
                    .map(|entry| (*entry).clone())
                    .collect::<Vec<_>>();
                // M6/PB42/43 (§O, 08 §3.1 step 4): when the same raw_hash is
                // placed at more than one path in this commit's tree (duplicate
                // placement), select the entry whose normalize.tool_profile_hash
                // binds to the pointer's, tie-broken by UTF-8 byte-order-minimal
                // path when more than one entry shares that binding — mirrors
                // `verify_pointer_for_cli`'s (`verify_objects.rs`) selection so
                // open/view/restore and evidence verify agree on which entry
                // wins. A lone raw-only tree entry can still be selected here
                // for the structural raw-hash check, but it is rejected below
                // before semantic chunk resolution: without `normalize`, this
                // snapshot has no profile/gen/manifest evidence attribution.
                let raw_matches = match pointer.path_at_commit.as_deref() {
                    Some(path) => raw_matches
                        .into_iter()
                        .filter(|entry| entry.path == path)
                        .collect::<Vec<_>>(),
                    None => raw_matches,
                };
                let entry = if let [single] = raw_matches.as_slice() {
                    if let Some(normalize) = &single.normalize
                        && normalize.tool_profile_hash != pointer.tool_profile_hash
                    {
                        return Err(invalid_pointer_identity_error(pointer));
                    }
                    *single
                } else {
                    raw_matches
                        .into_iter()
                        .filter(|entry| {
                            entry.normalize.as_ref().is_some_and(|normalize| {
                                normalize.tool_profile_hash == pointer.tool_profile_hash
                            })
                        })
                        .min_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()))
                        .ok_or_else(|| invalid_pointer_identity_error(pointer))?
                };
                // N5: the tree entry's normalization must ALSO bind `gen`
                // (checked below against the resolved chunk), so a pointer that
                // keeps an old commit but swaps in a newer-generation chunk_hash
                // produced by `reindex --force` cannot resolve (the gen axis M6
                // missed, 08 §3). `entry_gen` stays `None` for a no-normalize
                // entry — the chunk (raw, tool) identity check below is the
                // available guard there.
                let entry_gen = entry.normalize.as_ref().map(|normalize| normalize.r#gen);
                (
                    false,
                    entry_gen,
                    entry.normalize.clone(),
                    Some(entry.path.clone()),
                    entry_candidates,
                )
            }
            // Tree object gone (genuine shallow: commit present, tree GC'd) — resolve the
            // chunk directly, with gen unbound.
            Err(error) if is_store_not_found(&error) => {
                kio_core::gc::validate_final_shallow_tree_with_control(
                    &target.kio_dir,
                    &pointer.commit,
                    &commit.tree,
                    repo.control(),
                )?;
                (true, None, None, None, Vec::new())
            }
            Err(error) => return Err(error),
        };

    // 08 §3.1 step 5-6a: canonical final event dispatch (LC8-14/item 3),
    // fed the actual raw-CAS-or-working-tree presence answer up front so it
    // can distinguish LC12's expected erased-and-gone from LC13's
    // retired-but-corrupt and LC14's unmarked corruption — the three used to
    // collapse into the same generic `purge_not_found_error` under the old
    // two-step "barrier check, then separately check raw presence" shape.
    // Resolved before chunk/profile availability so an old pointer whose raw
    // and derivatives were deleted reports the purge/corruption state rather
    // than the unrelated retarget-required profile error.
    let raw_present = raw_object_present(&target, &pointer.raw_hash)?;
    enforce_canonical_marker_barrier(&target, &pointer.raw_hash, raw_present, ctx.control)?;
    if !raw_present {
        // Every canonical state with raw_present=false already returned Err
        // above (LC12/13/14 jointly cover it); defensive fallback only.
        return Err(purge_not_found_error(&target, &pointer.raw_hash));
    }

    if commit_shallow {
        return Err(unresolvable_shallow_tree_pointer_error(pointer));
    }
    // A raw-only snapshot has no same-snapshot normalization generation or
    // pinned manifest. It cannot authenticate a semantic Evidence Pointer,
    // even when an older mutable chunk row shares the raw hash. Raw short-hash
    // access is resolved on its separate current-HEAD path and remains able to
    // open this file without exposing chunk evidence from the bare snapshot.
    if entry_normalize.is_none() {
        return Err(invalid_pointer_identity_error(pointer));
    }

    // 08 §3.1 step 6-7: chunk_hash -> durable semantic chunk CAS. SQLite and
    // chunks.jsonl are acceleration/rebuild inputs and cannot independently make
    // a pointer alive (Step 4 decision #69/#75).
    // A pointer whose chunk_hash has NO materialized chunk object in this scope
    // cannot be served under this tool_profile_hash — that is 08 §3.2's
    // "tool_profile_hash 不一致: chunk が存在しない場合は retarget が必要 (§5)",
    // exit 8 (06 §7). Applies on the shallow path too: chunk rows outlive tree
    // discard, so their absence means the profile mismatch, not GC.
    let chunk = match repo.object_store().read_chunk(&pointer.chunk_hash) {
        Ok(chunk) => chunk,
        Err(error) if is_store_not_found(&error) => {
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
    // M6: the resolved chunk row must bind to the pointer's (raw_hash,
    // tool_profile_hash). A chunk_hash that materializes under a *different* raw
    // or tool identity than the pointer claims is a tampered pointer — reject it
    // rather than serve inconsistent evidence (body from A, raw from B).
    if chunk.raw_hash != pointer.raw_hash || chunk.tool_profile_hash != pointer.tool_profile_hash {
        return Err(invalid_pointer_identity_error(pointer));
    }
    // N5: on a non-shallow commit, the chunk's generation must equal the tree
    // entry's normalize.gen. Otherwise a pointer to an old commit could resolve a
    // chunk_hash from a *newer* generation (post `reindex --force`), serving body
    // from gen N+1 under a commit that only ever normalized gen N. The shallow
    // path has no tree entry, so gen stays unbound there (chunk (raw, tool)
    // identity is the only available check).
    if pointer.path_at_commit.is_some()
        && let Some(entry_gen) = entry_gen
        && chunk.r#gen != entry_gen
    {
        return Err(invalid_pointer_identity_error(pointer));
    }

    // Current access policy is evaluated only after the pointer's immutable
    // commit/tree/profile/gen attribution has succeeded. A supplied
    // `path_at_commit` is an authority claim, not a cache hint: it must be the
    // exact authenticated tree path, and another alias may not authorize it.
    if pointer
        .path_at_commit
        .as_deref()
        .is_some_and(|path| entry_path.as_deref() != Some(path))
    {
        return Err(invalid_pointer_identity_error(pointer));
    }
    let (policy_repo, current_policy) = current_search_policy(&target, ctx.control)?;
    let policy_head = policy_repo
        .head_commit_hash()?
        .ok_or_else(|| KioError::not_found("HEAD"))?;
    // A copied/dangling commit object is not evidence authority. The pointer's
    // already-authenticated commit must also remain reachable from the live
    // managed HEAD; a receipt-backed shallow ancestor is a boundary rather
    // than a reason to borrow authority from a detached CAS object.
    let (live_history, _) =
        HistoryReader::new_with_store(policy_repo.kio_dir(), policy_repo.object_store())
            .walk_allow_shallowed(&policy_head)?;
    if live_history.node(&pointer.commit).is_none() {
        return Err(KioError::new(
            "KIO-E-EVIDENCE-POLICY-001",
            "evidence commit is not retained by the current managed history",
            json!({}),
            ExitCode::PermanentFailure,
        ));
    }
    // URI pointers omit `path_at_commit`. Select an eligible alias only after
    // binding it to this chunk's exact profile/gen; a raw twin from another
    // generation or manifest cannot lend authority to it.
    if pointer.path_at_commit.is_none() {
        let mut selected: Option<&kio_core::dag::TreeEntry> = None;
        for candidate in &entry_candidates {
            if !candidate.normalize.as_ref().is_some_and(|normalize| {
                normalize.tool_profile_hash == pointer.tool_profile_hash
                    && normalize.r#gen == chunk.r#gen
            }) || !current_policy
                .allows_path(&candidate.path)
                .map_err(pipeline_to_kio)?
            {
                continue;
            }
            if selected.is_none_or(|current| candidate.path.as_bytes() < current.path.as_bytes()) {
                selected = Some(candidate);
            }
        }
        let Some(selected) = selected else {
            return Err(evidence_policy_denied());
        };
        entry_normalize = selected.normalize.clone();
        entry_path = Some(selected.path.clone());
    }
    let entry_path = entry_path.expect("non-shallow pointer has an authenticated tree path");
    if !current_policy
        .allows_path(&entry_path)
        .map_err(pipeline_to_kio)?
    {
        return Err(KioError::new(
            "KIO-E-EVIDENCE-POLICY-001",
            "evidence path is excluded by current management policy",
            json!({}),
            ExitCode::PermanentFailure,
        ));
    }
    // 08 §3.1 procedures 6a/6b: run attribution against the exact selected
    // alias, including its pinned manifest, after URI alias selection.
    let mut manifest_missing = false;
    if let Some(normalize) = &entry_normalize {
        match verify_point_in_time_attribution(&target, &repo, pointer, normalize, &chunk, None)? {
            PointInTimeAttribution::Alive => {}
            PointInTimeAttribution::ManifestMissing => manifest_missing = true,
            PointInTimeAttribution::NotFound => {
                return Err(purge_not_found_error(&target, &pointer.raw_hash));
            }
            PointInTimeAttribution::StoreCorrupt => {
                return Err(point_in_time_store_corrupt_error(pointer));
            }
            PointInTimeAttribution::IndexRebuilding => {
                return Err(point_in_time_index_rebuilding_error());
            }
        }
    }
    let (view_path, view_byte_start, view_byte_end) =
        match resolve_chunk_view(&target.kio_dir, &chunk, ctx.control) {
            Some((path, start, end)) => (Some(path), Some(start), Some(end)),
            None => (None, None, None),
        };

    // Raw object resolution: working tree first (rename-tolerant), else CAS
    // read-only expansion. Absent from both with no tombstone -> not_found.
    match open_authorized_raw_object(&target, &pointer.raw_hash, Some(&entry_path), ctx.control)? {
        Some((path, temporary)) => {
            // LC8-LC14 canonical dispatch (item 3) + §I checkpoint 2
            // (LC54/LC55), combined: either failure discards the resolved
            // text/path and any temp open-cache this call may have just
            // published (LC57's cache publish-then-final-check ordering).
            if let Err(error) =
                enforce_canonical_marker_barrier(&target, &pointer.raw_hash, true, ctx.control)
                    .and_then(|()| current_policy.revalidate().map_err(pipeline_to_kio))
                    .and_then(|()| {
                        if policy_repo.scope_identity()?.scope_id == target.scope_id
                            && policy_repo.head_commit_hash()?.as_deref()
                                == Some(policy_head.as_str())
                        {
                            Ok(())
                        } else {
                            Err(KioError::new(
                                "KIO-E-EVIDENCE-POLICY-001",
                                "evidence scope authority changed while it was being opened; retry",
                                json!({}),
                                ExitCode::PartialFailure,
                            ))
                        }
                    })
                    .and_then(|()| checkpoint.recheck())
            {
                if temporary {
                    let _ = fs::remove_file(&path);
                }
                return Err(error);
            }
            Ok(PointerResolution {
                path: Some(path),
                view_path,
                view_byte_start,
                view_byte_end,
                temporary,
                commit_shallow,
                manifest_missing,
            })
        }
        // raw_present=false always yields Err from the canonical dispatch
        // (LC12/13/14 jointly cover every marker state); this recovers the
        // precise code in place of the old unconditional not-found guess.
        None => {
            Err(
                enforce_canonical_marker_barrier(&target, &pointer.raw_hash, false, ctx.control)
                    .err()
                    .unwrap_or_else(|| purge_not_found_error(&target, &pointer.raw_hash)),
            )
        }
    }
}

/// Open only the current-policy-authorized working-tree alias when one was
/// authenticated by pointer/short-hash resolution. Falling back to CAS keeps
/// rename tolerance without scanning a different, denied raw twin by hash.
fn open_authorized_raw_object(
    target: &ScopeTarget,
    raw_hash: &str,
    authorized_path: Option<&str>,
    control: &ControlHandle,
) -> Result<Option<(PathBuf, bool)>> {
    if let Some(authorized_path) = authorized_path {
        let path = target.repo_root.join(authorized_path);
        if fs::symlink_metadata(&path)
            .map(|metadata| metadata.file_type().is_file())
            .unwrap_or(false)
            && hash_verified_scan_input(&target.repo_root, authorized_path, MAX_RAW_OBJECT_BYTES)
                .map(|identity| identity.raw_hash == raw_hash)
                .unwrap_or(false)
        {
            return Ok(Some((path, false)));
        }
    }
    open_cas_byte_object(target, "raw", false, raw_hash, authorized_path, control)
}

/// Open a CAS byte object (03 §2: raw / prepared / image), expanding it read-only
/// under `$XDG_CACHE_HOME/kio/open` when it lives only in the store. `subdir`
/// selects the CAS type directory ("raw" / "prepared" / "image"). For `raw` the
/// working tree is checked first (rename tolerant, 05 §4.2); derived byte objects
/// live only in the CAS. Returns `Ok(None)` when the object is absent.
pub(crate) fn open_cas_byte_object(
    target: &ScopeTarget,
    subdir: &str,
    scan_working_tree: bool,
    hash: &str,
    path_hint: Option<&str>,
    control: &ControlHandle,
) -> Result<Option<(PathBuf, bool)>> {
    if !is_hash(hash) {
        return Err(KioError::invalid_usage("CAS object hash is invalid"));
    }
    if scan_working_tree && let Some(path) = find_working_tree_raw(&target.repo_root, hash)? {
        return Ok(Some((path, false)));
    }
    let Some((_object_path, bytes)) =
        read_cas_byte_object(&target.kio_dir, subdir, hash, MAX_RAW_OBJECT_BYTES)?
    else {
        return Ok(None);
    };
    let basename = path_hint.unwrap_or("object");
    // P9 (06 §1.1): the read-only expansion cache belongs under $XDG_CACHE_HOME
    // (regenerable, safe to purge), not $XDG_DATA_HOME (durable truth/state).
    let cache = open_cache_path(subdir, basename, hash, control);
    // M5/PA08 (06 §1.1 L150): the open cache is idempotent. A prior open
    // already materialized this object read-only; a second open must reuse
    // it, not `fs::copy` onto a read-only destination (EACCES). But reuse is
    // never based on existence alone — the cache leaf's OWN content sha256
    // is re-verified against the dir key (`hash`) on EVERY reuse, not just at
    // first materialization (an externally-modified or torn-write cache leaf
    // must never be served as authentic Evidence). A mismatch fails closed
    // (`KIO-E-STORE-CORRUPT-001`, exit 4) WITHOUT touching the existing cache
    // file — recovery is the user deleting the cache themselves.
    if cache.is_file() {
        verify_bounded_cas_object(&cache, hash, MAX_RAW_OBJECT_BYTES)?;
        // R9-3: a cache dir/file materialized by an earlier (world-readable) build
        // is corrected in place on reuse — harden the subtree to 0700 and the file
        // to 0400 so document bytes written before this fix stop leaking to
        // group/other on a multi-user host.
        if let Some(parent) = cache.parent() {
            harden_open_cache_subtree(parent, control);
        }
        platform::harden_open_cache_hit(&cache);
        return Ok(Some((cache, true)));
    }
    // Q2: prepared / image CAS objects were historically written non-atomically
    // (`fs::write` straight to the final path), so a crash / ENOSPC could leave a
    // partial file under a correct `sha256:` name that `if !path.exists()` then
    // adopts forever. Verify the object's bytes hash to their filename before
    // serving — mirroring `ObjectStore::read_by_hash` (cas.rs) — so a torn /
    // corrupt object is rejected as STORE-CORRUPT instead of returned as authentic
    // evidence. The immutable object is verified once here at first
    // materialization; the read-only open cache is reused as-is thereafter (M5).
    if let Some(parent) = cache.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| KioError::io(err.to_string(), parent.display().to_string()))?;
        // R9-3: the open/view expansion cache holds document bytes / images /
        // pre-OCR raw data, so — like the CAS it mirrors (P2, 0600) — its whole
        // `$XDG_CACHE_HOME/kio` subtree must be owner-only (0700), not the umask
        // default (0755, world-readable).
        harden_open_cache_subtree(parent, control);
    }
    // R10-6: write the cache file atomically (temp in the same dir -> fsync -> 0400
    // -> rename) so a crash / ENOSPC / SIGKILL mid-write can never leave a torn
    // partial under the final cache name that the M5 cache-hit path (which does NOT
    // re-verify bytes) would later serve as authentic Evidence.
    write_open_cache_atomic(&cache, &bytes)?;
    Ok(Some((cache, true)))
}

fn read_bounded_cas_object(path: &Path, expected_hash: &str, max_bytes: u64) -> Result<Vec<u8>> {
    read_or_verify_bounded_cas_object(path, expected_hash, max_bytes, true)
}

fn verify_bounded_cas_object(path: &Path, expected_hash: &str, max_bytes: u64) -> Result<()> {
    read_or_verify_bounded_cas_object(path, expected_hash, max_bytes, false).map(|_| ())
}

fn read_or_verify_bounded_cas_object(
    path: &Path,
    expected_hash: &str,
    max_bytes: u64,
    materialize: bool,
) -> Result<Vec<u8>> {
    let listed = fs::symlink_metadata(path)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    if !listed.file_type().is_file() || listed.len() > max_bytes {
        return Err(store_corrupt_error(
            path,
            format!("CAS object is not a regular file within the {max_bytes} byte limit"),
        ));
    }
    platform::validate_cached_cas_links(&listed, path)?;
    let mut file = fs::File::open(path)
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    let opened = file
        .metadata()
        .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
    if !opened.is_file() || opened.len() != listed.len() || opened.len() > max_bytes {
        return Err(store_corrupt_error(
            path,
            "CAS object changed while it was opened",
        ));
    }
    let mut bytes = Vec::new();
    if materialize {
        let capacity = usize::try_from(opened.len()).map_err(|_| {
            store_corrupt_error(
                path,
                "CAS object size cannot be represented by this process",
            )
        })?;
        bytes.try_reserve_exact(capacity).map_err(|_| {
            store_corrupt_error(
                path,
                "CAS object cannot fit within the process memory limit",
            )
        })?;
    }
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|err| KioError::io(err.to_string(), path.display().to_string()))?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > max_bytes {
            return Err(store_corrupt_error(
                path,
                format!("CAS object exceeds the {max_bytes} byte limit while being read"),
            ));
        }
        hasher.update(&buffer[..count]);
        if materialize {
            bytes.extend_from_slice(&buffer[..count]);
        }
    }
    let actual_hash = format!("sha256:{}", kio_core::cas::lower_hex(&hasher.finalize()));
    if actual_hash != expected_hash {
        return Err(store_corrupt_error(path, "CAS object hash mismatch"));
    }
    Ok(bytes)
}

/// The read-only open/view expansion cache path for a CAS object. R10-6: the
/// per-object directory is the FULL `sha256` hex (not a 12-char/48-bit prefix), so
/// two objects that share a 12-hex prefix and a basename can no longer collide onto
/// one cache file. PA03 (§A, U22)/PA12-13 (§C, U24): `subdir == "image"` nests
/// under an extra `image/` type segment (`~/.cache/kio/open/image/<hash>/...`),
/// separating it from the flat `raw`/`prepared` namespace (`~/.cache/kio/open/
/// <hash>/...`) — a raw object and an image object can share the same digest
/// (identical byte content ingested both ways), and without this segment their
/// cache directories would collide and one materialize would silently serve/
/// overwrite the other's cache entry. `subdir` is the same CAS type
/// discriminator `open_cas_byte_object` already threads through
/// (`"raw"`/`"prepared"`/`"image"`), so purge's eviction side (`evict_open_cache`,
/// kio-cli/purge.rs) mirrors this exact split via its own `is_image` flag.
pub(crate) fn open_cache_path(
    subdir: &str,
    basename: &str,
    hash: &str,
    control: &ControlHandle,
) -> PathBuf {
    let mut root = cache_home(control).join("kio/open");
    if subdir == "image" {
        root = root.join("image");
    }
    root.join(hash.trim_start_matches("sha256:"))
        .join(portable_cache_leaf(basename))
}

/// R10-6: crash-atomic write of the open/view expansion cache file. Writes bytes to
/// a uniquely-named temp in the SAME directory (created 0600 so the body never
/// exists world-readable, R9-3), fsyncs, drops the temp to 0400 (read-only), then
/// renames into the final path. A crash before the rename leaves only the temp,
/// never a torn file at the served name; the temp is removed on any failure. Mirrors
/// `atomic_write_cas_object`.
pub(crate) fn write_open_cache_atomic(cache: &Path, bytes: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let parent = cache
        .parent()
        .ok_or_else(|| KioError::io("cache path has no parent", cache.display().to_string()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".tmp-{}-{}-{}", process::id(), nanos, seq));
    let result = (|| -> Result<()> {
        let mut open_options = OpenOptions::new();
        open_options.write(true).create(true).truncate(true);
        platform::configure_owner_only_file(&mut open_options);
        let mut file = open_options
            .open(&temp)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        file.write_all(bytes)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        file.sync_all()
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        drop(file);
        // Drop the temp to read-only BEFORE publishing so the served file is never
        // writable (mirrors the pre-fix final-path 0400 hardening).
        let mut permissions = fs::metadata(&temp)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?
            .permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&temp, permissions)
            .map_err(|err| KioError::io(err.to_string(), temp.display().to_string()))?;
        fs::rename(&temp, cache)
            .map_err(|err| KioError::io(err.to_string(), cache.display().to_string()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// R9-3: restrict the open/view expansion cache subtree to owner-only (0700) on
/// unix, walking from `leaf` up to `$XDG_CACHE_HOME/kio` inclusive. The cache
/// materializes raw / prepared / image CAS bytes, so it must carry the same
/// 0700/0600 hardening as the CAS itself (P2) rather than inherit the umask
/// default (0755). Best-effort (the cache is regenerable); the `starts_with`
/// guard keeps the walk from ever chmod-ing anything above the cache root
/// (e.g. `$XDG_CACHE_HOME` itself). No-op on non-unix.
pub(crate) fn harden_open_cache_subtree(leaf: &Path, control: &ControlHandle) {
    platform::harden_open_cache_subtree(leaf, &cache_home(control).join("kio"));
}

pub(crate) const MAX_WORKING_TREE_SCAN_ENTRIES: usize = 100_000;

#[derive(Debug)]
pub(crate) struct WorkingTreeScanBudget {
    pub(crate) remaining_bytes: u64,
    pub(crate) remaining_entries: usize,
}

#[derive(Debug)]
pub(crate) struct WorkingTreeScanAttempt {
    pub(crate) max_bytes: u64,
    pub(crate) reserved_bytes: u64,
}

impl WorkingTreeScanBudget {
    fn new() -> Self {
        Self {
            remaining_bytes: DEFAULT_MAX_ARCHIVE_SCOPE_BYTES,
            remaining_entries: MAX_WORKING_TREE_SCAN_ENTRIES,
        }
    }

    pub(crate) fn consume_entry(&mut self) -> bool {
        if self.remaining_entries == 0 {
            return false;
        }
        self.remaining_entries -= 1;
        true
    }

    pub(crate) fn reserve_file(&mut self, declared_size: u64) -> Option<WorkingTreeScanAttempt> {
        if self.remaining_bytes == 0 {
            return None;
        }
        let max_bytes = declared_size
            .min(MAX_RAW_OBJECT_BYTES)
            .min(self.remaining_bytes.saturating_sub(1));
        let reserved_bytes = max_bytes.saturating_add(1).min(self.remaining_bytes);
        self.remaining_bytes -= reserved_bytes;
        Some(WorkingTreeScanAttempt {
            max_bytes,
            reserved_bytes,
        })
    }

    pub(crate) fn finish_success(&mut self, attempt: &WorkingTreeScanAttempt, actual_size: u64) {
        let unused = attempt.reserved_bytes.saturating_sub(actual_size);
        self.remaining_bytes = self.remaining_bytes.saturating_add(unused);
    }
}

fn find_working_tree_raw(root: &Path, raw_hash: &str) -> Result<Option<PathBuf>> {
    let mut budget = WorkingTreeScanBudget::new();
    for entry in fs::read_dir(root)
        .map_err(|err| KioError::io(err.to_string(), root.display().to_string()))?
    {
        let entry =
            entry.map_err(|err| KioError::io(err.to_string(), root.display().to_string()))?;
        if !budget.consume_entry() {
            break;
        }
        if entry.file_name() == ".kio" {
            continue;
        }
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let declared_size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        let Some(attempt) = budget.reserve_file(declared_size) else {
            break;
        };
        let path = entry.path();
        let Some(input_path) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let identity = match hash_verified_scan_input(root, &input_path, attempt.max_bytes) {
            Ok(identity) => identity,
            Err(_) => continue,
        };
        budget.finish_success(&attempt, identity.size_bytes);
        if identity.raw_hash == raw_hash {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Portable fan-out path for a content-hashed byte object. Logical identities retain
/// `sha256:<digest>`; only the physical leaf omits the Windows-invalid colon.
pub(crate) fn cas_object_path(kio_dir: &Path, subdir: &str, hash: &str) -> Result<PathBuf> {
    fanout_path(kio_dir.join("objects").join(subdir), hash)
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(KioError::io(error.to_string(), path.display().to_string())),
    }
}

pub(crate) fn existing_cas_object_path(
    kio_dir: &Path,
    subdir: &str,
    hash: &str,
) -> Result<Option<PathBuf>> {
    let canonical = cas_object_path(kio_dir, subdir, hash)?;
    if path_entry_exists(&canonical)? {
        Ok(Some(canonical))
    } else {
        Ok(None)
    }
}

pub(crate) fn cas_object_present(
    kio_dir: &Path,
    subdir: &str,
    hash: &str,
    max_bytes: u64,
) -> Result<bool> {
    let canonical = cas_object_path(kio_dir, subdir, hash)?;
    if path_entry_exists(&canonical)? {
        verify_bounded_cas_object(&canonical, hash, max_bytes)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub(crate) fn read_cas_byte_object(
    kio_dir: &Path,
    subdir: &str,
    hash: &str,
    max_bytes: u64,
) -> Result<Option<(PathBuf, Vec<u8>)>> {
    let canonical = cas_object_path(kio_dir, subdir, hash)?;
    if path_entry_exists(&canonical)? {
        let bytes = read_bounded_cas_object(&canonical, hash, max_bytes)?;
        return Ok(Some((canonical, bytes)));
    }
    Ok(None)
}

/// `.kio/tombstones/ab/cd/<raw-digest>` (05 §3.5). `Ok(None)` when no
/// tombstone exists *or* its canonical state is not active (LC1:
/// `is_active()` false — e.g. retired by resurrection, U19; a retired
/// tombstone must resolve as alive, not as a dead pointer). Delegates to
/// `kio_core::purge::PurgeState` for the v1-flat/v2-`events[]` dispatch and
/// structural validation (LC5/LC15), then projects the tail `purged` event
/// into the flat `purged_at`/`purged_reason`/`purged_in_commit` response
/// shape (08 §4.1) that callers (`tombstone_error`,
/// `enforce_purge_read_barrier`) expect, augmented with the resolved
/// `scope_path`.
///
/// R23-11 (05-runtime.md §3.5 L934/L942): before trusting an active tail to
/// hide content, this — the primary resolver entry point
/// (`enforce_purge_read_barrier`'s shared dependency) —
/// now also runs the shared bounded (O(1), no ref-reachability walk)
/// semantic check: `in_commit` must resolve to a real, verified
/// `commit_type=purged` commit that actually lists this raw_hash and whose
/// `created_at` matches `at`. A structurally valid but semantically
/// fabricated marker (AUD-09/A-07) surfaces as `KIO-E-STORE-CORRUPT-001`
/// instead of silently hiding genuine content.
pub(crate) fn read_tombstone(
    target: &ScopeTarget,
    raw_hash: &str,
    control: &ControlHandle,
) -> Result<Option<Value>> {
    let Some(record) = PurgeState::open_with_control(&target.kio_dir, control.clone())?
        .read_tombstone(raw_hash)?
    else {
        return Ok(None);
    };
    if !record.is_active() {
        return Ok(None);
    }
    let tail = record.tail();
    kio_core::purge::verify_marker_binding_bounded_with_control(
        &target.kio_dir,
        raw_hash,
        tail,
        &control.now_utc_seconds(),
        control,
    )?;
    Ok(Some(json!({
        "raw_hash": record.raw_hash,
        "purged_at": tail.at,
        "purged_reason": tail.reason,
        "purged_in_commit": tail.in_commit,
        "scope_path": target.kio_dir.display().to_string(),
    })))
}

pub(crate) struct ObjectUri {
    scope_id: String,
    object_type: String,
    hash: String,
}

/// A live policy/HEAD snapshot that authorizes one image object's materialized
/// bytes. Object CAS presence and Markdown references alone never authorize
/// disclosure: an authenticated normalized unit must typed-own the image and
/// at least one of its retained owner paths must remain policy-eligible.
pub(crate) struct ImageObjectReadAuthorization {
    policy: CurrentPolicyEvaluator,
    head: String,
    scope_id: String,
}

impl ImageObjectReadAuthorization {
    fn revalidate(&self, repo: &Repository) -> Result<()> {
        self.policy.revalidate().map_err(pipeline_to_kio)?;
        if repo.head_commit_hash()?.as_deref() != Some(self.head.as_str())
            || repo.scope_identity()?.scope_id != self.scope_id
        {
            return Err(KioError::new(
                "KIO-E-OBJECT-POLICY-001",
                "image object authority changed while it was being opened; retry",
                json!({}),
                ExitCode::PartialFailure,
            ));
        }
        Ok(())
    }
}

fn image_object_policy_denied() -> KioError {
    KioError::new(
        "KIO-E-OBJECT-POLICY-001",
        "image object is not referenced by a currently eligible managed source",
        json!({}),
        ExitCode::PermanentFailure,
    )
}

/// Prove that an image object occurs in retained immutable typed ownership
/// from this exact scope, then apply current policy to every historical owner
/// alias of that normalized identity. Neither SQLite associations, raw hashes,
/// nor free-form Markdown can establish this authority: all can outlive a
/// move, copy, or current Ignore update.
fn authorize_image_object_read(
    repo: &Repository,
    object: &ObjectUri,
) -> Result<ImageObjectReadAuthorization> {
    if object.object_type != "image" || repo.scope_identity()?.scope_id != object.scope_id {
        return Err(image_object_policy_denied());
    }
    let policy = current_embedding_policy(repo)?;
    let head = repo
        .head_commit_hash()?
        .ok_or_else(image_object_policy_denied)?;
    let instances = retained_history_instances(repo, &head)?;
    let authority = image_authority::retained_image_authority(repo, &instances, &policy)?;
    if authority.permits(&object.hash) {
        return Ok(ImageObjectReadAuthorization {
            policy,
            head,
            scope_id: object.scope_id.clone(),
        });
    }
    Err(image_object_policy_denied())
}

/// Parses a `kio://<scope_id>/object/<type>/<hash>` object-reference URI
/// (08 §2.3). Returns `Ok(None)` for non-object URIs (Evidence Pointer URIs,
/// whose second segment is always a `sha256:` commit, never the literal
/// `object`). A syntactically-`object` URI that is malformed is `Err` (exit 2).
pub(crate) fn parse_object_uri(input: &str) -> Result<Option<ObjectUri>> {
    let Some(rest) = input.strip_prefix("kio://") else {
        return Ok(None);
    };
    let (path, _query) = rest.split_once('?').unwrap_or((rest, ""));
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.get(1) != Some(&"object") {
        return Ok(None);
    }
    if parts.len() != 4 {
        return Err(KioError::invalid_usage(
            "object URI must be kio://<scope_id>/object/<type>/<hash>",
        ));
    }
    let (scope_id, object_type, hash) = (parts[0], parts[2], parts[3]);
    if scope_id.is_empty() {
        return Err(KioError::invalid_usage("object URI is missing scope_id"));
    }
    // PA01 (§A, U22): MVP only issues/accepts `image` object URIs (06 §1.1
    // L117-119, 08 §2.3 L110-113) — every other type (`raw`/`chunk`/
    // `prepared`/`normalized`) is rejected here, at parse time, with
    // `KIO-E-CONFIG-USAGE-001` (exit 2). This intentionally makes
    // `resolve_object_uri`'s `raw`/`chunk`/`prepared` dispatch branches
    // unreachable from a real object URI today (kept, not deleted — MVP
    // scoping, not a permanent design decision) rather than duplicating the
    // type gate at two separate layers.
    const VALID_TYPES: [&str; 1] = ["image"];
    if !VALID_TYPES.contains(&object_type) {
        return Err(KioError::invalid_usage(format!(
            "unknown object type in URI: {object_type}"
        )));
    }
    if !is_hash(hash) {
        return Err(KioError::invalid_usage(
            "object URI hash must be sha256 lowercase hex",
        ));
    }
    Ok(Some(ObjectUri {
        scope_id: scope_id.to_owned(),
        object_type: object_type.to_owned(),
        hash: hash.to_owned(),
    }))
}

/// Resolves an `object` reference URI (08 §2.3) for `kio open` / `kio view`.
/// This is a distinct path from Evidence Pointer resolution.
fn resolve_object_uri(ctx: &Invocation<'_>, object: &ObjectUri, as_view: bool) -> Result<Value> {
    let status = if as_view { "viewed" } else { "opened" };
    let target = resolve_scope_target(
        ctx.cwd(),
        ctx.interaction.as_ref(),
        &object.scope_id,
        None,
        ctx.control,
    )?;
    // QB6 (step4b-contract-tests-p3b.md §A, 10 §3 L300-305): (0)
    // kio_format_version compatibility, checked before (1)/(3) below. This
    // object-URI path previously never opened a `Repository` at all — every
    // read went straight through `target`/CAS — so a format-incompatible
    // scope's object URIs resolved with zero version enforcement; validate
    // (and discard, this path has no further use for the `Repository`
    // handle) exactly as the pointer/short-hash resolution paths do.
    let repo = Repository::open_in_session(&target.repo_root, ctx.control.clone(), ctx.locks)?;
    // QB5/QB6/裁定1: shared (1)+(3) preflight pair.
    let checkpoint = preflight_barrier_and_index(&target.kio_dir, ctx.control)?;
    if object.object_type == "chunk" {
        // Chunk JSONL rows are association truth, not trusted text bytes. Read
        // the self-authenticating semantic object from CAS and require the
        // ledger tuple to agree before returning content.
        let chunk = repo.object_store().read_chunk(&object.hash)?;
        let association = read_stored_chunks(&target.kio_dir)?
            .into_iter()
            .find(|stored| stored.row.chunk_id == object.hash)
            .ok_or_else(|| KioError::not_found(object.hash.clone()))?;
        if association.row.raw_hash != chunk.raw_hash
            || association.row.tool_profile_hash != chunk.tool_profile_hash
            || association.row.r#gen != chunk.r#gen
            || association.row.unit_key != chunk.unit_key
            || association.row.unit_content_hash != chunk.unit_content_hash
        {
            return Err(store_corrupt_error(
                &chunks_jsonl_path(&target.kio_dir),
                "chunk association does not match its semantic CAS object",
            ));
        }
        // LC8-LC14 canonical dispatch (item 3) + §I checkpoint 2 (LC54/LC55).
        let raw_present = raw_object_present(&target, &chunk.raw_hash)?;
        enforce_canonical_marker_barrier(&target, &chunk.raw_hash, raw_present, ctx.control)?;
        checkpoint.recheck()?;
        let payload = ObjectChunkResponse {
            status: status.to_owned(),
            object_type: "chunk".to_owned(),
            hash: object.hash.clone(),
            text: chunk.text,
        };
        let body = if as_view {
            ResponseBody::View(ViewResponse::ObjectChunk(payload))
        } else {
            ResponseBody::Open(OpenResponse::ObjectChunk(payload))
        };
        return project_legacy(CommandResponse {
            body,
            completion: Completion::Complete,
        });
    }
    // M7: dispatch each object_type to its correct CAS directory (03 §2 / 07 §5.2)
    // instead of routing every byte object through objects/raw:
    //   raw      -> objects/raw       (working-tree-first, rename tolerant)
    //   image    -> objects/image     (embedded document images, 07 §5.2)
    //   prepared -> objects/prepared  (pre-Markdownize intermediate)
    // `normalized` is the full-text view, path-named by
    // `<raw_hash>.<tool_profile_hash>.g<gen>` (03 §2.1) and not addressable by a
    // single content hash, so it is not resolvable through an object URI.
    let (subdir, scan_working_tree) = match object.object_type.as_str() {
        "raw" => ("raw", true),
        "image" => ("image", false),
        "prepared" => ("prepared", false),
        other => {
            return Err(KioError::invalid_usage(format!(
                "object type '{other}' is not resolvable by a single-hash object URI"
            )));
        }
    };
    // Image/prepared object URIs carry only the derived-object hash, so no
    // trustworthy raw association can be recovered at this boundary. During
    // the short destructive window, fail the whole derived-object surface
    // closed; normal reads resume after terminal publication/deletion. The
    // `raw` branch's own pre-`open_cas_byte_object` barrier check is
    // deliberately NOT duplicated here (item 3): raw presence is not yet
    // known at this point, and LC57's cache-publish-then-final-check
    // ordering means the authoritative canonical dispatch belongs after
    // `open_cas_byte_object` resolves it either way, not before.
    if object.object_type != "raw"
        && PurgeState::open_with_control(&target.kio_dir, ctx.control.clone())?
            .read_journal()?
            .is_some_and(|journal| journal.phase.is_barrier_visible())
    {
        return Err(KioError::new(
            "KIO-E-PURGE-NOT-FOUND-001",
            "derived object access is hidden by an in-progress purge barrier",
            json!({
                "object_type": object.object_type,
                "hash": object.hash,
                "scope_path": target.kio_dir.display().to_string(),
                "purge_state": "in_progress",
            }),
            ExitCode::PermanentFailure,
        ));
    }
    // Only image object URIs are currently accepted, but keep the authority
    // check adjacent to derived-object materialization so a future accepted
    // type cannot accidentally inherit image CAS's disclosure semantics.
    let image_authorization = (object.object_type == "image")
        .then(|| authorize_image_object_read(&repo, object))
        .transpose()?;
    match open_cas_byte_object(
        &target,
        subdir,
        scan_working_tree,
        &object.hash,
        None,
        ctx.control,
    )? {
        Some((path, temporary)) => {
            // LC8-LC14 canonical dispatch (item 3, `raw` only) + §I
            // checkpoint 2 (LC54/LC55, every object type).
            let blocked_after_open = if object.object_type == "raw" {
                enforce_canonical_marker_barrier(&target, &object.hash, true, ctx.control)
                    .and_then(|()| checkpoint.recheck())
                    .err()
            } else if PurgeState::open_with_control(&target.kio_dir, ctx.control.clone())?
                .read_journal()?
                .is_some_and(|journal| journal.phase.is_barrier_visible())
            {
                Some(KioError::new(
                    "KIO-E-PURGE-NOT-FOUND-001",
                    "derived object access is hidden by an in-progress purge barrier",
                    json!({
                        "object_type": object.object_type,
                        "hash": object.hash,
                        "scope_path": target.kio_dir.display().to_string(),
                        "purge_state": "in_progress",
                    }),
                    ExitCode::PermanentFailure,
                ))
            } else {
                image_authorization
                    .as_ref()
                    .and_then(|authorization| authorization.revalidate(&repo).err())
                    .or_else(|| checkpoint.recheck().err())
            };
            if let Some(error) = blocked_after_open {
                if temporary {
                    let _ = fs::remove_file(&path);
                }
                return Err(error);
            }
            let payload = ObjectFileResponse {
                status: status.to_owned(),
                object_type: object.object_type.clone(),
                hash: object.hash.clone(),
                path,
                temporary,
            };
            let body = if as_view {
                ResponseBody::View(ViewResponse::ObjectFile(payload))
            } else {
                ResponseBody::Open(OpenResponse::ObjectFile(payload))
            };
            project_legacy(CommandResponse {
                body,
                completion: Completion::Complete,
            })
        }
        // raw_present=false always yields Err from the canonical dispatch
        // (LC12/13/14 jointly cover every marker state).
        None if object.object_type == "raw" => {
            Err(
                enforce_canonical_marker_barrier(&target, &object.hash, false, ctx.control)
                    .err()
                    .unwrap_or_else(|| purge_not_found_error(&target, &object.hash)),
            )
        }
        None => Err(KioError::not_found(object.hash.clone())),
    }
}
