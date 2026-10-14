use crate::approvals;
use crate::commands::IndexArgs;
use crate::commands::ReindexArgs;
use crate::context::EvidenceRegistry;
use crate::discovery;
use crate::egress::active_markdownize_profile_for;
use crate::egress::embedding_online_allowed;
use crate::egress::markdown_output_ref_for_profile;
use crate::egress::materialize_tool_lock;
use crate::egress::persistent_network_allowed;
use crate::egress::record_quarantine_candidates;
use crate::egress::release_markdownize_secret_holds;
use crate::egress::secrets_send_approved;
use crate::egress::write_approval_record;
use crate::embedding::enrichment::generate_scope_embeddings;
use crate::embedding::enrichment::run_embedding_enrichment;
use crate::embedding::transport::LaneOverride;
use crate::embedding::transport::poll_batch_embedding_jobs;
use crate::embedding::transport::resolve_invocation_lane;
use crate::gc;
use crate::historical_reindex;
use crate::index::chunk_ledger::ensure_purge_epoch_initialized;
use crate::index::chunk_ledger::recover_index_generation;
use crate::index::pipeline::explicitly_allowed_tier_a_paths;
use crate::index::pipeline::index_preview_response;
use crate::index::pipeline::run_index_pipeline;
use crate::index::rebuild::is_rebuild_skippable_unit_error;
use crate::index::rebuild::read_head_tree_for_rebuild;
use crate::index::rebuild::rebuild_step3_index;
use crate::index::rebuild::skipped_disclosure;
use crate::invocation::Invocation;
use crate::invocation::merged_index_completion;
use crate::ledger_access::RETIRED_NON_LIVE;
use crate::ledger_access::open_optional_ledger_db;
use crate::ledger_access::recover_stale_sync_rows;
use crate::management;
use crate::management::ChildScope;
use crate::management::binding_for_repo;
use crate::management::open_existing_managed_root;
use crate::management::reconcile_planned_child;
use crate::markdownize::batch::poll_batch_markdownize_jobs;
use crate::object_persistence::compute_manifest_hash;
use crate::object_persistence::copy_normalized_instance_gen;
use crate::object_persistence::pipeline_to_kio;
use crate::promotion::apply_online_promotion_to_index;
use crate::promotion::finish_pending_online_promotion;
use crate::promotion::recover_pending_online_promotion;
use crate::response::gc::{GcPreflightRecovered, GcReport};
use crate::response::index::{
    ChildPreviewRow, ChildPublishedRow, IndexPublishedResponse, IndexResponse,
    PreviewChildDiscovery, PublishedChildDiscovery,
};
use crate::response::reindex::{HeadReindexResponse, ReindexExecution, ReindexResponse};
use crate::response::search::FileDispositionResponse;
use crate::response::search::IndexStatusError;
use crate::response::search::IndexStatusResponse;
use crate::response::search::OfficeConversionUnavailableResponse;
use crate::response::skipped_units::SkippedUnit;
use crate::response::{CommandResponse, Completion, ResponseBody};
use crate::scope_access::ensure_raw_ingest_allowed;
use crate::scope_access::register_scope;
use crate::scope_access::resolve_evidence_scope_target;
use crate::scope_access::scope_id;
use crate::search::projection::mark_replica_rebuilding_or_log;
use crate::search::projection::maybe_inject_replica_after_head_fault;
use crate::search::scope::SearchedScopeInfo;
use crate::search::scope::current_embedding_policy;
use crate::settings::user_config_toml_path;
use crate::settings::validate_repo_tool_lock;
use crate::task_execution::enrichment_completion;
use crate::task_execution::scope_budget_warning;
use crate::task_execution::task_retry_allowed;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_core::ExitCode;
use kio_core::KioError;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::dag::NormalizeRef;
use kio_core::scope::PendingNormalizeRef;
use kio_core::scope::Repository;
use kio_pipeline::budget::read_budget_policy;
use kio_pipeline::ledger::LedgerReadSnapshot;
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::scan::CHILD_SCOPE_DISCOVERY_PAGE_SIZE;
use kio_pipeline::scan::ScanPreview;
use kio_pipeline::scan::ScanPreviewRequest;
use kio_pipeline::scan::bind_planned_child;
use kio_pipeline::scan::build_managed_scan_preview;
use kio_pipeline::scan::build_planned_child_scan_preview;
use kio_pipeline::scan::build_scan_preview;
use kio_pipeline::scan::discover_managed_child_scopes_page;
use kio_pipeline::scan::managed_scan_policy_allows_file;
use kio_pipeline::scan::read_bound_verified_scan_input;
use kio_pipeline::task::TaskStatus;
use kio_pipeline::task::TaskStore;
use kio_pipeline::task::TaskType;
use kio_pipeline::unsupported::UNSUPPORTED_REASON_UNRECOGNIZED_BINARY;
use kio_pipeline::unsupported::UnsupportedInputDisposition;
use kio_pipeline::unsupported::UnsupportedInputStore;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

/// 07-adapter-spec.md §5.1: `UnsupportedInputDisposition.reason` recorded for an
/// Office (DOCX/PPTX) file when no converter (`KIO_OFFICE_CONVERTER` / PATH
/// `soffice`) resolves at `kio index` time -- no online task is enqueued for it
/// (no doomed task), and `compute_index_status`'s `office_conversion_unavailable`
/// surfaces it. Shares the `UnsupportedInputStore` (`UNSUPPORTED_INPUTS_FILE`)
/// used for `UNSUPPORTED_REASON_UNRECOGNIZED_BINARY` -- `reason` is a free-form
/// `String`, not a closed enum, so this is a plain sibling value, not a change to
/// `kio-pipeline`.
pub(crate) const OFFICE_CONVERSION_UNAVAILABLE_REASON: &str = "office_conversion_unavailable";

pub(crate) fn run_index(ctx: &Invocation<'_>, args: IndexArgs) -> Result<CommandResponse> {
    // `index --preview` is read-only all the way through opening the repository:
    // the ordinary open path may repair a missing HEAD, which would violate the
    // preview no-state-creation contract before scanning.
    let repo = open_existing_managed_root(ctx.cwd(), ctx.control)?.in_lock_session(ctx.locks);
    let execution = run_index_for_repo(ctx, args, &repo, true)?;
    Ok(CommandResponse {
        body: ResponseBody::Index(execution.response),
        completion: execution.completion,
    })
}

struct IndexExecution {
    response: IndexResponse,
    completion: Completion,
}

/// Index one scope. Only the user-invoked parent discovers children; child
/// scopes stay direct-file-only and cannot recursively fan out a single run.
fn run_index_for_repo(
    ctx: &Invocation<'_>,
    args: IndexArgs,
    repo: &Repository,
    discover_children: bool,
) -> Result<IndexExecution> {
    run_index_for_repo_inner(ctx, args, repo, discover_children)
}

struct ChildDiscoveryPage {
    slice: kio_pipeline::scan::ChildScopeDiscoverySlice,
    frontier: Option<discovery::ChildDiscoveryFrontier>,
    restarted_from_root: bool,
}

fn discover_child_page(
    repo: &Repository,
    frontier: Option<&discovery::ChildDiscoveryFrontier>,
) -> Result<ChildDiscoveryPage> {
    let binding = binding_for_repo(repo)?;
    let continuation = frontier
        .map(discovery::ChildDiscoveryFrontier::load)
        .transpose()?
        .flatten();
    match discover_managed_child_scopes_page(&binding, continuation.as_ref()) {
        Ok(slice) => Ok(ChildDiscoveryPage {
            slice,
            frontier: None,
            restarted_from_root: false,
        }),
        Err(kio_pipeline::PipelineError::Contract {
            code: "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            ..
        }) if frontier.is_some() => {
            // A stale cursor never drops the unfinished page. Clear it while
            // holding the frontier lease, then reconcile again from the root.
            frontier.expect("guarded frontier").commit(None)?;
            let slice =
                discover_managed_child_scopes_page(&binding, None).map_err(pipeline_to_kio)?;
            Ok(ChildDiscoveryPage {
                slice,
                frontier: None,
                restarted_from_root: true,
            })
        }
        Err(error) => Err(pipeline_to_kio(error)),
    }
}

