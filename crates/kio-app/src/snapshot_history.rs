use crate::commands;
use crate::gc;
use crate::index::chunk_ledger::ensure_purge_epoch_initialized;
use crate::index::pipeline::explicitly_allowed_tier_a_paths;
use crate::invocation::Invocation;
use crate::object_persistence::index_to_kio;
use crate::object_persistence::pipeline_to_kio;
use crate::response::CommandResponse;
use crate::response::Completion;
use crate::response::ResponseBody;
use crate::response::history::DiffResponse;
use crate::response::history::InspectResponse;
use crate::response::history::LogResponse;
use crate::response::history::RawInspection;
use crate::response::history::TagDeleteResponse;
use crate::response::history::TagResponse;
use crate::response::history::TagSetResponse;
use crate::response::project_legacy;
use crate::scope_access::enforce_purge_read_barrier;
use crate::scope_access::preflight_barrier_and_index;
use crate::scope_access::scope_target;
use crate::search::projection::capture_ready_replica_snapshot;
use crate::search::projection::log_aggregator_degraded;
use crate::search::projection::mark_replica_rebuilding_or_log;
use crate::search::projection::republish_equivalent_manual_snapshot;
use crate::search_time::PositiveDuration;
use crate::search_time::since_cutoff_seconds;
use crate::settings::validate_repo_tool_lock;
use crate::task_execution::set_exit_override;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::cas::canonical_json_bytes;
use kio_core::gc::GcAutomationBinding;
use kio_core::gc::GcAutomationMode;
use kio_core::gc::GcSweepSession;
use kio_core::gc::SnapshotAutoBinding;
use kio_core::gc::SnapshotAutoStateBinding;
use kio_core::gc::snapshot_auto_clock_error;
use kio_core::portable::PORTABLE_TAGS_DIRECTORY;
use kio_core::portable::portable_tag_leaf;
use kio_core::scope::InspectedObject;
use kio_core::scope::Repository;
use kio_core::scope::SnapshotOutcome;
use kio_core::scope::parse_utc_seconds;
use kio_index::fts::BoundGcIndexMetadata;
use kio_index::fts::FtsSchemaConfig;
use kio_index::fts::FtsTokenizer;
use kio_index::fts::read_bound_gc_index_metadata;
use kio_pipeline::scan::ScanPreview;
use kio_pipeline::scan::ScanPreviewRequest;
use kio_pipeline::scan::build_bound_scan_preview;
use kio_pipeline::scan::build_scan_preview;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

pub(crate) fn run_snapshot_create(
    ctx: &Invocation<'_>,
    args: commands::SnapshotCreateArgs,
    adapter_settings: &Arc<AdapterRuntimeSettings>,
) -> Result<Value> {
    // Opening an ordinary repository can self-heal HEAD. Bind and
    // validate without repair first, acquire the GC-aware store lock,
    // and only then perform that write. This keeps even the open path
    // behind the active-sweep barrier without following a replaced
    // public `.kio` pathname.
    let repo = Repository::open_recovery_in_session(ctx.cwd(), ctx.control.clone(), ctx.locks)?;
    let (recovered_before_publication, gc_binding) =
        match gc::preflight_automatic(&repo, "snapshot")? {
            gc::AutomaticGcPreflight::Proceed { recovered, binding } => (recovered, binding),
            gc::AutomaticGcPreflight::Deferred(report) => {
                return Ok(gc::deferred_before_publication("snapshot", report));
            }
        };
    let lock = repo.lock_store_in_session(repo.lock_session())?;
    repo.recover_publication()?;
    validate_repo_tool_lock(&repo)?;
    // LC39/LC40 (see `ensure_purge_epoch_initialized`'s doc comment): a
    // scope created before this session's read-barrier wiring, or one
    // whose epoch file was otherwise lost, self-heals on its next write.
    ensure_purge_epoch_initialized(repo.kio_dir(), repo.control())?;
    // N2: a manual snapshot must exclude the same Tier A secrets `kio index`
    // does, or `.env`/`*.pem` plaintext lands irreversibly in objects/raw and
    // the latest tree. Compute the exclusion set from the scan preview (the
    // shared classifier) and pass it through the filtered snapshot path.
    let preview = build_scan_preview(
        &adapter_settings.declared_pricing("markdown"),
        &adapter_settings.declared_pricing("embedding"),
        ScanPreviewRequest {
            scope_path: repo.root().display().to_string(),
            include_raw_hashes: false,
            require_network_approval: false,
        },
    )
    .map_err(pipeline_to_kio)?;
    let excluded = preview
        .candidates
        .iter()
        .filter(|candidate| candidate.ignored)
        .map(|candidate| candidate.input_path.clone())
        .collect::<BTreeSet<_>>();
    let explicitly_allowed_tier_a = explicitly_allowed_tier_a_paths(&preview);
    // Keep the source HEAD, its ready replica header, and the optional
    // same-content projection reuse in one writer critical section.
    // `snapshot_filtered_with_policy` takes this lock reentrantly.
    let ready_replica_before_snapshot = capture_ready_replica_snapshot(&repo);
    let outcome = repo.snapshot_filtered_with_policy(
        args.message.as_deref(),
        None,
        &excluded,
        &explicitly_allowed_tier_a,
    )?;
    if let Some(commit_hash) = &outcome.commit_hash {
        ctx.control.append_event_log_in_session(
            ctx.locks,
            "KIO-I-COMMIT-CREATED-001",
            "commit created",
            json!({
                "commit_hash": commit_hash,
                "tree_hash": outcome.tree_hash,
            }),
        )?;
    }
    if let Some(commit_hash) = outcome.commit_hash.as_deref() {
        // Never leave the old Ready projection certified once HEAD has
        // advanced.  A narrow writer-side reuse path below may make the
        // new snapshot Ready again, but only after proving that it merely
        // removed no source content or normalization identity.
        mark_replica_rebuilding_or_log(ctx, repo.kio_dir(), commit_hash);
        if let Some(before) = ready_replica_before_snapshot.as_ref() {
            match republish_equivalent_manual_snapshot(&repo, before, commit_hash) {
                Ok(true) => {}
                Ok(false) => {}
                Err(reason) => log_aggregator_degraded(
                    ctx,
                    &format!("manual snapshot replica reuse {commit_hash} failed: {reason}"),
                ),
            }
        }
    }
    let output = json!({
        "status": if outcome.noop { "noop" } else { "created" },
        "message": outcome.message,
        "tree_hash": outcome.tree_hash,
        "commit_hash": outcome.commit_hash,
        "commit": outcome.commit,
        "stats": {
            "files_added": outcome.stats.files_added,
            "files_modified": outcome.stats.files_modified,
            "files_deleted": outcome.stats.files_deleted,
        }
    });
    // Automatic GC owns a distinct bound store lock.  The snapshot,
    // replica publication, and event log are already durable here;
    // release the writer lock before entering the resumable sweep.
    drop(lock);
    Ok(gc::attach_after_success(
        &repo,
        "snapshot",
        recovered_before_publication,
        gc_binding,
        output,
    ))
}