/// Admit the scope itself separately from ordinary per-file ignore filtering.
/// A denied child must never publish an empty tree or start writer recovery.
fn admit_scope_writer(repo: &Repository) -> Result<CurrentPolicyEvaluator> {
    let policy = current_embedding_policy(repo)?;
    if !policy.allows_scope().map_err(pipeline_to_kio)? {
        return Err(pipeline_to_kio(kio_pipeline::PipelineError::contract(
            "KIO-E-POLICY-SCOPE-DENIED-001",
            "managed scope is denied by current ancestor policy",
        )));
    }
    Ok(policy)
}

fn run_index_for_repo_inner(
    ctx: &Invocation<'_>,
    args: IndexArgs,
    repo: &Repository,
    discover_children: bool,
) -> Result<IndexExecution> {
    let adapter_settings = ctx.settings;

    if args.online && args.offline {
        return Err(KioError::invalid_usage(
            "--online and --offline are mutually exclusive",
        ));
    }
    // Shared by explicit invocations and retained internal child runs, including
    // preview. Admission precedes GC, writer acquisition and recovery writes.
    let admission_policy = admit_scope_writer(repo)?;
    // Scope identity conflicts matter to local publication and replication as
    // well as paid sends. Check an owned read-only registry snapshot before
    // GC recovery, grants or any index mutation can begin.
    if !args.preview {
        let scope_id = repo.scope_identity()?.scope_id;
        resolve_evidence_scope_target(
            ctx.cwd(),
            &EvidenceRegistry::default(),
            &scope_id,
            Some(&repo.canonical_root().display().to_string()),
            ctx.control,
        )?;
    }
    let lane_override = LaneOverride::new(args.realtime, args.batch);
    let mut child_discovery = if discover_children && args.preview {
        Some(discover_child_page(repo, None)?)
    } else {
        None
    };
    // Resolve the invocation lane BEFORE anything plans or prices a send: the
    // OCR lane selection and both reservation estimates read it, and by ruling
    // OCR and embedding must never end up on different lanes.
    resolve_invocation_lane(
        lane_override,
        Some(&repo.kio_dir().join("config.toml")),
        ctx.control,
    )?;
    // Preview and network-revoke are not successful index publications, so
    // they never activate automatic GC.  A real index first resumes any
    // after_index marker; if the bounded slice expires, return before taking
    // the writer lock so the next invocation can continue recovery.
    let (recovered_before_publication, gc_binding) = if args.preview {
        (false, None)
    } else {
        match gc::preflight_automatic(repo, "index")? {
            gc::AutomaticGcPreflight::Proceed { recovered, binding } => (recovered, Some(binding)),
            gc::AutomaticGcPreflight::Deferred(report) => {
                return Ok(IndexExecution {
                    response: IndexResponse::Deferred(gc::deferred_before_publication(
                        "index", *report,
                    )),
                    completion: Completion::Partial,
                });
            }
        }
    };
    if args.preview {
        // Preview is an observational operation: taking the writer lock itself
        // materializes `.kio/.lock`, so construct its scan before any store
        // writer is acquired.  This stays descriptor-bound for internal child
        // scopes through `build_repository_scan_preview`.
        validate_repo_tool_lock(repo)?;
        let preview = build_repository_scan_preview(
            adapter_settings,
            repo,
            ScanPreviewRequest {
                scope_path: repo.root().display().to_string(),
                include_raw_hashes: false,
                require_network_approval: !args.offline,
            },
        )
        .map_err(pipeline_to_kio)?;
        let mut output = index_preview_response(repo.canonical_root(), &preview);
        if let Some(discovery) = child_discovery.as_ref() {
            let plan = &discovery.slice.plan;
            let mut children = Vec::new();
            let mut aggregate_files = preview
                .candidates
                .iter()
                .filter(|candidate| !candidate.ignored)
                .count();
            let mut aggregate_bytes = preview
                .candidates
                .iter()
                .filter(|candidate| !candidate.ignored)
                .map(|candidate| candidate.size_bytes)
                .sum::<u64>();
            let mut aggregate_cost = preview
                .estimated_cost
                .as_ref()
                .map(|cost| cost.estimated_usd)
                .unwrap_or_default();
            let parent_binding = binding_for_repo(repo)?;
            let parent_case = kio_core::management::read_record(&parent_binding)?.case_insensitive;
            for child in plan.candidates.clone() {
                let mut row = ChildPreviewRow {
                    discovery: child.clone(),
                    estimated_file_count: None,
                    estimated_size_bytes: None,
                    estimated_cost_usd: None,
                };
                if child.status == "planned" {
                    let Some(bound) =
                        bind_planned_child(plan, &child.path).map_err(pipeline_to_kio)?
                    else {
                        row.discovery.status = "skipped_vcs".to_owned();
                        row.discovery.reason = Some("vcs_marker_added_after_discovery".to_owned());
                        children.push(row);
                        continue;
                    };
                    let child_preview = build_planned_child_scan_preview(
                        &adapter_settings.declared_pricing("markdown"),
                        &adapter_settings.declared_pricing("embedding"),
                        &bound,
                        ScanPreviewRequest {
                            scope_path: child.path.clone(),
                            include_raw_hashes: false,
                            require_network_approval: !args.offline,
                        },
                        parent_case,
                    )
                    .map_err(pipeline_to_kio)?;
                    let included = child_preview
                        .candidates
                        .iter()
                        .filter(|candidate| !candidate.ignored)
                        .collect::<Vec<_>>();
                    let count = included.len();
                    let bytes = included
                        .iter()
                        .map(|candidate| candidate.size_bytes)
                        .sum::<u64>();
                    let cost = child_preview
                        .estimated_cost
                        .as_ref()
                        .map(|cost| cost.estimated_usd)
                        .unwrap_or_default();
                    row.estimated_file_count = Some(count);
                    row.estimated_size_bytes = Some(bytes);
                    row.estimated_cost_usd = Some(cost);
                    aggregate_files += count;
                    aggregate_bytes += bytes;
                    aggregate_cost += cost;
                }
                children.push(row);
            }
            output.child_scopes = Some(children);
            output.estimated_aggregate_file_count = Some(aggregate_files);
            output.estimated_aggregate_size_bytes = Some(aggregate_bytes);
            output.estimated_aggregate_cost_usd = Some(aggregate_cost);
            output.child_scope_discovery = Some(PreviewChildDiscovery {
                page_size: CHILD_SCOPE_DISCOVERY_PAGE_SIZE,
                has_more: discovery.slice.continuation.is_some(),
                preview: true,
            });
        }
        admission_policy.revalidate().map_err(pipeline_to_kio)?;
        return Ok(IndexExecution {
            response: IndexResponse::Preview(output),
            completion: Completion::Complete,
        });
    }

    // An explicit network request must not silently turn into an offline
    // success. Check before writer recovery, grants, ledgers or task mutation.
    if args.online && !approvals::any_network_allowed(adapter_settings, repo)? {
        return Err(KioError::invalid_usage(
            "online indexing requires an active external-send approval for this scope",
        ));
    }

    // M1(a): serialize every mutating index path against concurrent
    // index/repair/reindex (05 §6).  The preview above intentionally runs
    // without this writer lock; therefore the write path acquires it first and
    // only then constructs a fresh preview, binding approvals and writes to the
    // current tree rather than a stale pre-lock observation. The lock is
    // reentrant, so the internal auto-snapshot re-acquisition does not deadlock.
    let lock = repo.lock_store_in_session(repo.lock_session())?;
    admission_policy.revalidate().map_err(pipeline_to_kio)?;
    management::reconcile_child_lifecycle(repo)?;
    // Lifecycle reconciliation may legitimately change management records.
    // Bind scanning and publication to the resulting live policy snapshot.
    let publication_policy = admit_scope_writer(repo)?;
    validate_repo_tool_lock(repo)?;
    let preview = build_repository_scan_preview(
        adapter_settings,
        repo,
        ScanPreviewRequest {
            scope_path: repo.root().display().to_string(),
            include_raw_hashes: true,
            require_network_approval: !args.offline,
        },
    )
    .map_err(pipeline_to_kio)?;

    // Validated management membership is the continuing local scan grant.
    // External adapter consent is evaluated separately below; absence of a
    // network grant must not prevent local indexing of a newly enrolled child.
    if args.yes {
        write_approval_record(repo, &preview, "yes")?;
    }
    let markdown_profile = active_markdownize_profile_for(adapter_settings, repo)?;
    let secrets_approved = secrets_send_approved(
        adapter_settings,
        repo,
        approvals::AdapterRole::Markdown,
        &markdown_profile.adapter_id,
        &markdown_profile.tool_profile_hash,
    );
    if secrets_approved {
        release_markdownize_secret_holds(
            repo,
            &markdown_output_ref_for_profile(&markdown_profile),
        )?;
    }
    record_quarantine_candidates(adapter_settings, repo, &preview)?;

    if recover_pending_online_promotion(repo)? {
        finish_pending_online_promotion(ctx, repo)?;
    }
    // CL45/item 5: reconcile any stale `request_kind='sync'` cost-ledger.sqlite
    // rows left by a crashed prior run, same write-command-entry point as the
    // online-promotion recovery just above.
    if !args.offline
        && let Some(ledger) = open_optional_ledger_db(ctx.control)?
    {
        recover_stale_sync_rows(
            &ledger,
            Some(&TaskStore::new_with_control(
                repo.kio_dir(),
                repo.control().clone(),
            )),
            &scope_id(repo.kio_dir())?,
        )?;
    }
    // 04 §5.8 相 3: poll/collect this scope's in-flight Batch markdownize jobs
    // at the same write-command entry point (receive/cleanup of existing
    // requests — no new send, no network opt-in needed). Results collected
    // here land as Done tasks that `apply_online_promotion_to_index` below
    // folds into this run's single snapshot.
    if !args.offline
        && let Some(ledger) = open_optional_ledger_db(ctx.control)?
    {
        poll_batch_markdownize_jobs(
            adapter_settings,
            repo,
            &TaskStore::new_with_control(repo.kio_dir(), repo.control().clone()),
            &ledger,
        )?;
        // Offline also forbids polling, downloading or deleting existing jobs.
        poll_batch_embedding_jobs(ctx, repo, &ledger)?;
    }
    // LC39/LC40 (see `ensure_purge_epoch_initialized`'s doc comment).
    ensure_purge_epoch_initialized(repo.kio_dir(), repo.control())?;
    materialize_tool_lock(adapter_settings, repo)?;
    let mut index_result = run_index_pipeline(adapter_settings, repo, &preview, &args)?;
    // A prior batch may already have produced a complete accepted online
    // instance. Overlay it before the one normal index snapshot so an ordinary
    // index neither demotes the file to the deterministic baseline nor creates a
    // second promotion commit.
    apply_online_promotion_to_index(adapter_settings, repo, &mut index_result)?;
    let excluded = preview
        .candidates
        .iter()
        .filter(|candidate| candidate.ignored)
        .map(|candidate| candidate.input_path.clone())
        .collect::<BTreeSet<_>>();
    let explicitly_allowed_tier_a = explicitly_allowed_tier_a_paths(&preview);
    let outcome = repo.auto_snapshot_with_bound_normalize(
        Some("kio index auto snapshot"),
        None,
        &excluded,
        &index_result.normalize_by_path,
        &explicitly_allowed_tier_a,
        Some(&|| publication_policy.revalidate().map_err(pipeline_to_kio)),
    )?;
    if let Some(commit_hash) = &outcome.commit_hash {
        maybe_inject_replica_after_head_fault("index_before_marker", ctx.control)?;
        // HEAD now names content the old source index and replica projection do
        // not describe.  Mark it before the potentially long source rebuild,
        // rather than only at the final SQLite swap, so direct replica-only
        // search cannot serve the prior Ready snapshot in that interval.
        mark_replica_rebuilding_or_log(ctx, repo.kio_dir(), commit_hash);
        maybe_inject_replica_after_head_fault("index", ctx.control)?;
        ctx.control.append_event_log_in_session(
            ctx.locks,
            "KIO-I-COMMIT-CREATED-001",
            "auto commit created",
            json!({
                "commit_hash": commit_hash,
                "tree_hash": outcome.tree_hash,
                "commit_type": "auto",
            }),
        )?;
    }
    let rebuild_report = rebuild_step3_index(ctx, repo)?;
    // LC42-LC44: after the rebuild's temp-build-then-rename lands the final
    // `sqlite.db`, reconcile `index_metadata` (item 2).
    recover_index_generation(repo.kio_dir(), repo.control())?;
    // Generate chunk embeddings behind the online opt-in / budget / cost-ledger
    // guardrails (K4). No-op unless an embedding adapter is configured. R11-2: keep
    // the ExecOutcome (was discarded) so the embedding enrichment result is disclosed
    // and can raise the exit code (auth/budget) instead of a silent exit 0.
    let enrichment = generate_scope_embeddings(ctx, repo, &args)?;
    // The scope now has a search index; register it as indexed so it participates
    // in default multi-scope search (05 §1.8, K3). Cache write, never fatal.
    register_scope(ctx, repo, true);
    let mut output = IndexPublishedResponse {
        status: if outcome.noop { "noop" } else { "indexed" }.to_owned(),
        approval_method: if args.yes { "yes" } else { "existing" }.to_owned(),
        network_allowed: index_result.network_allowed,
        network_opt_in: persistent_network_allowed(adapter_settings, repo)?,
        pending_online_tasks: index_result.pending_online_tasks,
        paused_tasks: index_result.paused_tasks + enrichment.paused,
        failed_files: index_result.failed_files,
        normalized_files: index_result.normalized_files,
        pending_files: index_result.pending_files,
        skipped_oversized_files: index_result.skipped_oversized_files,
        skipped_unrecognized_binary_files: index_result.skipped_unrecognized_binary_files,
        embedding_tasks_executed: enrichment.executed,
        embedding_tasks_failed: enrichment.failed,
        tree_hash: outcome.tree_hash,
        commit_hash: outcome.commit_hash,
        commit: outcome.commit,
        budget_warning: scope_budget_warning(repo)?,
        skipped: skipped_disclosure(&rebuild_report, repo.kio_dir()),
        error_code: (index_result.failed_files > 0).then(|| "KIO-E-INDEX-PARTIAL-001".to_owned()),
        gc: None,
        publication_status: None,
        child_scopes: None,
        child_scope_discovery: None,
    };
    let mut completion = match enrichment_completion(&enrichment) {
        Completion::Complete if index_result.failed_files > 0 => Completion::Partial,
        completion => completion,
    };
    // This scope's publication and projection are complete. Enrollment takes
    // its own retained parent lock, so holding the indexing lock while entering
    // a child would contend with ourselves. No parent content is changed by the
    // child loop; each enrollment revalidates the live management chain.
    drop(lock);
    if discover_children {
        let frontier = discovery::ChildDiscoveryFrontier::acquire(repo)?;
        let mut page = discover_child_page(repo, Some(&frontier))?;
        page.frontier = Some(frontier);
        child_discovery = Some(page);
    }
    if let Some(discovery) = child_discovery {
        let mut page = discovery;
        let frontier = page
            .frontier
            .take()
            .expect("mutating child discovery retains its frontier lease");
        let mut children = Vec::new();
        let mut child_rows_total = 0_u64;
        let mut managed_parents = BTreeMap::<String, Repository>::new();
        let mut child_gc_reports = BTreeMap::<String, GcReport>::new();
        let parent_completion = completion;
        let mut child_completions = Vec::new();
        let mut incomplete_child_discovery = false;
        let mut restart_count = u8::from(page.restarted_from_root);
        let mut continuation_committed = false;
        let has_more = loop {
            let plan = &page.slice.plan;
            let mut page_completed = true;
            for mut child in plan.candidates.clone() {
                if child.status != "planned" {
                    retain_child_discovery_row(&mut children, &mut child_rows_total, child);
                    continue;
                }
                // The bounded discovery plan is sorted parent-first. Enrollment
                // always belongs to the immediate parent, including empty ones.
                let parent_name = child.path.rsplit_once('/').map(|(parent, _)| parent);
                let direct_parent = match parent_name {
                    None => repo,
                    Some(name) => {
                        if !managed_parents.contains_key(name) {
                            match reopen_managed_child_ancestor(repo, name) {
                                Ok(ManagedChildAncestor::Managed(parent)) => {
                                    cache_managed_child_parent(
                                        &mut managed_parents,
                                        name.to_owned(),
                                        *parent,
                                    );
                                }
                                Ok(ManagedChildAncestor::ExcludedByPolicy) => {
                                    child.status = "skipped_ignored".to_owned();
                                    child.reason = Some("current_ancestor_policy".to_owned());
                                    retain_child_discovery_row(
                                        &mut children,
                                        &mut child_rows_total,
                                        child,
                                    );
                                    continue;
                                }
                                Ok(ManagedChildAncestor::IndependentRoot) => {
                                    child.status = "skipped_independent_root".to_owned();
                                    child.reason = Some("independent_nested_root".to_owned());
                                    retain_child_discovery_row(
                                        &mut children,
                                        &mut child_rows_total,
                                        child,
                                    );
                                    continue;
                                }
                                Err(error) => {
                                    page_completed = false;
                                    incomplete_child_discovery = true;
                                    child.status = "skipped_ancestor".to_owned();
                                    child.reason = Some("ancestor_not_managed_by_root".to_owned());
                                    child.error_code = Some(error.error_code().to_owned());
                                    child.message = Some(error.message().to_owned());
                                    retain_child_discovery_row(
                                        &mut children,
                                        &mut child_rows_total,
                                        child,
                                    );
                                    continue;
                                }
                            }
                        }
                        managed_parents
                            .get(name)
                            .expect("managed child ancestor was inserted")
                    }
                };
                match run_bound_child_index(ctx, direct_parent, plan, &child.path, &args) {
                    Ok(BoundChildIndex::SkippedVcs) => {
                        child.status = "skipped_vcs".to_owned();
                        child.reason = Some("vcs_marker_added_after_discovery".to_owned());
                    }
                    Ok(BoundChildIndex::ExcludedIndependentRoot) => {
                        child.status = "skipped_independent_root".to_owned();
                        child.reason = Some("independent_nested_root".to_owned());
                    }
                    Ok(BoundChildIndex::ExcludedByPolicy) => {
                        child.status = "skipped_ignored".to_owned();
                        child.reason = Some("current_parent_policy".to_owned());
                    }
                    Ok(BoundChildIndex::Output(child_output, child_repo))
                        if child_output.completion == Completion::Complete =>
                    {
                        cache_managed_child_parent(
                            &mut managed_parents,
                            child.path.clone(),
                            *child_repo,
                        );
                        if let Some(gc) = child_output.response.gc() {
                            child_gc_reports.insert(child.path.clone(), gc.clone());
                        }
                        child.status = "indexed".to_owned()
                    }
                    Ok(BoundChildIndex::Output(child_output, child_repo)) => {
                        cache_managed_child_parent(
                            &mut managed_parents,
                            child.path.clone(),
                            *child_repo,
                        );
                        if let Some(gc) = child_output.response.gc() {
                            child_gc_reports.insert(child.path.clone(), gc.clone());
                        }
                        let child_completion = child_output.completion;
                        child_completions.push(child_completion);
                        child.status = "indexed_partial".to_owned();
                        child.error_code = child_output
                            .response
                            .error_code()
                            .map(str::to_owned)
                            .or_else(|| {
                                Some(
                                    match child_completion {
                                        Completion::AuthRequired => "KIO-E-ADAPTER-AUTH-001",
                                        Completion::BudgetBlocked => "KIO-E-BUDGET-EXCEEDED-001",
                                        Completion::Partial => "KIO-E-SEARCH-PARTIAL-001",
                                        Completion::Permanent => {
                                            "KIO-E-SEARCH-SCOPE-ALL-FAILED-001"
                                        }
                                        Completion::Complete => {
                                            unreachable!("guarded child must be noncomplete")
                                        }
                                    }
                                    .to_owned(),
                                )
                            });
                        child.message =
                            child_output
                                .response
                                .message()
                                .map(str::to_owned)
                                .or_else(|| {
                                    Some("child index completed with a non-success exit".to_owned())
                                });
                    }
                    Err(error) => {
                        incomplete_child_discovery = true;
                        page_completed = false;
                        match error.exit_code() {
                            ExitCode::AuthError => child_completions.push(Completion::AuthRequired),
                            ExitCode::BudgetExceeded => {
                                child_completions.push(Completion::BudgetBlocked)
                            }
                            ExitCode::PartialFailure => child_completions.push(Completion::Partial),
                            ExitCode::PermanentFailure => {
                                child_completions.push(Completion::Permanent)
                            }
                            _ => {}
                        }
                        child.status = "skipped_error".to_owned();
                        child.error_code = Some(error.error_code().to_owned());
                        child.message = Some(error.message().to_owned());
                    }
                }
                retain_child_discovery_row(&mut children, &mut child_rows_total, child);
            }
            // Advance only after every row in this bounded page completed. A
            // failure deliberately leaves the old cursor in place; replay is
            // idempotent and cannot lose a descendant after a crash or error.
            if !page_completed {
                incomplete_child_discovery = true;
                // The durable state still names this page, even when this
                // page would have been the final slice.
                break true;
            }
            frontier.commit(page.slice.continuation.as_ref())?;
            continuation_committed = true;
            if page.slice.continuation.is_none() {
                break false;
            }
            let next = discover_child_page(repo, Some(&frontier))?;
            if next.restarted_from_root {
                restart_count = restart_count.saturating_add(1);
                if restart_count > 1 {
                    incomplete_child_discovery = true;
                    break true;
                }
                managed_parents.clear();
            }
            page = next;
        };
        let child_rows_omitted = child_rows_total.saturating_sub(children.len() as u64);
        output.child_scopes = Some(
            children
                .into_iter()
                .map(|discovery| {
                    let gc = child_gc_reports.get(&discovery.path).cloned();
                    ChildPublishedRow { discovery, gc }
                })
                .collect(),
        );
        output.child_scope_discovery = Some(PublishedChildDiscovery {
            page_size: CHILD_SCOPE_DISCOVERY_PAGE_SIZE,
            has_more,
            continuation_committed,
            restarted_from_root: restart_count > 0,
            rows_total: child_rows_total,
            rows_omitted: child_rows_omitted,
        });
        completion = merged_index_completion(
            parent_completion,
            child_completions,
            incomplete_child_discovery,
        );
        if completion != Completion::Complete && output.error_code.is_none() {
            output.error_code = Some(
                match completion {
                    Completion::AuthRequired => "KIO-E-ADAPTER-AUTH-001",
                    Completion::BudgetBlocked => "KIO-E-BUDGET-EXCEEDED-001",
                    _ => "KIO-E-INDEX-PARTIAL-001",
                }
                .to_owned(),
            );
        }
    }
    let successful_publication = completion == Completion::Complete;
    if !successful_publication && recovered_before_publication {
        output.gc = Some(GcReport::PreflightRecovered(GcPreflightRecovered {
            status: "completed".to_owned(),
            trigger: "preflight_recovery".to_owned(),
            mode: "after_index".to_owned(),
            recovered_before_publication: true,
        }));
    }
    // Automatic GC acquires its own descriptor-bound lock after publication.
    if successful_publication {
        let gc_binding = gc_binding.expect("successful index has automatic GC preflight binding");
        let attachment =
            gc::attach_after_success(repo, "index", recovered_before_publication, gc_binding);
        output.gc = Some(attachment.report);
        if let Some(status) = attachment.publication_status {
            output.publication_status = Some(status);
        }
        if let Some(code) = attachment.error_code {
            output.error_code = Some(code);
        }
        if let Some(override_completion) = attachment.completion_override {
            completion = override_completion;
        }
    }
    Ok(IndexExecution {
        response: IndexResponse::Published(Box::new(output)),
        completion,
    })
}

/// Build a preview through retained management and live ancestor policy.
/// Public paths in the request are presentation-only and never select I/O.
fn build_repository_scan_preview(
    adapter_settings: &Arc<AdapterRuntimeSettings>,
    repo: &Repository,
    request: ScanPreviewRequest,
) -> kio_pipeline::Result<ScanPreview> {
    let binding = binding_for_repo(repo).map_err(|error| {
        kio_pipeline::PipelineError::contract("KIO-E-POLICY-MANAGEMENT-001", error.to_string())
    })?;
    build_managed_scan_preview(
        &adapter_settings.declared_pricing("markdown"),
        &adapter_settings.declared_pricing("embedding"),
        &binding,
        request,
    )
}

pub(crate) fn read_repository_scan_input(
    repo: &Repository,
    input_path: &str,
    max_bytes: u64,
) -> kio_pipeline::Result<kio_pipeline::scan::VerifiedScanInput> {
    let root = repo.bound_root_handle().ok_or_else(|| {
        kio_pipeline::PipelineError::contract(
            "KIO-E-SCOPE-BOUND-STATE-001",
            "managed index is missing its retained scope handle",
        )
    })?;
    read_bound_verified_scan_input(root, input_path, max_bytes)
}

pub(crate) fn repository_scan_policy_allows_file(
    repo: &Repository,
    input_path: &str,
) -> kio_pipeline::Result<bool> {
    let binding = binding_for_repo(repo).map_err(|error| {
        kio_pipeline::PipelineError::contract("KIO-E-POLICY-MANAGEMENT-001", error.to_string())
    })?;
    managed_scan_policy_allows_file(&binding, input_path)
}