pub(crate) fn run_log(ctx: &Invocation<'_>, args: commands::LogArgs) -> Result<Value> {
    let repo = ctx.open_recovery_repository()?; // (0) kio_format_version
    validate_repo_tool_lock(&repo)?;
    // QB5/QB6/裁定1: shared (1)+(3) preflight pair.
    let checkpoint = preflight_barrier_and_index(repo.kio_dir(), repo.control())?;
    // QB50/QB51/QB54 (step4b-contract-tests-p3b.md §D, 裁定5): `--at
    // <commit>` resolves through the same HEAD/tag/hash operand
    // grammar `diff`/`tag`/`restore` already use
    // (`Repository::resolve_commit`) and becomes the history walk's
    // starting point instead of HEAD. QB51: a shallow-but-present
    // commit is a fine `--at` target — `log` only walks the
    // commit-object parent chain, never a tree, so tree discard is
    // irrelevant here (unlike `restore`/`search --at`).
    let start = args
        .at
        .as_deref()
        .map(|value| repo.resolve_commit(value))
        .transpose()?;
    // R16-1: `log` degrades on a missing ancestor commit — it returns the
    // healthy prefix plus `truncated` rather than dying on a raw
    // KIO-E-STORE-NOT-FOUND-001.
    let report = repo.log_from(start)?;
    // QB52/QB55/QB56 (§D, 裁定5): `--since <dur>` filters the walked
    // entries to `commit.created_at >= now - <dur>`, reusing search's
    // duration grammar (`PositiveDuration` — accepts `s`/`m`/`h`/`d`/`w`
    // units). Composes with `--at` as an intersection (recommendation
    // (a) — narrows whatever `--at` already selected as the walk's
    // origin, rather than picking a competing origin of its own); the
    // default HEAD-rooted walk origin is unchanged when `--at` is
    // absent (QB52's non-breaking requirement).
    let entries = match args.since.as_deref() {
        Some(duration) => {
            let duration = PositiveDuration::parse(duration)?;
            let now = parse_utc_seconds(&ctx.control.now_utc_seconds())
                .ok_or_else(|| KioError::schema("current time is not canonical UTC seconds"))?;
            let cutoff = since_cutoff_seconds(now, duration)?;
            report
                .entries
                .into_iter()
                .filter(|entry| {
                    parse_utc_seconds(&entry.commit.created_at)
                        .is_some_and(|created_at| created_at >= cutoff)
                })
                .collect::<Vec<_>>()
        }
        None => report.entries,
    };
    // §I checkpoint 2 (LC54/LC55). QB57: the wire shape (`commits` +
    // `truncated`) is unchanged by `--at`/`--since` — they narrow
    // which commits are listed, not the response shape.
    checkpoint.finish(project_legacy(CommandResponse {
        body: ResponseBody::Log(LogResponse {
            commits: entries,
            truncated: report.truncated,
        }),
        completion: Completion::Complete,
    })?)
}