/// A stable presentation-only source path. Bound children use the retained
/// descriptor for all I/O; this is never dereferenced after binding.
pub(crate) fn repository_input_display_path(repo: &Repository, input_path: &str) -> PathBuf {
    repo.canonical_root().join(input_path)
}

/// Execute one planned child against the retained discovery handle.  Its
/// inherited parent rules are kept only in the invocation-local policy stack.
enum BoundChildIndex {
    SkippedVcs,
    ExcludedIndependentRoot,
    ExcludedByPolicy,
    Output(Box<IndexExecution>, Box<Repository>),
}

/// Keep CLI output bounded even when one invocation reconciles many pages.
/// The aggregate fields report every processed row; retained rows are a
/// presentation sample only and never influence frontier advancement.
fn retain_child_discovery_row(
    rows: &mut Vec<kio_pipeline::scan::ChildScopeDiscovery>,
    total: &mut u64,
    row: kio_pipeline::scan::ChildScopeDiscovery,
) {
    *total = total.saturating_add(1);
    if rows.len() < CHILD_SCOPE_DISCOVERY_PAGE_SIZE {
        rows.push(row);
    }
}

/// Reopen only a parent that a prior page has already enrolled, proving every
/// physical step still belongs to this invocation's management root.  The
/// continuation itself carries no handles and pages need not end at a depth
/// boundary, so the current page cannot rely solely on its in-memory map.
enum ManagedChildAncestor {
    Managed(Box<Repository>),
    ExcludedByPolicy,
    IndependentRoot,
}

fn reopen_managed_child_ancestor(
    repo: &Repository,
    relative: &str,
) -> Result<ManagedChildAncestor> {
    let root_record = kio_core::management::read_record(&binding_for_repo(repo)?)?;
    let root_scope_id = match &root_record.authority {
        kio_core::management::ManagementAuthority::Root => root_record.scope_id.clone(),
        kio_core::management::ManagementAuthority::Child { root_scope_id, .. } => {
            root_scope_id.clone()
        }
    };
    let mut current_scope_id = root_record.scope_id;
    let mut path = PathBuf::new();
    let mut current: Option<Repository> = None;
    for component in Path::new(relative).components() {
        let std::path::Component::Normal(name) = component else {
            return Err(KioError::invalid_usage(
                "child discovery ancestor path is not scope-relative",
            ));
        };
        // A page can have been planned before an immediate parent's Ignore
        // was loaded during enrollment. Check each live parent before opening
        // the next .kio: excluded ancestors intentionally have no management
        // directory and must not strand the durable discovery frontier.
        let parent = current.as_ref().unwrap_or(repo);
        let parent_binding = binding_for_repo(parent)?;
        let parent_record = kio_core::management::read_record(&parent_binding)?;
        let policy = kio_pipeline::policy::CurrentPolicyEvaluator::load(
            &parent_binding,
            parent_record.case_insensitive,
        )
        .map_err(pipeline_to_kio)?;
        let name_text = name
            .to_str()
            .ok_or_else(|| KioError::invalid_usage("child discovery ancestor is not UTF-8"))?;
        if !policy
            .allows_directory(name_text)
            .map_err(pipeline_to_kio)?
        {
            return Ok(ManagedChildAncestor::ExcludedByPolicy);
        }
        policy.revalidate().map_err(pipeline_to_kio)?;
        path.push(name);
        let next = open_existing_managed_root(&repo.canonical_root().join(&path), repo.control())?
            .in_lock_session(repo.lock_session());
        let record = kio_core::management::read_record(&binding_for_repo(&next)?)?;
        match &record.authority {
            kio_core::management::ManagementAuthority::Root => {
                return Ok(ManagedChildAncestor::IndependentRoot);
            }
            kio_core::management::ManagementAuthority::Child {
                parent_scope_id,
                root_scope_id: recorded_root_scope_id,
                ..
            } if parent_scope_id == &current_scope_id
                && recorded_root_scope_id == &root_scope_id => {}
            _ => {
                return Err(KioError::invalid_usage(
                    "child discovery ancestor is not managed by this root",
                ));
            }
        }
        current_scope_id = record.scope_id;
        current = Some(next);
    }
    current
        .map(|ancestor| ManagedChildAncestor::Managed(Box::new(ancestor)))
        .ok_or_else(|| KioError::invalid_usage("child discovery ancestor path is empty"))
}

/// Repositories retain several directory descriptors. Bound the discovery
/// cache independently of the number of scopes; evicted parents are reopened
/// through `reopen_managed_child_ancestor`, including its current-authority
/// checks, when their descendants are encountered.
fn cache_managed_child_parent(
    parents: &mut BTreeMap<String, Repository>,
    path: String,
    repository: Repository,
) {
    const MAX_RETAINED_CHILD_PARENTS: usize = 32;
    if !parents.contains_key(&path) && parents.len() >= MAX_RETAINED_CHILD_PARENTS {
        parents.pop_first();
    }
    parents.insert(path, repository);
}

fn run_bound_child_index(
    ctx: &Invocation<'_>,
    parent: &Repository,
    plan: &kio_pipeline::scan::ChildScopePlan,
    relative: &str,
    args: &IndexArgs,
) -> Result<BoundChildIndex> {
    let Some(bound) = bind_planned_child(plan, relative).map_err(pipeline_to_kio)? else {
        return Ok(BoundChildIndex::SkippedVcs);
    };
    match reconcile_planned_child(parent, bound)? {
        ChildScope::ExcludedIndependentRoot => Ok(BoundChildIndex::ExcludedIndependentRoot),
        ChildScope::ExcludedByPolicy => Ok(BoundChildIndex::ExcludedByPolicy),
        ChildScope::Managed { repo, .. } => {
            let mut child_args = args.clone();
            // Root confirmation never creates descendant network or secret
            // grants. Existing child grants may still authorize the requested
            // execution lane through that child's ordinary send gates.
            child_args.yes = false;
            let output = run_index_for_repo(ctx, child_args, &repo, false)?;
            Ok(BoundChildIndex::Output(Box::new(output), repo))
        }
    }
}