pub(crate) fn run_diff(ctx: &Invocation<'_>, args: commands::DiffArgs) -> Result<Value> {
    let repo = ctx.open_recovery_repository()?; // (0) kio_format_version
    validate_repo_tool_lock(&repo)?;
    // QB5/QB6/裁定1: shared (1)+(3) preflight pair.
    let checkpoint = preflight_barrier_and_index(repo.kio_dir(), repo.control())?;
    let changes = repo.diff(&args.a, &args.b)?;
    // §I checkpoint 2 (LC54/LC55).
    checkpoint.finish(project_legacy(CommandResponse {
        body: ResponseBody::Diff(DiffResponse { changes }),
        completion: Completion::Complete,
    })?)
}

pub(crate) fn run_inspect(ctx: &Invocation<'_>, args: commands::InspectArgs) -> Result<Value> {
    let repo = ctx.open_recovery_repository()?; // (0) kio_format_version
    validate_repo_tool_lock(&repo)?;
    let target = scope_target(repo.root(), ctx.control)?;
    // QB5/QB6/裁定1: shared (1)+(3) preflight pair. Distinct from the
    // per-raw_hash `enforce_purge_read_barrier` calls below (LC11-14
    // tombstone dispatch) — see `ReadBarrierCheckpoint`'s doc comment.
    let checkpoint = preflight_barrier_and_index(&target.kio_dir, ctx.control)?;
    enforce_purge_read_barrier(&target, &args.hash, ctx.control)?;
    match repo.inspect(&args.hash)? {
        InspectedObject::Tree(tree) => {
            let value = project_legacy(CommandResponse {
                body: ResponseBody::Inspect(InspectResponse::Tree(tree)),
                completion: Completion::Complete,
            })?;
            // §I checkpoint 2 (LC54/LC55).
            checkpoint.finish(value)
        }
        InspectedObject::Commit(commit) => {
            let value = project_legacy(CommandResponse {
                body: ResponseBody::Inspect(InspectResponse::Commit(commit)),
                completion: Completion::Complete,
            })?;
            // §I checkpoint 2 (LC54/LC55).
            checkpoint.finish(value)
        }
        InspectedObject::Raw {
            raw_hash,
            size_bytes,
        } => {
            // Recheck after the metadata read so a barrier published in
            // the inspect window cannot leak even the raw object's size.
            enforce_purge_read_barrier(&target, &raw_hash, ctx.control)?;
            // §I checkpoint 2 (LC54/LC55).
            checkpoint.finish(project_legacy(CommandResponse {
                body: ResponseBody::Inspect(InspectResponse::Raw(RawInspection {
                    object_type: "raw".to_owned(),
                    raw_hash,
                    size_bytes,
                })),
                completion: Completion::Complete,
            })?)
        }
    }
}