/// Aggregate `index_status` (05 §1.7, CT3-OBS-001) over the searched scopes.
/// `enriched_ratio` = done AI-enrichment tasks / all AI-enrichment tasks pooled
/// across scopes (count-weighted average). Enrichment tasks are Markdownize +
/// Embedding; "done" is `Done`/`Partial`. `budget_paused` is a budget-paused task or
/// an exhausted monthly budget (R22-7 — a secrets hold pauses work but not for budget).
pub(crate) fn compute_index_status(
    searched: &[SearchedScopeInfo],
    budget_ledger: Option<&LedgerReadSnapshot>,
    ledger_path: &Path,
    budget_month: &str,
    control: &ControlHandle,
    locks: &kio_core::scope::LockSession,
) -> Result<IndexStatusResponse> {
    let mut total = 0u64;
    let mut done = 0u64;
    let mut pending = 0u64;
    let mut budget_paused = false;
    let mut unsupported_inputs = Vec::new();
    let mut unsupported_input_errors = Vec::new();
    let mut task_errors = Vec::new();
    let mut office_conversion_unavailable_inputs = Vec::new();
    let device_spent = match budget_ledger {
        Some(snapshot) => snapshot
            .month_total(None, None, budget_month)
            .map_err(|error| {
                pipeline_to_kio(kio_pipeline::PipelineError::ledger_snapshot(
                    ledger_path.display().to_string(),
                    error,
                ))
            })?,
        None => 0.0,
    };

    for scope in searched {
        // R23-20 (03 §4 L296): `scope.scope_path` is now the canonical `.kio`
        // directory itself (was the parent repo root before the fix, hence
        // the `.join(".kio")` this used to need here).
        let store = TaskStore::new_with_control(&scope.scope_path, control.clone());
        let tasks = match store.all() {
            Ok(tasks) => tasks,
            Err(error) => {
                let error = pipeline_to_kio(error);
                task_errors.push(IndexStatusError {
                    scope_path: scope.scope_path.clone(),
                    error_code: error.error_code().to_owned(),
                    message: error.message().to_owned(),
                });
                Vec::new()
            }
        };
        for task in tasks {
            if !matches!(task.task_type, TaskType::Markdownize | TaskType::Embedding) {
                continue;
            }
            // R20-7: a `retired_non_live` task's chunk went non-live (deleted / re-chunked /
            // superseded), so it is NOT an enrichment gap in the CURRENT corpus and must not
            // deflate enriched_ratio. R11-8 deliberately keeps a LIVE permanent-gap Failed
            // (invalid_input/contract) in the denominator; this excludes only the non-live
            // subset, which R19-3's dedicated reason makes unambiguous. Without this, a
            // deleted/reverted chunk left enriched_ratio < 1.0 with pending == 0 forever.
            if task.fallback_reason.as_deref() == Some(RETIRED_NON_LIVE) {
                continue;
            }
            total += 1;
            match task.status {
                TaskStatus::Done => done += 1,
                // R9-4: a Partial task has Failed units still awaiting completion —
                // count it as incomplete (pending), not done. Counting it as done
                // made `index_status` report enriched_ratio = 1.0 with
                // pending_enrichment_tasks = 0 while units were permanently missing
                // (a silent data gap on text-layer-less scans). `batch retry` now
                // drives it to Done (docs/04 §5.2).
                TaskStatus::Partial | TaskStatus::Pending | TaskStatus::Running => pending += 1,
                // R22-7: only a BUDGET pause may set `budget_paused`. docs/05-runtime.md:200
                // has agents render this flag as "(budget により一時停止中)", so a Tier B
                // `secrets_tier_b_hold` — which costs nothing and is cleared by
                // `--send-secrets`, never by raising a cap — was steering them to
                // `--override-budget` while `device_remaining_usd` sat untouched at its full
                // cap. It is still outstanding work, so it stays in `pending`.
                TaskStatus::Paused => {
                    pending += 1;
                    if task.fallback_reason.as_deref() == Some("budget_exceeded") {
                        budget_paused = true;
                    }
                }
                // R11-8: a RETRYABLE Failed enrichment task (network_error/
                // quota_exceeded etc., holding next_retry_at, recoverable by
                // `batch retry`) is outstanding work — count it as pending.
                // (rate_limit no longer reaches this arm at all under QA3,
                // 04 §5.3: it stays Pending — already counted by the
                // `TaskStatus::Partial | TaskStatus::Pending | TaskStatus::Running`
                // arm above — never Failed.) Otherwise the scope reads
                // enriched_ratio<1.0 with pending_enrichment_tasks=0 and
                // budget_paused=false, an impossible-looking dead end an Agent
                // cannot act on (the dual of R9-4). A NON-retryable Failed task
                // (permanent gap) stays excluded: it surfaces only as
                // ratio<1.0, never as actionable pending work.
                TaskStatus::Failed if task_retry_allowed(&task) => pending += 1,
                TaskStatus::Failed => {}
            }
        }
        // R23-20: `scope.scope_path` is the `.kio` directory; `Repository::open`
        // wants the scope ROOT (it appends `.kio` itself), so this uses the
        // separately-carried `repo_root` field.
        match Repository::open_in_session(&scope.repo_root, control.clone(), locks) {
            Ok(repo) => {
                // 07 §5.1: a single shared read (`current_dispositions`) feeds
                // BOTH `unsupported_inputs` and the new `office_conversion_unavailable`
                // field -- they partition the SAME underlying `UnsupportedInputStore`
                // read/tree-liveness check by `reason`, so a single corrupt/
                // unreadable scope must report exactly ONE error here, not one
                // per output field (two independent re-reads of the identical
                // failing store would double-count it).
                match current_dispositions(&repo) {
                    Ok(dispositions) => {
                        let (office_unavailable, unsupported): (Vec<_>, Vec<_>) =
                            dispositions.into_iter().partition(|disposition| {
                                disposition.reason == OFFICE_CONVERSION_UNAVAILABLE_REASON
                            });
                        let unsupported: Vec<_> = unsupported
                            .into_iter()
                            .filter(|disposition| {
                                disposition.reason == UNSUPPORTED_REASON_UNRECOGNIZED_BINARY
                            })
                            .collect();
                        total = total.saturating_add(unsupported.len() as u64);
                        total = total.saturating_add(office_unavailable.len() as u64);
                        unsupported_inputs.extend(unsupported.into_iter().map(|disposition| {
                            FileDispositionResponse {
                                scope_path: scope.scope_path.clone(),
                                path: disposition.path,
                                raw_hash: disposition.raw_hash,
                                media_type: disposition.media_type,
                                size_bytes: disposition.size_bytes,
                                reason: disposition.reason,
                            }
                        }));
                        // 07 §5.1: an Office file with no resolvable converter has
                        // NO enrichment task at all (item 1's enqueue gate) -- it
                        // is exactly as "not enriched" as an `unsupported_inputs`
                        // entry, so it also counts toward `total` (denominator)
                        // without counting toward `done`. Kept in its OWN field
                        // (not folded into `unsupported_inputs`) because the
                        // reason and the recovery action (install a converter)
                        // differ.
                        office_conversion_unavailable_inputs.extend(
                            office_unavailable.into_iter().map(|disposition| {
                                FileDispositionResponse {
                                    scope_path: scope.scope_path.clone(),
                                    path: disposition.path,
                                    raw_hash: disposition.raw_hash,
                                    media_type: disposition.media_type,
                                    size_bytes: disposition.size_bytes,
                                    reason: disposition.reason,
                                }
                            }),
                        );
                    }
                    Err(error) => unsupported_input_errors.push(IndexStatusError {
                        scope_path: scope.scope_path.clone(),
                        error_code: error.error_code().to_owned(),
                        message: error.message().to_owned(),
                    }),
                }
                // Preserve the established best-effort treatment of unrelated
                // per-scope budget-config/identity failures: they must not
                // discard otherwise valid multi-scope search results. Snapshot
                // query failures remain command-fatal inside the successful
                // policy+identity branch and are never converted to zero.
                if let Ok(caps) = read_budget_policy(
                    user_config_toml_path(control),
                    repo.kio_dir().join("config.toml"),
                ) && let Ok(identity) = repo.scope_identity()
                {
                    let folder_spent = match budget_ledger {
                        Some(snapshot) => snapshot
                            .month_total(Some(&identity.scope_id), None, budget_month)
                            .map_err(|error| {
                                pipeline_to_kio(kio_pipeline::PipelineError::ledger_snapshot(
                                    ledger_path.display().to_string(),
                                    error,
                                ))
                            })?,
                        None => 0.0,
                    };
                    let device_exhausted = caps.device_monthly_usd_cap - device_spent <= 0.0;
                    let folder_exhausted = caps
                        .folder_monthly_usd_cap
                        .is_some_and(|cap| cap - folder_spent <= 0.0);
                    if device_exhausted || folder_exhausted {
                        budget_paused = true;
                    }
                }
            }
            Err(error) => {
                let error = IndexStatusError {
                    scope_path: scope.scope_path.clone(),
                    error_code: error.error_code().to_owned(),
                    message: error.message().to_owned(),
                };
                task_errors.push(error.clone());
                unsupported_input_errors.push(error);
            }
        }
    }

    let enriched_ratio = if total == 0 {
        1.0
    } else {
        done as f64 / total as f64
    };
    let unsupported_inputs_complete = unsupported_input_errors.is_empty();
    let tasks_complete = task_errors.is_empty();
    Ok(IndexStatusResponse {
        enriched_ratio,
        pending_enrichment_tasks: pending,
        budget_paused,
        unsupported_inputs,
        unsupported_input_errors,
        unsupported_inputs_complete,
        task_errors,
        tasks_complete,
        // 07 §5.1: Office (DOCX/PPTX) files skipped for online enrichment
        // because no converter (`KIO_OFFICE_CONVERTER` / PATH `soffice`)
        // resolved at `kio index` time -- no doomed task was enqueued for
        // them (item 1's enqueue gate). Disappears once a converter is
        // installed and the file is re-indexed (the disposition transitions
        // to `resolved`, dropping it from this "still live" view).
        office_conversion_unavailable: OfficeConversionUnavailableResponse {
            count: office_conversion_unavailable_inputs.len(),
            files: office_conversion_unavailable_inputs,
        },
    })
}

pub(crate) fn current_unsupported_inputs(
    repo: &Repository,
) -> Result<Vec<UnsupportedInputDisposition>> {
    Ok(current_dispositions(repo)?
        .into_iter()
        .filter(|disposition| disposition.reason == UNSUPPORTED_REASON_UNRECOGNIZED_BINARY)
        .collect())
}

/// Shared body for [`current_unsupported_inputs`] / `compute_index_status`'s
/// own combined read (which partitions by `reason` itself — including 07
/// §5.1's `OFFICE_CONVERSION_UNAVAILABLE_REASON` — in one pass, so a single
/// corrupt/unreadable scope reports exactly one error rather than one per
/// reason): every `UnsupportedInputStore` disposition (any reason) still LIVE
/// at HEAD with the SAME `raw_hash` the disposition recorded (a reverted /
/// deleted / re-hashed file's stale disposition must not keep surfacing).
fn current_dispositions(repo: &Repository) -> Result<Vec<UnsupportedInputDisposition>> {
    let store = UnsupportedInputStore::new(repo.kio_dir());
    let dispositions = store.latest_by_path().map_err(pipeline_to_kio)?;
    let Some(head) = repo.head_commit_hash()? else {
        return Ok(Vec::new());
    };
    let commit = repo.read_commit(&head)?;
    let tree = repo.read_tree(&commit.tree)?;
    let live = tree
        .entries
        .into_iter()
        .map(|entry| (entry.path, entry.raw_hash))
        .collect::<BTreeMap<_, _>>();
    Ok(dispositions
        .into_iter()
        .filter(|disposition| live.get(&disposition.path) == Some(&disposition.raw_hash))
        .collect())
}

pub(crate) fn record_unsupported_if_changed(
    store: &UnsupportedInputStore,
    latest_by_path: &mut BTreeMap<String, UnsupportedInputDisposition>,
    disposition: UnsupportedInputDisposition,
) -> Result<bool> {
    if latest_by_path.get(&disposition.path) == Some(&disposition) {
        return Ok(false);
    }
    store.record(&disposition).map_err(pipeline_to_kio)?;
    latest_by_path.insert(disposition.path.clone(), disposition);
    Ok(true)
}

pub(crate) fn run_reindex(ctx: &Invocation<'_>, args: ReindexArgs) -> Result<CommandResponse> {
    let execution = run_reindex_response(ctx, args)?;
    Ok(CommandResponse {
        body: ResponseBody::Reindex(execution.response),
        completion: execution.completion,
    })
}