pub(crate) fn run_tag(ctx: &Invocation<'_>, args: commands::TagArgs) -> Result<Value> {
    if args.delete && args.commit.is_some() {
        return Err(KioError::invalid_usage(
            "tag deletion does not accept a commit operand",
        ));
    }
    let repo = ctx.open_recovery_repository()?;
    validate_repo_tool_lock(&repo)?;
    let commit_hash = if args.delete {
        repo.delete_tag(&args.name)?
    } else {
        repo.tag(&args.name, args.commit.as_deref())?
    };
    let path = repo
        .kio_dir()
        .join("refs")
        .join(PORTABLE_TAGS_DIRECTORY)
        .join(portable_tag_leaf(&args.name));
    if args.delete {
        project_legacy(CommandResponse {
            body: ResponseBody::Tag(TagResponse::Delete(TagDeleteResponse {
                operation: "tag_delete".to_owned(),
                status: "deleted".to_owned(),
                tag: args.name,
                commit_hash,
                path,
            })),
            completion: Completion::Complete,
        })
    } else {
        project_legacy(CommandResponse {
            body: ResponseBody::Tag(TagResponse::Set(TagSetResponse {
                tag: args.name,
                commit_hash,
                path,
            })),
            completion: Completion::Complete,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ScheduledAutoObservation {
    gc: GcAutomationBinding,
    config: SnapshotAutoBinding,
    state: SnapshotAutoStateBinding,
    direct_entries: BTreeSet<String>,
    preview: ScanPreview,
    index: BoundGcIndexMetadata,
    change_count: u64,
    eligible: bool,
    eligibility_reason: Option<&'static str>,
    next_eligible_at: Option<String>,
    working_set_digest: String,
    idle: ScheduledIdleObservation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScheduledIdleObservation {
    Inactive,
    Baseline {
        observed_since: String,
        threshold_seconds: u64,
    },
    Changed {
        observed_since: String,
        threshold_seconds: u64,
    },
    Waiting {
        observed_since: String,
        observed_seconds: u64,
        threshold_seconds: u64,
    },
    Eligible {
        observed_since: String,
        observed_seconds: u64,
        threshold_seconds: u64,
    },
}

impl ScheduledIdleObservation {
    fn needs_state_publication(&self) -> bool {
        matches!(self, Self::Baseline { .. } | Self::Changed { .. })
    }

    fn observed_since(&self) -> Option<&str> {
        match self {
            Self::Inactive => None,
            Self::Baseline { observed_since, .. }
            | Self::Changed { observed_since, .. }
            | Self::Waiting { observed_since, .. }
            | Self::Eligible { observed_since, .. } => Some(observed_since),
        }
    }

    fn observed_seconds(&self) -> Option<u64> {
        match self {
            Self::Waiting {
                observed_seconds, ..
            }
            | Self::Eligible {
                observed_seconds, ..
            } => Some(*observed_seconds),
            Self::Baseline { .. } | Self::Changed { .. } => Some(0),
            Self::Inactive => None,
        }
    }

    fn threshold_seconds(&self) -> Option<u64> {
        match self {
            Self::Waiting {
                threshold_seconds, ..
            }
            | Self::Eligible {
                threshold_seconds, ..
            } => Some(*threshold_seconds),
            Self::Baseline {
                threshold_seconds, ..
            }
            | Self::Changed {
                threshold_seconds, ..
            } => Some(*threshold_seconds),
            Self::Inactive => None,
        }
    }
}

macro_rules! snapshot_auto_test_barrier {
    ($control:expr, $field:ident) => {{
        #[cfg(debug_assertions)]
        {
            $control
                .as_ref()
                .and_then(|control| control.cli.$field.as_deref())
        }
        #[cfg(not(debug_assertions))]
        {
            None::<&Path>
        }
    }};
}

pub(crate) fn run_snapshot_auto(ctx: &Invocation<'_>) -> Result<Value> {
    let adapter_settings = ctx.settings;

    #[cfg(debug_assertions)]
    let test_control = Some(ctx.control.debug().clone());
    let repo = ctx.open_read_only_repository()?;
    let session = GcSweepSession::bind_repository(&repo)?;
    let first_gc = session.automation_binding()?;
    let first_config = session.snapshot_auto_binding()?;
    let second_gc = session.automation_binding()?;
    let second_config = session.snapshot_auto_binding()?;
    if first_gc != second_gc || first_config != second_config {
        return Err(snapshot_auto_authority_changed(
            "scheduled snapshot configuration changed during observation",
        ));
    }
    if !first_config.config.is_some_and(|config| config.enabled) {
        if session.has_pending_snapshot_auto_state_exchange()? {
            return Err(snapshot_auto_authority_changed(
                "scheduled snapshot is disabled while checkpoint recovery is pending",
            ));
        }
        return Ok(snapshot_auto_output(
            "skipped",
            "disabled",
            "not_started",
            None,
            None,
            None,
            false,
        ));
    }
    // A Windows checkpoint exchange can temporarily remove its public name.
    // Recover that exact pending operation under its own writer barrier before
    // GC preflight or eligibility reads; absence must never mean a fresh run.
    if session.has_pending_snapshot_auto_state_exchange()? {
        let _lock = session.acquire_snapshot_auto_recovery_lock()?;
        if session.snapshot_auto_binding()? != first_config
            || session.automation_binding()? != first_gc
        {
            return Err(snapshot_auto_authority_changed(
                "scheduled snapshot authority changed before checkpoint recovery",
            ));
        }
        session.recover_snapshot_auto_state_under_lock(&first_config, &first_gc)?;
        if session.snapshot_auto_binding()? != first_config
            || session.automation_binding()? != first_gc
        {
            return Err(snapshot_auto_authority_changed(
                "scheduled snapshot authority changed during checkpoint recovery",
            ));
        }
    }
    let first_indexed = scheduled_auto_index_metadata(&session)?.is_some();
    let second_indexed = scheduled_auto_index_metadata(&session)?.is_some();
    if first_indexed != second_indexed
        || session.automation_binding()? != first_gc
        || session.snapshot_auto_binding()? != first_config
    {
        return Err(snapshot_auto_authority_changed(
            "scheduled snapshot index authority changed during observation",
        ));
    }
    if !first_indexed {
        return Ok(snapshot_auto_output(
            "skipped",
            "not_indexed",
            "not_started",
            None,
            None,
            None,
            false,
        ));
    }
    wait_at_snapshot_auto_test_barrier(snapshot_auto_test_barrier!(
        test_control,
        snapshot_pre_gc_preflight_ready
    ));
    let (recovered_gc, gc_binding) = match gc::preflight_automatic_bound(
        &session,
        &first_gc,
        Some(&first_config),
        "snapshot_auto",
    )? {
        gc::AutomaticGcPreflight::Proceed { recovered, binding } => (recovered, binding),
        gc::AutomaticGcPreflight::Deferred(report) => {
            let mut output = snapshot_auto_output(
                "deferred",
                "recovery_pending",
                "not_started",
                None,
                None,
                Some(report),
                true,
            );
            output
                .as_object_mut()
                .expect("snapshot output object")
                .insert("error_code".to_owned(), json!("KIO-E-GC-RUNTIME-LIMIT-001"));
            set_exit_override(&mut output, ExitCode::PartialFailure);
            return Ok(output);
        }
    };
    if gc_binding != first_gc
        || session.automation_binding()? != first_gc
        || session.snapshot_auto_binding()? != first_config
        || scheduled_auto_index_metadata(&session)?.is_some() != first_indexed
    {
        return Err(snapshot_auto_authority_changed(
            "scheduled snapshot authority changed during automatic GC preflight",
        ));
    }
    let now = ctx.control.now_utc_seconds();
    let initial = observe_scheduled_auto(adapter_settings, &repo, &session, &now)?;
    if initial.gc != gc_binding {
        return Err(snapshot_auto_authority_changed(
            "GC automation changed after scheduled preflight",
        ));
    }
    let confirmed = observe_scheduled_auto(adapter_settings, &repo, &session, &now)?;
    if confirmed != initial {
        return Err(snapshot_auto_authority_changed(
            "scheduled snapshot authority changed during observation",
        ));
    }
    let on_idle = initial.gc.config.mode == GcAutomationMode::OnIdle;
    let idle_eligible = matches!(initial.idle, ScheduledIdleObservation::Eligible { .. });
    if !initial.eligible && !initial.idle.needs_state_publication() && !idle_eligible {
        let (status, reason) = if on_idle {
            ("not_idle", "idle_threshold_not_reached")
        } else {
            ("skipped", "not_eligible")
        };
        return Ok(snapshot_auto_output(
            status,
            reason,
            "not_started",
            Some(&initial),
            None,
            None,
            recovered_gc,
        ));
    }
    wait_at_snapshot_auto_test_barrier(snapshot_auto_test_barrier!(
        test_control,
        snapshot_prelock_ready
    ));
    let lock = session.acquire_snapshot_auto_store_lock()?;
    let bound_repo = Repository::open_bound_in_session(
        repo.canonical_root().to_path_buf(),
        session.retained_scope_handle()?,
        session.retained_kio_handle()?,
        ctx.control.clone(),
        ctx.locks,
    )?;
    let locked = observe_scheduled_auto(adapter_settings, &bound_repo, &session, &now)?;
    if locked != initial || locked.gc != gc_binding {
        return Err(snapshot_auto_authority_changed(
            "scheduled snapshot authority changed during locked eligibility handoff",
        ));
    }
    wait_at_snapshot_auto_test_barrier(snapshot_auto_test_barrier!(
        test_control,
        snapshot_locked_ready
    ));
    let final_observation = observe_scheduled_auto(adapter_settings, &bound_repo, &session, &now)?;
    if final_observation != locked || final_observation.gc != gc_binding {
        return Err(snapshot_auto_authority_changed(
            "scheduled snapshot authority changed immediately before publication",
        ));
    }
    wait_at_snapshot_auto_test_barrier(snapshot_auto_test_barrier!(
        test_control,
        snapshot_before_publication_ready
    ));
    let publication_observation =
        observe_scheduled_auto(adapter_settings, &bound_repo, &session, &now)?;
    if publication_observation != final_observation || publication_observation.gc != gc_binding {
        return Err(snapshot_auto_authority_changed(
            "scheduled snapshot authority changed at publication boundary",
        ));
    }
    let mut state_published = false;
    let outcome = if publication_observation.eligible {
        let excluded = scheduled_auto_excluded(&publication_observation.preview);
        let allowed = explicitly_allowed_tier_a_paths(&publication_observation.preview);
        let expected_raw_by_path = scheduled_auto_expected_raw(&publication_observation.preview)?;
        session.prepare_snapshot_auto_state_publication(&publication_observation.state)?;
        session.assert_public_identity()?;
        wait_at_snapshot_auto_test_barrier(snapshot_auto_test_barrier!(
            test_control,
            snapshot_writer_boundary_ready
        ));
        session.assert_public_identity()?;
        // This closure runs only after immutable snapshot preparation and
        // before any scheduled ref/manifest publication. A failed conditional
        // state CAS therefore leaves the candidate commit unreachable.
        let mut publish_checkpoint = || {
            session.publish_snapshot_auto_state(
                &publication_observation.state,
                &now,
                &publication_observation.working_set_digest,
                true,
            )
        };
        let outcome = bound_repo.scheduled_auto_snapshot(
            &now,
            &excluded,
            &allowed,
            &expected_raw_by_path,
            &publication_observation.direct_entries,
            &publication_observation.config,
            &mut publish_checkpoint,
        )?;
        state_published = true;
        if let Some(commit_hash) = outcome.commit_hash.as_deref() {
            mark_replica_rebuilding_or_log(ctx, bound_repo.kio_dir(), commit_hash);
            ctx.control.append_event_log_in_session(
                ctx.locks,
                "KIO-I-COMMIT-CREATED-001",
                "commit created",
                json!({
                    "commit_hash": commit_hash,
                    "tree_hash": outcome.tree_hash,
                    "trigger": "snapshot_auto",
                }),
            )?;
        }
        Some(outcome)
    } else if publication_observation.idle.needs_state_publication() {
        session.prepare_snapshot_auto_state_publication(&publication_observation.state)?;
        if session.automation_binding()? != publication_observation.gc
            || session.snapshot_auto_binding()? != publication_observation.config
        {
            return Err(snapshot_auto_authority_changed(
                "scheduled snapshot authority changed before idle state publication",
            ));
        }
        session.publish_snapshot_auto_state(
            &publication_observation.state,
            &now,
            &publication_observation.working_set_digest,
            false,
        )?;
        if session.automation_binding()? != publication_observation.gc
            || session.snapshot_auto_binding()? != publication_observation.config
        {
            return Err(snapshot_auto_authority_changed(
                "scheduled snapshot authority changed during idle state publication",
            ));
        }
        state_published = true;
        None
    } else {
        None
    };
    drop(lock);
    let publication_status = if state_published || outcome.is_some() {
        "completed"
    } else {
        "not_started"
    };

    match &publication_observation.idle {
        ScheduledIdleObservation::Baseline { .. } => Ok(snapshot_auto_output(
            "baseline_recorded",
            "first_observation",
            publication_status,
            Some(&publication_observation),
            outcome.as_ref(),
            Some(json!({"status":"skipped","reason":"baseline_recorded","mode":"on_idle"})),
            recovered_gc,
        )),
        ScheduledIdleObservation::Changed { .. } => Ok(snapshot_auto_output(
            "not_idle",
            "working_set_changed",
            publication_status,
            Some(&publication_observation),
            outcome.as_ref(),
            Some(json!({"status":"skipped","reason":"working_set_changed","mode":"on_idle"})),
            recovered_gc,
        )),
        ScheduledIdleObservation::Waiting { .. } => Ok(snapshot_auto_output(
            "not_idle",
            "idle_threshold_not_reached",
            publication_status,
            Some(&publication_observation),
            outcome.as_ref(),
            Some(
                json!({"status":"skipped","reason":"idle_threshold_not_reached","mode":"on_idle"}),
            ),
            recovered_gc,
        )),
        ScheduledIdleObservation::Eligible { .. } => {
            let gc = match gc::run_on_idle_after_snapshot_bound(
                &session,
                &gc_binding,
                &publication_observation.config,
                &publication_observation.index,
                recovered_gc,
            ) {
                Ok(report) => report,
                Err(error) => {
                    let recovery_pending = session
                        .read_marker()
                        .map(|marker| marker.is_some())
                        .unwrap_or(true);
                    let mut output = snapshot_auto_output(
                        if publication_status == "completed" {
                            "completed"
                        } else {
                            "skipped"
                        },
                        "gc_failed",
                        publication_status,
                        Some(&publication_observation),
                        outcome.as_ref(),
                        Some(json!({
                            "status": "failed",
                            "reason": "gc_failed",
                            "mode": "on_idle",
                            "recovery_pending": recovery_pending,
                            "error": error.to_error_json(),
                        })),
                        recovered_gc,
                    );
                    output
                        .as_object_mut()
                        .expect("snapshot output object")
                        .insert("error_code".to_owned(), json!(error.error_code()));
                    let exit = if error.exit_code() == ExitCode::PermanentFailure {
                        ExitCode::PermanentFailure
                    } else {
                        ExitCode::PartialFailure
                    };
                    set_exit_override(&mut output, exit);
                    return Ok(output);
                }
            };
            let deferred = gc.get("status").and_then(Value::as_str) == Some("deferred");
            let no_candidates = gc.get("reason").and_then(Value::as_str) == Some("no_candidates");
            let snapshot_created = outcome.as_ref().is_some_and(|value| !value.noop);
            let (status, reason) = if deferred {
                ("deferred", "max_runtime_seconds")
            } else if no_candidates && !snapshot_created {
                ("noop", "no_gc_candidates")
            } else {
                ("completed", "idle_gc_completed")
            };
            let mut output = snapshot_auto_output(
                status,
                reason,
                publication_status,
                Some(&publication_observation),
                outcome.as_ref(),
                Some(gc),
                recovered_gc,
            );
            if deferred {
                output
                    .as_object_mut()
                    .expect("snapshot output object")
                    .insert("error_code".to_owned(), json!("KIO-E-GC-RUNTIME-LIMIT-001"));
                set_exit_override(&mut output, ExitCode::PartialFailure);
            }
            Ok(output)
        }
        ScheduledIdleObservation::Inactive => {
            let (status, reason) = match outcome.as_ref() {
                Some(outcome) if outcome.noop => ("noop", "tree_and_tool_lock_unchanged"),
                Some(_) => ("completed", "snapshot_created"),
                None => ("skipped", "not_eligible"),
            };
            Ok(snapshot_auto_output(
                status,
                reason,
                publication_status,
                Some(&publication_observation),
                outcome.as_ref(),
                None,
                recovered_gc,
            ))
        }
    }
}

fn snapshot_auto_output(
    status: &str,
    reason: &str,
    publication_status: &str,
    observation: Option<&ScheduledAutoObservation>,
    outcome: Option<&SnapshotOutcome>,
    gc: Option<Value>,
    recovered_gc: bool,
) -> Value {
    let idle = observation.map(|value| &value.idle);
    let recovery_pending = gc
        .as_ref()
        .and_then(|value| value.get("recovery_pending"))
        .and_then(Value::as_bool)
        .unwrap_or(status == "deferred");
    json!({
        "operation": "snapshot_auto",
        "status": status,
        "reason": reason,
        "publication_status": publication_status,
        "snapshot_status": match outcome {
            Some(value) if value.noop => "noop",
            Some(_) => "completed",
            None => "not_started",
        },
        "eligibility_reason": observation.and_then(|value| value.eligibility_reason),
        "eligible": observation.is_some_and(|value| value.eligible),
        "change_count": observation.map(|value| value.change_count),
        "next_eligible_at": observation.and_then(|value| value.next_eligible_at.as_deref()),
        "commit_hash": outcome.and_then(|value| value.commit_hash.as_deref()),
        "tree_hash": outcome.map(|value| value.tree_hash.as_str()),
        "stats": outcome.map(|value| json!({
            "files_added": value.stats.files_added,
            "files_modified": value.stats.files_modified,
            "files_deleted": value.stats.files_deleted,
        })),
        "working_set_digest": observation.map(|value| value.working_set_digest.as_str()),
        "idle_observed_since": idle.and_then(|value| value.observed_since()),
        "idle_observed_seconds": idle.and_then(|value| value.observed_seconds()),
        "idle_threshold_seconds": idle.and_then(|value| value.threshold_seconds()),
        "idle_eligible": idle.is_some_and(|value| matches!(value, ScheduledIdleObservation::Eligible { .. })),
        "recovered_gc": recovered_gc,
        "recovery_pending": recovery_pending,
        "gc": gc,
    })
}

fn snapshot_auto_authority_changed(message: &str) -> KioError {
    KioError::new(
        "KIO-E-SNAPSHOT-AUTHORITY-CHANGED-001",
        message,
        json!({}),
        ExitCode::PartialFailure,
    )
}

fn wait_at_snapshot_auto_test_barrier(ready_path: Option<&Path>) {
    let Some(ready_path) = ready_path else {
        return;
    };
    if fs::write(ready_path, b"ready").is_err() {
        return;
    }
    let release_path = ready_path.with_extension("release");
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while !release_path.exists() && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn observe_scheduled_auto(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    session: &GcSweepSession,
    now: &str,
) -> Result<ScheduledAutoObservation> {
    session.assert_public_identity()?;
    let gc = session.automation_binding()?;
    let config = session.snapshot_auto_binding()?;
    let config_value = config
        .config
        .ok_or_else(|| KioError::schema("scheduled auto is disabled"))?;
    if !config_value.enabled {
        return Err(KioError::schema("scheduled auto authority became disabled"));
    }
    let index = scheduled_auto_index_metadata(session)?
        .ok_or_else(|| KioError::schema("scheduled auto index disappeared during observation"))?;
    let direct_entries = session.validate_snapshot_auto_direct_entries()?;
    let state = session.snapshot_auto_state()?;
    let now_seconds = parse_utc_seconds(now)
        .filter(|seconds| kio_core::scope::format_utc_seconds(*seconds) == now)
        .ok_or_else(|| snapshot_auto_clock_error("current time is not canonical UTC seconds"))?;
    let (interval_elapsed, prior_next) = match &state.state {
        Some(state) => {
            let last =
                parse_utc_seconds(&state.last_successful_eligible_attempt_at).ok_or_else(|| {
                    snapshot_auto_clock_error("scheduler state time is not canonical UTC seconds")
                })?;
            if now_seconds < last {
                return Err(snapshot_auto_clock_error(
                    "scheduled snapshot clock moved backwards",
                ));
            }
            let next = scheduled_auto_add_interval(last, config_value.interval_seconds)?;
            (
                now_seconds >= next,
                Some(kio_core::scope::format_utc_seconds(next)),
            )
        }
        None => (true, None),
    };
    let root = session.retained_scope_handle()?;
    let kio = session.retained_kio_handle()?;
    let preview = build_bound_scan_preview(
        &adapter_settings.declared_pricing("markdown"),
        &adapter_settings.declared_pricing("embedding"),
        &root,
        &kio,
        ScanPreviewRequest {
            scope_path: repo.canonical_root().display().to_string(),
            include_raw_hashes: true,
            require_network_approval: false,
        },
        &[],
    )
    .map_err(pipeline_to_kio)?;
    let working_set_digest = scheduled_working_set_digest(&preview)?;
    let idle = if gc.config.mode == GcAutomationMode::OnIdle {
        let threshold_seconds = gc
            .config
            .idle_threshold_seconds
            .ok_or_else(|| KioError::schema("gc.mode=on_idle requires idle_threshold_seconds"))?;
        match state.state.as_ref() {
            None => ScheduledIdleObservation::Baseline {
                observed_since: now.to_owned(),
                threshold_seconds,
            },
            Some(previous) if previous.working_set_digest != working_set_digest => {
                ScheduledIdleObservation::Changed {
                    observed_since: now.to_owned(),
                    threshold_seconds,
                }
            }
            Some(previous) => {
                let observed_since = parse_utc_seconds(&previous.idle_observed_since)
                    .filter(|seconds| {
                        kio_core::scope::format_utc_seconds(*seconds)
                            == previous.idle_observed_since
                    })
                    .ok_or_else(|| {
                        snapshot_auto_clock_error(
                            "scheduler idle observation time is not canonical UTC seconds",
                        )
                    })?;
                if now_seconds < observed_since {
                    return Err(snapshot_auto_clock_error(
                        "scheduled snapshot idle clock moved backwards",
                    ));
                }
                let observed_seconds =
                    u64::try_from(now_seconds - observed_since).map_err(|_| {
                        snapshot_auto_clock_error("scheduled idle duration is outside u64 range")
                    })?;
                if observed_seconds >= threshold_seconds {
                    ScheduledIdleObservation::Eligible {
                        observed_since: previous.idle_observed_since.clone(),
                        observed_seconds,
                        threshold_seconds,
                    }
                } else {
                    ScheduledIdleObservation::Waiting {
                        observed_since: previous.idle_observed_since.clone(),
                        observed_seconds,
                        threshold_seconds,
                    }
                }
            }
        }
    } else {
        ScheduledIdleObservation::Inactive
    };
    let change_count = scheduled_preview_change_count(repo, session, &preview)?;
    let threshold = change_count >= config_value.on_change_threshold;
    let (eligible, eligibility_reason) = match (state.state.is_none(), interval_elapsed, threshold)
    {
        (true, _, _) => (true, Some("first_run")),
        (_, true, true) => (true, Some("interval_and_change_threshold")),
        (_, true, false) => (true, Some("interval_elapsed")),
        (_, false, true) => (true, Some("change_threshold")),
        _ => (false, None),
    };
    session.assert_public_identity()?;
    Ok(ScheduledAutoObservation {
        gc,
        config,
        state,
        direct_entries,
        preview,
        index,
        change_count,
        eligible,
        eligibility_reason,
        next_eligible_at: if eligible {
            Some(kio_core::scope::format_utc_seconds(
                scheduled_auto_add_interval(now_seconds, config_value.interval_seconds)?,
            ))
        } else {
            prior_next
        },
        working_set_digest,
        idle,
    })
}

fn scheduled_auto_add_interval(at: i64, interval: u64) -> Result<i64> {
    at.checked_add(i64::try_from(interval).map_err(|_| {
        snapshot_auto_clock_error("scheduled interval is outside signed timestamp range")
    })?)
    .ok_or_else(|| snapshot_auto_clock_error("scheduled interval overflow"))
}

fn scheduled_auto_excluded(preview: &ScanPreview) -> BTreeSet<String> {
    let mut excluded = preview
        .candidates
        .iter()
        .filter(|candidate| candidate.ignored)
        .map(|candidate| candidate.input_path.clone())
        .collect::<BTreeSet<_>>();
    // The ignore policy file is scheduler control, not scope content. The
    // bound preview intentionally omits it, so exclude it explicitly from the
    // lower-level snapshot candidate enumeration too.
    excluded.insert(".kioignore".to_owned());
    excluded
}

fn scheduled_auto_expected_raw(preview: &ScanPreview) -> Result<BTreeMap<String, String>> {
    preview
        .candidates
        .iter()
        .filter(|candidate| !candidate.ignored)
        .map(|candidate| {
            Ok((
                candidate.input_path.clone(),
                candidate
                    .raw_hash
                    .clone()
                    .ok_or_else(|| KioError::schema("bound scheduled scan omitted a raw hash"))?,
            ))
        })
        .collect()
}

fn scheduled_working_set_digest(preview: &ScanPreview) -> Result<String> {
    let entries = scheduled_auto_expected_raw(preview)?;
    let bytes = canonical_json_bytes(
        &serde_json::to_value(entries).map_err(|error| KioError::schema(error.to_string()))?,
    )?;
    Ok(format!(
        "sha256:{}",
        kio_core::cas::lower_hex(&Sha256::digest(bytes))
    ))
}

fn scheduled_auto_index_metadata(session: &GcSweepSession) -> Result<Option<BoundGcIndexMetadata>> {
    session.assert_public_identity()?;
    let kio = session.retained_kio_handle()?;
    let metadata = read_bound_gc_index_metadata(
        &kio,
        &FtsSchemaConfig {
            tokenizer: FtsTokenizer::Trigram,
        },
    )
    .map_err(index_to_kio)?;
    session.assert_public_identity()?;
    Ok(metadata)
}

fn scheduled_preview_change_count(
    repo: &Repository,
    session: &GcSweepSession,
    preview: &ScanPreview,
) -> Result<u64> {
    session.assert_public_identity()?;
    let head = repo.head_commit_hash()?;
    let head_tree = head
        .as_deref()
        .map(|hash| {
            repo.read_commit(hash)
                .and_then(|commit| repo.read_tree(&commit.tree))
        })
        .transpose()?;
    let old = head_tree
        .as_ref()
        .map(|tree| {
            tree.entries
                .iter()
                .map(|e| (&e.path, &e.raw_hash))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let new = preview
        .candidates
        .iter()
        .filter(|candidate| !candidate.ignored)
        .map(|candidate| {
            Ok((
                &candidate.input_path,
                candidate
                    .raw_hash
                    .as_ref()
                    .ok_or_else(|| KioError::schema("bound scheduled scan omitted a raw hash"))?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let changes = old
        .keys()
        .chain(new.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| old.get(*path) != new.get(*path))
        .count() as u64;
    session.assert_public_identity()?;
    Ok(changes)
}