fn run_reindex_response(ctx: &Invocation<'_>, args: ReindexArgs) -> Result<ReindexExecution> {
    let adapter_settings = ctx.settings;

    // B: `--regenerate`/`--at` exclusivity and the flag vocabulary are enforced
    // by the clap declaration on `ReindexArgs`; this only shapes the struct the
    // command body already consumed.
    let parsed = historical_reindex::ParsedReindex {
        at: args.at.clone(),
        force: args.regenerate,
        yes: args.yes,
        online: args.online,
        offline: args.offline,
    };
    let lane_override = LaneOverride::new(args.realtime, args.batch);
    // Recovery-capable opening only binds and reads durable scope authority;
    // reject scope denial before any writer/recovery or derived-state mutation.
    let repo = ctx.open_recovery_repository()?;
    let publication_policy = admit_scope_writer(&repo)?;
    resolve_invocation_lane(
        lane_override,
        Some(&repo.kio_dir().join("config.toml")),
        ctx.control,
    )?;
    // M1(a): serialize both HEAD reindex and historical enrichment against
    // concurrent index/repair/reindex. Historical enrichment is derived-state
    // only, but still appends to the chunk ledger and SQLite projection.
    let _lock = repo.lock_store_in_session(repo.lock_session())?;
    publication_policy.revalidate().map_err(pipeline_to_kio)?;
    validate_repo_tool_lock(&repo)?;
    // CL45/item 5: reconcile any stale `request_kind='sync'` cost-ledger.sqlite
    // rows left by a crashed prior run — applies uniformly to both the
    // historical (`--at`) and HEAD (`--force`) reindex modes below.
    if !parsed.offline
        && let Some(ledger) = open_optional_ledger_db(ctx.control)?
    {
        recover_stale_sync_rows(
            &ledger,
            Some(&TaskStore::new_with_control(
                repo.kio_dir(),
                repo.control().clone(),
            )),
            &scope_id(repo.kio_dir())?,
        )?;
    }
    // 04 §5.8 相 3: poll/collect in-flight Batch markdownize jobs at the same
    // write-command entry point (see `poll_batch_markdownize_jobs`).
    if !parsed.offline
        && let Some(ledger) = open_optional_ledger_db(ctx.control)?
    {
        poll_batch_markdownize_jobs(
            adapter_settings,
            &repo,
            &TaskStore::new_with_control(repo.kio_dir(), repo.control().clone()),
            &ledger,
        )?;
        // 04 §5.8 相 3 for the embedding Batch lane at the same entry point.
        poll_batch_embedding_jobs(ctx, &repo, &ledger)?;
    }
    // LC39/LC40 (see `ensure_purge_epoch_initialized`'s doc comment).
    ensure_purge_epoch_initialized(repo.kio_dir(), repo.control())?;
    if let Some(at) = parsed.at.as_deref() {
        return historical_reindex::run(ctx, &repo, at, parsed.online, parsed.offline);
    }
    if !parsed.force {
        return Err(KioError::invalid_usage(
            "reindex requires --regenerate in Step 3",
        ));
    }
    if !parsed.yes {
        return Err(KioError::new(
            "KIO-E-CONFIRM-REJECTED-001",
            "reindex --regenerate requires confirmation; pass --yes in non-interactive mode",
            json!({}),
            ExitCode::ConfirmationRejected,
        ));
    }
    let head = repo
        .head_commit_hash()?
        .ok_or_else(|| KioError::not_found("HEAD"))?;
    // R15-4/R16-1: reindex re-normalizes the HEAD tree, so it needs the full commit
    // AND tree objects. If either is gone (shallow: GC'd / deleted / corrupt), fail
    // with a clear KIO-E-COMMIT-SHALLOW-001 + recovery guidance rather than a raw,
    // opaque KIO-E-STORE-NOT-FOUND-001. `read_head_tree_for_rebuild` folds both the
    // missing-commit and missing-tree cases (the same conversion the shared
    // `rebuild_step3_index` below applies, so reindex's two tree reads stay symmetric).
    let tree = read_head_tree_for_rebuild(&repo, &head)?;
    // Validate the complete selected identity set before copying any normalized
    // generation. A post-barrier reindex must not partially republish derived
    // state for entries encountered before the purged target.
    for entry in &tree.entries {
        ensure_raw_ingest_allowed(&repo, &entry.raw_hash)?;
    }
    let mut normalize_by_path = BTreeMap::new();
    let mut reindexed = 0u64;
    // R17-2: a single document's missing/corrupt normalized unit must not abort the
    // whole re-normalization — the same docs/10 §7.2 resilience R16-4 gave the shared
    // `rebuild_step3_index`, which this pre-rebuild copy loop never inherited (R16-4
    // shared only the *tree* read via `read_head_tree_for_rebuild`, not this per-unit
    // copy). A skippable failure keeps the document's PREVIOUS gen (so the snapshot
    // still points at its last-good normalized instance and search keeps serving that
    // gen's cached chunks) and is recorded; healthy documents still re-normalize.
    let mut reindex_skipped = Vec::<SkippedUnit>::new();
    for entry in &tree.entries {
        let Some(normalize) = &entry.normalize else {
            continue;
        };
        let new_gen = normalize.r#gen + 1;
        match copy_normalized_instance_gen(
            repo.kio_dir(),
            &entry.raw_hash,
            &normalize.tool_profile_hash,
            normalize.r#gen,
            new_gen,
            ctx.control,
        ) {
            Ok(()) => {
                // PB04: the copied instance's manifest.json now declares
                // `gen: new_gen` (different bytes than the source gen's
                // manifest), so its manifest_hash must be recomputed, not
                // carried forward.
                let manifest_hash = compute_manifest_hash(
                    &repo,
                    &entry.raw_hash,
                    &normalize.tool_profile_hash,
                    new_gen,
                )?;
                normalize_by_path.insert(
                    entry.path.clone(),
                    PendingNormalizeRef {
                        expected_raw_hash: entry.raw_hash.clone(),
                        normalize: NormalizeRef {
                            tool_profile_hash: normalize.tool_profile_hash.clone(),
                            r#gen: new_gen,
                            manifest_hash,
                        },
                    },
                );
                reindexed += 1;
            }
            // STORE-IO (missing unit) / STORE-CORRUPT (malformed manifest/unit) are
            // localized to one document; keep the previous gen and continue. Any other
            // class is not a localized unit fault and still aborts (mirrors R16-4).
            Err(error) if is_rebuild_skippable_unit_error(&error) => {
                normalize_by_path.insert(
                    entry.path.clone(),
                    PendingNormalizeRef {
                        expected_raw_hash: entry.raw_hash.clone(),
                        normalize: NormalizeRef {
                            tool_profile_hash: normalize.tool_profile_hash.clone(),
                            r#gen: normalize.r#gen,
                            // PB04: gen is unchanged (copy failed, previous
                            // gen retained) — carry the existing manifest_hash
                            // forward rather than recompute.
                            manifest_hash: normalize.manifest_hash.clone(),
                        },
                    },
                );
                reindex_skipped.push(SkippedUnit {
                    raw_hash: entry.raw_hash.clone(),
                    path: entry.path.clone(),
                    r#gen: normalize.r#gen,
                    reason: error.error_code().to_owned(),
                });
            }
            Err(error) => return Err(error),
        }
    }
    let closing_preview = build_scan_preview(
        &adapter_settings.declared_pricing("markdown"),
        &adapter_settings.declared_pricing("embedding"),
        ScanPreviewRequest {
            scope_path: repo.root().display().to_string(),
            include_raw_hashes: false,
            require_network_approval: false,
        },
    )
    .map_err(pipeline_to_kio)?;
    let excluded = closing_preview
        .candidates
        .iter()
        .filter(|candidate| candidate.ignored)
        .map(|candidate| candidate.input_path.clone())
        .collect::<BTreeSet<_>>();
    let explicitly_allowed_tier_a = explicitly_allowed_tier_a_paths(&closing_preview);
    let outcome = repo.auto_snapshot_with_bound_normalize(
        Some("kio reindex --force"),
        None,
        &excluded,
        &normalize_by_path,
        &explicitly_allowed_tier_a,
        Some(&|| publication_policy.revalidate().map_err(pipeline_to_kio)),
    )?;
    if let Some(commit_hash) = &outcome.commit_hash {
        maybe_inject_replica_after_head_fault("reindex_before_marker", ctx.control)?;
        // `--regenerate` has the same HEAD-before-rebuild window as `kio index`.
        // The old replica must fail closed until the replacement source index
        // and its full projection have been published.
        mark_replica_rebuilding_or_log(ctx, repo.kio_dir(), commit_hash);
        maybe_inject_replica_after_head_fault("reindex", ctx.control)?;
    }
    let mut report = rebuild_step3_index(ctx, &repo)?;
    // LC42-LC44 (item 2), same ordering rationale as `run_index`'s call.
    recover_index_generation(repo.kio_dir(), repo.control())?;
    // R17-2: fold the re-normalization loop's skips into the rebuild report so the one
    // `skipped_disclosure` disclosure below covers both phases. Dedup by raw_hash: a
    // skipped document whose kept previous gen is ALSO unloadable is already reported by
    // the rebuild above; this appends only reindex-only skips (e.g. a corrupt non-`Done`
    // unit whose healthy `Done` units still let the rebuild serve the document).
    historical_reindex::merge_reindex_skips(&mut report, reindex_skipped);
    // L1: reindex = re-normalize + re-embedding (docs/06). The rebuild appends
    // fresh chunk rows; enrich them symmetrically with the `kio index` path so
    // the embedding index tracks the new generation. Online only under the
    // embedding adapter's opt-in; offline this enqueues Embedding tasks so
    // `index_status` reports the pending enrichment instead of falsely showing
    // enriched_ratio = 1.0 (the tasks would otherwise never be created).
    // QA31 (step4b-contract-tests-p3a.md §I): `--force`'s `--online`/
    // `--offline` now reach this pass instead of the hard-coded
    // `(false, false, false)` this used to pass unconditionally.
    let embedding_online = embedding_online_allowed(
        adapter_settings,
        &repo,
        parsed.offline,
        parsed.online,
        false,
    )?;
    // R11-2: keep the enrichment ExecOutcome (was discarded) so a new-generation
    // embedding auth/budget-pause is disclosed and raises the exit (result on stdout).
    let enrichment = run_embedding_enrichment(ctx, &repo, embedding_online, false, false)?;
    let response = HeadReindexResponse {
        status: "reindexed".to_owned(),
        reindexed_files: reindexed,
        commit_hash: outcome.commit_hash,
        rebuilt_chunks: report.rebuilt_chunks,
        embedding_tasks_executed: enrichment.executed,
        embedding_tasks_failed: enrichment.failed,
        paused_tasks: enrichment.paused,
        skipped: skipped_disclosure(&report, repo.kio_dir()),
    };
    let completion = enrichment_completion(&enrichment);
    Ok(ReindexExecution {
        response: ReindexResponse::Head(response),
        completion,
    })
}
